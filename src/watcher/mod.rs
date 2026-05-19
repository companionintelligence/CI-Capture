//! File-activity watcher pipeline.
//!
//! Connects the OS file-system observer ([`notify_glue`]) to the
//! coalescing + rename-inference state machine ([`normalizer`]) and
//! exposes two entry points:
//!
//! - [`run_dry_pipeline`] — observe + normalize + return a channel of
//!   normalized decisions. Used by `--watch <path> --dry-run` for
//!   on-device testing without a queue or server.
//! - [`run_full_pipeline`] — observe + normalize + fingerprint + enqueue
//!   into the SQLite queue. The sync engine drains separately.
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md).

pub mod fingerprint;
pub mod ignore;
pub mod normalizer;
pub mod notify_glue;
pub mod settle;
pub mod types;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::interval;
use uuid::Uuid;

use self::ignore::IgnoreRules;
use self::normalizer::{NormalizedDecision, Normalizer};
use self::notify_glue::{NotifyHandle, spawn_notify_watcher};
use self::types::{EventType, FileFingerprint, NormalizedEvent};
use crate::queue::rusqlite_impl::Queue;

/// Driver that owns the OS watcher + bridge thread. Drop to stop.
pub struct WatcherPipeline {
    _notify: NotifyHandle,
    _normalize_task: tokio::task::JoinHandle<()>,
}

/// Cadence at which we sweep the normalizer for events whose debounce
/// has expired.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Watch the configured roots and emit normalized decisions (no
/// fingerprint, no queue). The receiver yields one decision per event
/// the normalizer commits. Intended for `--watch ... --dry-run`.
pub fn run_dry_pipeline(
    roots: Vec<PathBuf>,
    ignore: IgnoreRules,
) -> Result<(WatcherPipeline, mpsc::UnboundedReceiver<NormalizedDecision>)> {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    let (dec_tx, dec_rx) = mpsc::unbounded_channel();

    let notify_handle = spawn_notify_watcher(&roots, ignore, raw_tx)?;

    let normalize_task = tokio::spawn(async move {
        let mut normalizer = Normalizer::default();
        let mut tick = interval(TICK_INTERVAL);
        loop {
            tokio::select! {
                obs = raw_rx.recv() => {
                    match obs {
                        Some(obs) => {
                            normalizer.observe(obs, Instant::now());
                            for d in normalizer.drain_immediate() {
                                if dec_tx.send(d).is_err() {
                                    return;
                                }
                            }
                        }
                        None => return,
                    }
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    for d in normalizer.drain_due(now) {
                        if dec_tx.send(d).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });

    Ok((
        WatcherPipeline {
            _notify: notify_handle,
            _normalize_task: normalize_task,
        },
        dec_rx,
    ))
}

/// Full pipeline: observe + normalize + fingerprint + enqueue. The sync
/// engine in [`crate::sync`] drains the queue separately.
pub fn run_full_pipeline(
    roots: Vec<PathBuf>,
    ignore: IgnoreRules,
    queue: Arc<Mutex<Queue>>,
) -> Result<WatcherPipeline> {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    let notify_handle = spawn_notify_watcher(&roots, ignore, raw_tx)?;

    let normalize_task = tokio::spawn(async move {
        let mut normalizer = Normalizer::default();
        let mut tick = interval(TICK_INTERVAL);
        loop {
            tokio::select! {
                obs = raw_rx.recv() => {
                    match obs {
                        Some(obs) => {
                            normalizer.observe(obs, Instant::now());
                            for d in normalizer.drain_immediate() {
                                enqueue_decision(&queue, d);
                            }
                        }
                        None => return,
                    }
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    for d in normalizer.drain_due(now) {
                        enqueue_decision(&queue, d);
                    }
                }
            }
        }
    });

    Ok(WatcherPipeline {
        _notify: notify_handle,
        _normalize_task: normalize_task,
    })
}

fn enqueue_decision(queue: &Arc<Mutex<Queue>>, decision: NormalizedDecision) {
    let event = decision_to_event(&decision);
    let q = queue.lock();
    if let Err(err) = q.enqueue(&event) {
        log::warn!("watcher: enqueue failed: {err}");
    }
}

fn decision_to_event(decision: &NormalizedDecision) -> NormalizedEvent {
    let now = Utc::now();
    let event_id = Uuid::new_v4().to_string();
    match decision {
        NormalizedDecision::Created { path } => NormalizedEvent {
            event_id,
            event_type: EventType::FileCreated,
            occurred_at: now,
            observed_at: now,
            file: fingerprint_or_meta(path),
            old_path: None,
            new_path: Some(path.clone()),
            correlation_id: None,
        },
        NormalizedDecision::Modified { path } => NormalizedEvent {
            event_id,
            event_type: EventType::FileModified,
            occurred_at: now,
            observed_at: now,
            file: fingerprint_or_meta(path),
            old_path: None,
            new_path: Some(path.clone()),
            correlation_id: None,
        },
        NormalizedDecision::Deleted { path } => NormalizedEvent {
            event_id,
            event_type: EventType::FileDeleted,
            occurred_at: now,
            observed_at: now,
            file: None,
            old_path: Some(path.clone()),
            new_path: None,
            correlation_id: None,
        },
        NormalizedDecision::Renamed { old_path, new_path } => NormalizedEvent {
            event_id,
            event_type: EventType::FileRenamed,
            occurred_at: now,
            observed_at: now,
            file: fingerprint_or_meta(new_path),
            old_path: Some(old_path.clone()),
            new_path: Some(new_path.clone()),
            correlation_id: None,
        },
    }
}

/// Best-effort fingerprint. If the file has already been deleted or is
/// inaccessible, returns `None` and the event carries no file payload —
/// the server can still record the timeline entry.
fn fingerprint_or_meta(path: &std::path::Path) -> Option<FileFingerprint> {
    match fingerprint::fingerprint(path) {
        Ok(fp) => Some(fp),
        Err(err) => {
            log::debug!("fingerprint failed for {}: {err}", path.display());
            None
        }
    }
}
