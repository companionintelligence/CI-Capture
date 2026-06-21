//! File-activity watcher — the second capture lane alongside the existing
//! screenshot + vizier lanes in [`crate::worker`].
//!
//! It watches a user-selected set of folders, normalizes OS-native file
//! events into a stable schema ([`types::NormalizedEvent`]), waits for each
//! changed file to settle, fingerprints it (sha256 + size + mtime + inode),
//! and streams the events out on an unbounded channel for the file-tracker
//! lane to queue and ship to CI-Server.
//!
//! Pipeline: `notify` → [`notify_glue`] → [`normalizer`] → [`settle`] →
//! [`fingerprint`], gated throughout by [`ignore`].
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md).

#![allow(dead_code)]

pub mod fingerprint;
pub mod ignore;
pub mod normalizer;
pub mod notify_glue;
pub mod settle;
pub mod types;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::{Duration, interval};
use uuid::Uuid;

use self::fingerprint::fingerprint;
use self::ignore::IgnoreRules;
use self::normalizer::{NormalizedDecision, Normalizer};
use self::settle::{SettleStatus, wait_for_settle};
use self::types::{EventType, NormalizedEvent};

/// How often the driver drains debounce-expired decisions from the
/// normalizer. Tighter than the debounce window so events propagate
/// promptly once they settle.
const DRAIN_TICK: Duration = Duration::from_millis(250);

/// Max files fingerprinted concurrently. Hashing is I/O + CPU; a small
/// bound keeps a burst of edits from saturating the disk.
const MAX_CONCURRENT_FINGERPRINTS: usize = 4;

/// Handle returned by [`start_watcher`]. Owns the underlying OS watcher —
/// dropping it stops file-system notifications and lets the driver task
/// wind down.
pub struct WatcherHandle {
    /// Normalized events as they're observed and fingerprinted.
    pub events: UnboundedReceiver<NormalizedEvent>,
    /// Kept alive so the platform watcher keeps delivering events.
    _watcher: RecommendedWatcher,
}

/// Configuration for the watcher, projected from `AppConfig`.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub watched_paths: Vec<PathBuf>,
    pub ignore_globs: Vec<String>,
    pub max_file_bytes: u64,
}

/// Start a watcher across all configured roots. Must be called from within
/// a Tokio runtime — the driver runs as a spawned task.
///
/// Returns a handle whose `events` receiver yields normalized events as
/// they're observed. Paths that don't exist or can't be watched are logged
/// and skipped rather than failing the whole watcher.
pub fn start_watcher(cfg: WatcherConfig) -> Result<WatcherHandle> {
    let user_globs: Vec<&str> = cfg.ignore_globs.iter().map(String::as_str).collect();
    let ignore = IgnoreRules::from_patterns(&user_globs, cfg.max_file_bytes)
        .context("compiling ignore rules")?;

    let (raw_tx, mut raw_rx) = unbounded_channel::<notify::Event>();
    let (event_tx, event_rx) = unbounded_channel::<NormalizedEvent>();

    // The notify callback runs on notify's own thread; forward every event
    // into the async driver via the unbounded channel.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                let _ = raw_tx.send(event);
            }
            Err(err) => log::warn!("notify watcher error: {err}"),
        }
    })
    .context("creating filesystem watcher")?;

    let mut watched = 0usize;
    for path in &cfg.watched_paths {
        if !path.exists() {
            log::warn!("watched path does not exist, skipping: {}", path.display());
            continue;
        }
        match watcher.watch(path, RecursiveMode::Recursive) {
            Ok(()) => {
                watched += 1;
                log::info!("watching folder: {}", path.display());
            }
            Err(err) => log::error!("failed to watch {}: {err}", path.display()),
        }
    }
    log::info!("file watcher active on {watched} folder(s)");

    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_FINGERPRINTS));

    tokio::spawn(async move {
        let mut normalizer = Normalizer::default();
        let mut tick = interval(DRAIN_TICK);

        loop {
            tokio::select! {
                maybe = raw_rx.recv() => {
                    match maybe {
                        Some(event) => {
                            for obs in notify_glue::to_raw_observations(&event) {
                                if let Some(p) = obs_path(&obs) {
                                    if ignore.is_ignored(p) {
                                        if ignore.is_sensitive(p) {
                                            log::debug!("refusing sensitive path: {}", p.display());
                                        }
                                        continue;
                                    }
                                }
                                normalizer.observe(obs, Instant::now());
                            }
                        }
                        None => {
                            // Watcher dropped — flush and exit.
                            for decision in normalizer.flush() {
                                dispatch(decision, &ignore, &permits, &event_tx);
                            }
                            break;
                        }
                    }
                }
                _ = tick.tick() => {
                    for decision in normalizer.drain_due(Instant::now()) {
                        dispatch(decision, &ignore, &permits, &event_tx);
                    }
                }
            }
        }
        log::info!("file watcher driver stopped");
    });

    Ok(WatcherHandle {
        events: event_rx,
        _watcher: watcher,
    })
}

