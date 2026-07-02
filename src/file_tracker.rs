//! File-tracking lane: the third capture lane (after screenshots + vizier).
//!
//! Wires Phase 2 + Phase 3 into a working pipeline:
//!
//! ```text
//! watcher (notify → normalize → settle → fingerprint)
//!     → durable SQLite queue
//!         → extract changed file to Markdown (text family; rich formats opt-in)
//!             → POST to CI-Server via /api/v1/events
//! ```
//!
//! Design: a single Tokio task owns the [`Queue`] (a `rusqlite::Connection`
//! isn't `Sync`), draining the watcher's event stream into it and, on a
//! timer, shipping batches to CI-Server with adaptive sizing + exponential
//! backoff ([`BatchController`]). Events stay durably queued until the
//! server acknowledges them, so a network outage or a hard kill never loses
//! a tracked change.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::time::{Duration, interval};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::extractors::{self, traits::ExtractOptions};
use crate::queue::rusqlite_impl::Queue;
use crate::server_ingest::{self, FileEventContent};
use crate::sync::batching::BatchController;
use crate::watcher::types::{EventType, NormalizedEvent};
use crate::watcher::{self, WatcherConfig, ignore};

/// How often the lane attempts to drain the queue to CI-Server.
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// After this many failed delivery attempts, an event is dropped to the
/// dead-letter log instead of being retried forever. Without this, a single
/// poison event (one the server permanently rejects) would sit at the head
/// of the FIFO queue and block every newer event behind it.
const MAX_ATTEMPTS: u32 = 50;

/// Upper bound on wall-clock spent POSTing in one sync tick. Keeps a slow
/// server from monopolizing the lane's single task (which also drains the
/// watcher) for minutes at a time — leftover events stay queued and resume
/// next tick.
const TICK_BUDGET: Duration = Duration::from_secs(20);

/// True if the lane has everything it needs to run: enabled, an identity to
/// attribute events to, a place to send them, and at least one folder.
pub fn is_runnable(config: &AppConfig) -> bool {
    config.file_tracker.enabled
        && !config.file_tracker.watched_paths.is_empty()
        && server_ingest::is_configured(config)
}

/// Run the file-tracking lane until the watcher stops (or the task is
/// aborted on shutdown/reconfigure). Returns `Ok(())` when it exits cleanly.
pub async fn run(config: AppConfig, queue_path: PathBuf) -> Result<()> {
    let device_id = config.identity.device_id.unwrap_or_else(Uuid::nil);
    if config.file_tracker.watched_paths.is_empty() {
        log::info!("file tracker: no watched folders, lane idle");
        return Ok(());
    }

    // Stack the user's globs on top of the built-in defaults; the sensitive
    // list is always added inside the watcher.
    let mut ignore_globs: Vec<String> = ignore::DEFAULT_IGNORES
        .iter()
        .map(|s| s.to_string())
        .collect();
    ignore_globs.extend(config.file_tracker.ignore_globs.iter().cloned());

    let watcher_cfg = WatcherConfig {
        watched_paths: config.file_tracker.watched_paths.clone(),
        ignore_globs,
        max_file_bytes: config.file_tracker.max_file_bytes,
    };

    let mut handle = watcher::start_watcher(watcher_cfg).context("starting file watcher")?;
    let queue = Queue::open(&queue_path)
        .with_context(|| format!("opening file-event queue at {}", queue_path.display()))?;

    let session_id = Uuid::new_v4();
    let mut batch = BatchController::new();
    let mut backoff_until: Option<Instant> = None;
    let mut sync_tick = interval(SYNC_INTERVAL);

    log::info!(
        "file tracker running: {} folder(s), queue depth {}",
        config.file_tracker.watched_paths.len(),
        queue.depth().unwrap_or(0)
    );

    loop {
        tokio::select! {
            maybe = handle.events.recv() => {
                match maybe {
                    Some(event) => {
                        if let Err(err) = queue.enqueue(&event) {
                            log::error!("failed to enqueue file event: {err:?}");
                        } else {
                            log::debug!(
                                "queued {:?} for {}",
                                event.event_type,
                                describe(&event)
                            );
                        }
                    }
                    None => {
                        log::info!("file watcher stopped; file tracker exiting");
                        break;
                    }
                }
            }
            _ = sync_tick.tick() => {
                if let Some(until) = backoff_until
                    && Instant::now() < until
                {
                    continue; // still backing off
                }

                let want = batch.batch_size();
                let pending = match queue.take_batch(want) {
                    Ok(p) => p,
                    Err(err) => {
                        log::error!("failed to read queue batch: {err:?}");
                        continue;
                    }
                };
                if pending.is_empty() {
                    continue;
                }

                let tick_start = Instant::now();
                let mut failed = false;
                for queued in pending {
                    // Don't let one slow batch monopolize the task; the rest
                    // stay queued (still ready) and resume next tick.
                    if tick_start.elapsed() > TICK_BUDGET {
                        log::debug!("sync tick budget reached; deferring rest of batch");
                        break;
                    }

                    let content = build_content(&config, &queued.event).await;
                    match server_ingest::ingest_file_event(
                        &config,
                        device_id,
                        session_id,
                        &queued.event,
                        &content,
                    )
                    .await
                    {
                        Ok(()) => {
                            if let Err(err) = queue.ack(&queued.event_id) {
                                log::error!("failed to ack synced event: {err:?}");
                            }
                        }
                        Err(err) => {
                            let attempts = queued.attempts + 1;
                            if attempts >= MAX_ATTEMPTS {
                                // Dead-letter: drop the poison event so it
                                // can't block the queue forever. Keep going —
                                // this is a per-event problem, not an outage.
                                log::error!(
                                    "dead-letter: dropping {} after {attempts} attempts; last error: {err}",
                                    describe(&queued.event)
                                );
                                let _ = queue.ack(&queued.event_id);
                                continue;
                            }
                            failed = true;
                            let delay = batch.backoff().max(Duration::from_secs(1));
                            log::warn!("file event sync failed (attempt {attempts}): {err}");
                            let _ = queue.requeue(&queued.event_id, delay, &err.to_string());
                            break; // outage path: stop, back off, the rest stay queued
                        }
                    }
                }

                if failed {
                    batch.record_failure();
                    backoff_until = Some(Instant::now() + batch.backoff());
                } else {
                    batch.record_success();
                    backoff_until = None;
                }
            }
        }
    }

    Ok(())
}

