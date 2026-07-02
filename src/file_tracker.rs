//! File-tracking lane: the third capture lane (after screenshots + vizier).
//!
//! Wires Phase 2 + Phase 3 into a working pipeline:
//!
//! ```text
//! watcher (notify → normalize → settle → fingerprint)
//!     → durable SQLite queue
//!         → extract changed file to Markdown (text family; rich formats opt-in)
//!             → POST to CI-Server via /v1/events (canonical; /api/v1/events fallback)
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
use chrono::Utc;
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

/// After failing continuously for this long, an event is *parked* in the
/// queue's persisted dead-letter table so it can't block the FIFO head
/// forever. Parking is a liveness valve, not a drop: transient failures
/// (outages, 5xx, expired auth) otherwise retry indefinitely, permanently
/// rejected events (4xx) park immediately, and parked events stay on disk.
const PARK_AFTER: Duration = crate::sync::batching::DEAD_LETTER_AFTER;

/// Upper bound on wall-clock spent POSTing in one sync tick. Keeps a slow
/// server from monopolizing the lane's single task (which also drains the
/// watcher) for minutes at a time — leftover events stay queued and resume
/// next tick.
const TICK_BUDGET: Duration = Duration::from_secs(20);

/// Cap on events parked per sync tick. Parking is meant for the rare poison
/// event; a *systemic* 4xx (server-side schema change rejecting everything)
/// must not flush the whole queue into dead_letters in seconds. Hitting the
/// cap ends the tick and backs off like an outage, giving the operator time
/// to notice before more events park.
const MAX_PARKS_PER_TICK: usize = 5;

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
        "file tracker running: {} folder(s), queue depth {}, {} parked dead-letter(s)",
        config.file_tracker.watched_paths.len(),
        queue.depth().unwrap_or(0),
        queue.dead_letter_count().unwrap_or(0)
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
                let mut parked = 0usize;
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
                        Err(err) if err.permanent => {
                            // The server rejected this payload — retrying the
                            // same bytes can never succeed. Park it (persisted,
                            // recoverable) and keep going: this is a per-event
                            // problem, not an outage, so no backoff — unless
                            // parks pile up this tick, which smells systemic.
                            log::error!(
                                "parking permanently rejected event {}: {err}",
                                describe(&queued.event)
                            );
                            if let Err(park_err) =
                                queue.dead_letter(&queued.event_id, &err.to_string())
                            {
                                log::error!("failed to park rejected event: {park_err:?}");
                            }
                            parked += 1;
                            if parked >= MAX_PARKS_PER_TICK {
                                log::error!(
                                    "{parked} events parked in one tick — treating as systemic rejection, backing off"
                                );
                                failed = true;
                                break;
                            }
                        }
                        Err(err) => {
                            // Transient (outage, 5xx, expired auth): retry
                            // forever with backoff — an offline weekend must
                            // not cost data. The one exception is liveness: an
                            // event that has been failing continuously for
                            // PARK_AFTER gets parked so it can't pin the FIFO
                            // head indefinitely.
                            let attempts = queued.attempts + 1;
                            let blocking_since = queued.first_failed_at.map(|t| Utc::now() - t);
                            if let Some(elapsed) = blocking_since
                                && elapsed.to_std().unwrap_or_default() >= PARK_AFTER
                            {
                                log::error!(
                                    "parking {} after {attempts} attempts over {}h (head-of-queue liveness): {err}",
                                    describe(&queued.event),
                                    elapsed.num_hours()
                                );
                                if let Err(park_err) =
                                    queue.dead_letter(&queued.event_id, &err.to_string())
                                {
                                    log::error!("failed to park stuck event: {park_err:?}");
                                }
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

                if failed || parked > 0 {
                    // Parks are server rejections too — a tick that parked
                    // must not read as success (which would grow the batch
                    // and reset backoff against a rejecting server).
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

/// End-to-end tests: the real lane (watcher → queue → extract → classify →
/// POST → ack/park) against a scripted in-process mock CI-Server. This is
/// the closest CI can get to the live-server round-trip without credentials.
#[cfg(test)]
mod e2e {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// One recorded request: (path, x-api-key header value, body).
    type Recorded = (String, Option<String>, String);

    /// Minimal HTTP/1.1 responder. Each incoming request pops the next
    /// scripted status (default 200). 200s answer with a valid
    /// `/v1/events` ingest body; 4xx/5xx answer with that status.
    fn spawn_mock_server(
        script: Vec<u16>,
    ) -> (String, Arc<Mutex<Vec<Recorded>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let script = Arc::new(Mutex::new(script));
        let rec = recorded.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();

                let mut content_length = 0usize;
                let mut api_key = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                    if let Some(v) = line
                        .split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("x-api-key"))
                        .map(|(_, v)| v.trim().to_string())
                    {
                        api_key = Some(v);
                    }
                }
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
                let body = String::from_utf8_lossy(&body).to_string();
                rec.lock().unwrap().push((path, api_key, body));

                let status = {
                    let mut s = script.lock().unwrap();
                    if s.is_empty() { 200 } else { s.remove(0) }
                };
                let response = if status == 200 {
                    let payload = r#"{"ingested":1,"duplicate":0,"rejected":[]}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                } else {
                    format!(
                        "HTTP/1.1 {status} X\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                    )
                };
                let _ = reader.into_inner().write_all(response.as_bytes());
            }
        });

        (origin, recorded)
    }

    fn lane_config(origin: &str, watched: PathBuf) -> AppConfig {
        let mut config = AppConfig::default();
        config.server.portal_url = origin.to_string();
        // 64 lowercase hex → classified as an x-api-key credential.
        config.server.applet_auth_token = Some("ab".repeat(32));
        config.file_tracker.watched_paths = vec![watched];
        config.file_tracker.send_content = true;
        config
    }

    async fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_edit_reaches_server_with_content_and_api_key_and_drains_queue() {
        let (origin, recorded) = spawn_mock_server(vec![]);
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("watched");
        std::fs::create_dir_all(&watched).unwrap();
        let queue_path = dir.path().join("queue.db");

        let config = lane_config(&origin, watched.clone());
        let lane = tokio::spawn(run(config, queue_path.clone()));

        // Give the watcher a beat to arm, then edit a file.
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::write(watched.join("notes.md"), "# Plan\n\nremember the tailscale rollout").unwrap();

        let got_event = wait_until(Duration::from_secs(30), || {
            recorded
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, body)| body.contains("file_event"))
        })
        .await;
        lane.abort();
        assert!(got_event, "file event never reached the mock server");

        let requests = recorded.lock().unwrap();
        let (path, api_key, body) = requests
            .iter()
            .find(|(_, _, b)| b.contains("file_event"))
            .unwrap();
        assert!(path.contains("/v1/events"), "unexpected ingest path {path}");
        assert_eq!(
            api_key.as_deref(),
            Some("ab".repeat(32).as_str()),
            "credential must ride x-api-key"
        );
        // The recall quick win: extracted content in the description field.
        assert!(
            body.contains("remember the tailscale rollout"),
            "description must carry extracted markdown"
        );

        // Ack drained the durable queue.
        let queue = Queue::open(&queue_path).unwrap();
        assert_eq!(queue.depth().unwrap(), 0, "acked event must leave the queue");
        assert_eq!(queue.dead_letter_count().unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejected_event_is_parked_while_outage_event_is_retried() {
        // Script: first POST → 400 (permanent), second → 500 (transient),
        // third → 200. One file edit per phase.
        let (origin, recorded) = spawn_mock_server(vec![400, 500, 200]);
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("watched");
        std::fs::create_dir_all(&watched).unwrap();
        let queue_path = dir.path().join("queue.db");

        let config = lane_config(&origin, watched.clone());
        let lane = tokio::spawn(run(config, queue_path.clone()));

        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::write(watched.join("poison.md"), "rejected payload").unwrap();

        // 400 → parked in dead_letters, not retried, not lost.
        let parked = wait_until(Duration::from_secs(30), || {
            Queue::open(&queue_path)
                .map(|q| q.dead_letter_count().unwrap_or(0) == 1)
                .unwrap_or(false)
        })
        .await;
        assert!(parked, "permanently rejected event was not parked");

        // Next edit: first attempt hits the scripted 500 (transient), the
        // retry hits 200 — the event must survive the outage and drain.
        std::fs::write(watched.join("survivor.md"), "survives the outage").unwrap();
        let drained = wait_until(Duration::from_secs(45), || {
            recorded
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, b)| b.contains("survives the outage"))
                && Queue::open(&queue_path).map(|q| q.depth().unwrap_or(99) == 0).unwrap_or(false)
        })
        .await;
        lane.abort();
        assert!(drained, "transient failure must retry to success, not drop");

        // The poison event is still recoverable on disk.
        let queue = Queue::open(&queue_path).unwrap();
        assert_eq!(queue.dead_letter_count().unwrap(), 1);
    }
}