/// Turn a settled decision into a `NormalizedEvent` and send it. Fingerprint
/// work for create/modify happens on a spawned task so a slow settle never
/// stalls the driver loop.
fn dispatch(
    decision: NormalizedDecision,
    ignore: &IgnoreRules,
    permits: &Arc<Semaphore>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<NormalizedEvent>,
) {
    let ignore = ignore.clone();
    let permits = permits.clone();
    let event_tx = event_tx.clone();

    tokio::spawn(async move {
        let observed_at = Utc::now();
        let event = match decision {
            NormalizedDecision::Created { path } => {
                let file = settle_and_fingerprint(&path, &ignore, &permits).await;
                if file.is_none() {
                    // File vanished before it settled — treat as a delete.
                    delete_event(observed_at, path)
                } else {
                    NormalizedEvent {
                        event_id: Uuid::new_v4().to_string(),
                        event_type: EventType::FileCreated,
                        occurred_at: observed_at,
                        observed_at,
                        file,
                        old_path: None,
                        new_path: Some(path),
                        correlation_id: None,
                    }
                }
            }
            NormalizedDecision::Modified { path } => {
                let file = settle_and_fingerprint(&path, &ignore, &permits).await;
                if file.is_none() {
                    delete_event(observed_at, path)
                } else {
                    NormalizedEvent {
                        event_id: Uuid::new_v4().to_string(),
                        event_type: EventType::FileModified,
                        occurred_at: observed_at,
                        observed_at,
                        file,
                        old_path: None,
                        new_path: Some(path),
                        correlation_id: None,
                    }
                }
            }
            NormalizedDecision::Deleted { path } => delete_event(observed_at, path),
            NormalizedDecision::Renamed { old_path, new_path } => {
                let file = settle_and_fingerprint(&new_path, &ignore, &permits).await;
                NormalizedEvent {
                    event_id: Uuid::new_v4().to_string(),
                    event_type: EventType::FileRenamed,
                    occurred_at: observed_at,
                    observed_at,
                    file,
                    old_path: Some(old_path),
                    new_path: Some(new_path),
                    correlation_id: None,
                }
            }
        };
        let _ = event_tx.send(event);
    });
}

fn delete_event(observed_at: chrono::DateTime<Utc>, path: PathBuf) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Uuid::new_v4().to_string(),
        event_type: EventType::FileDeleted,
        occurred_at: observed_at,
        observed_at,
        file: None,
        old_path: None,
        new_path: Some(path),
        correlation_id: None,
    }
}

fn obs_path(obs: &normalizer::RawObservation) -> Option<&std::path::Path> {
    match obs {
        normalizer::RawObservation::Created { path, .. }
        | normalizer::RawObservation::Modified { path, .. }
        | normalizer::RawObservation::Deleted { path, .. } => Some(path),
        normalizer::RawObservation::Renamed { new_path, .. } => Some(new_path),
    }
}

/// Settle then fingerprint, honoring the size cap. Returns `None` when the
/// file vanished mid-settle or is too large to hash.
async fn settle_and_fingerprint(
    path: &std::path::Path,
    ignore: &IgnoreRules,
    permits: &Arc<Semaphore>,
) -> Option<types::FileFingerprint> {
    match wait_for_settle(path).await {
        Ok(SettleStatus::Deleted) => return None,
        Ok(_) => {}
        Err(err) => {
            log::debug!("settle failed for {}: {err}", path.display());
            return None;
        }
    }

    if let Ok(meta) = std::fs::metadata(path) {
        if ignore.exceeds_size_limit(meta.len()) {
            log::debug!(
                "skipping hash for oversized file ({} bytes): {}",
                meta.len(),
                path.display()
            );
            return None;
        }
    }

    let _permit = permits.acquire().await.ok()?;
    let path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || fingerprint(&path)).await {
        Ok(Ok(fp)) => Some(fp),
        Ok(Err(err)) => {
            log::debug!("fingerprint failed: {err}");
            None
        }
        Err(err) => {
            log::debug!("fingerprint task panicked: {err}");
            None
        }
    }
}