/// Convert a changed file to Markdown (when content sync is enabled and the
/// format is supported). Deletes and unsupported formats yield empty
/// content — the event itself still ships so the timeline stays complete.
async fn build_content(config: &AppConfig, event: &NormalizedEvent) -> FileEventContent {
    if !config.file_tracker.send_content {
        return FileEventContent::default();
    }
    // Only created/modified/renamed carry a fingerprinted file to extract.
    if matches!(event.event_type, EventType::FileDeleted) {
        return FileEventContent::default();
    }
    let path = match event.file.as_ref().map(|f| f.path.clone()) {
        Some(p) => p,
        None => return FileEventContent::default(),
    };
    let max = config.file_tracker.max_content_bytes;

    let outcome = tokio::task::spawn_blocking(move || match extractors::classify(&path, None) {
        None => Extraction::Unsupported,
        Some(extractor) => match extractor.extract(
            &path,
            ExtractOptions {
                metadata_only: false,
                allow_heavy_models: false,
            },
        ) {
            Ok(doc) => Extraction::Ok {
                name: extractor.name().to_string(),
                markdown: doc.markdown,
                sections: doc.sections.len(),
            },
            Err(err) => Extraction::Failed(err.to_string()),
        },
    })
    .await
    .unwrap_or_else(|join_err| Extraction::Failed(format!("extraction task panicked: {join_err}")));

    match outcome {
        Extraction::Ok {
            name,
            markdown,
            sections,
        } => {
            let truncated = markdown.len() > max;
            let body = if truncated {
                truncate_on_char_boundary(&markdown, max)
            } else {
                markdown
            };
            FileEventContent {
                markdown: Some(body),
                section_count: sections,
                extractor: Some(name),
                content_truncated: truncated,
                extraction_error: None,
            }
        }
        // Unsupported format: no content, no error — a legitimate empty body.
        Extraction::Unsupported => FileEventContent::default(),
        // Supported format that failed: surface the error instead of shipping
        // a silently-empty body that looks identical to "unsupported".
        Extraction::Failed(err) => {
            log::debug!("content extraction failed: {err}");
            FileEventContent {
                extraction_error: Some(err),
                ..Default::default()
            }
        }
    }
}

/// Result of trying to extract a changed file's content.
enum Extraction {
    /// No extractor for this format — metadata-only, not an error.
    Unsupported,
    Ok {
        name: String,
        markdown: String,
        sections: usize,
    },
    /// A supported format whose extraction failed.
    Failed(String),
}

fn describe(event: &NormalizedEvent) -> String {
    event
        .new_path
        .as_ref()
        .or(event.old_path.as_ref())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 char.
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n\n…[truncated]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        // Multi-byte char ('é' is 2 bytes) straddling the cut point.
        let s = "aaaaé";
        let out = truncate_on_char_boundary(s, 5);
        assert!(out.starts_with("aaaa"));
        assert!(out.contains("truncated"));
        // We never produced invalid UTF-8 (this would have panicked).
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn short_string_is_untouched() {
        assert_eq!(truncate_on_char_boundary("hi", 100), "hi");
    }
}
