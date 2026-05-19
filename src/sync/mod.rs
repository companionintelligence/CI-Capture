//! Sync engine — drains the local event queue and delivers batches to
//! the configured destination.
//!
//! Two destinations:
//! - [`SyncSink::DryRun`] logs every delivered batch via `log::info!`
//!   and reports Success. Useful for on-device testing without a server.
//! - [`SyncSink::Http`] POSTs to CI-Server (`/api/history/file-events/batch`).
//!   Requires a paired bearer token.
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 5.

#![allow(dead_code)]

pub mod batching;
pub mod http;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::time::sleep;

use self::batching::{BatchController, Outcome};
use self::http::HttpSink;
use crate::queue::rusqlite_impl::Queue;
use crate::watcher::types::NormalizedEvent;

/// Where the sync engine sends batches.
pub enum SyncSink {
    /// Log every batch instead of sending. Default for the
    /// `--watch ... --dry-run` testing mode.
    DryRun,
    /// POST to CI-Server.
    Http(HttpSink),
}

impl SyncSink {
    pub async fn deliver(&self, events: &[NormalizedEvent]) -> Outcome {
        match self {
            SyncSink::DryRun => {
                if events.is_empty() {
                    return Outcome::Success;
                }
                log::info!("[dry-run] would deliver batch of {} event(s):", events.len());
                for e in events {
                    log::info!("  - {:?} {:?}", e.event_type, summarize(e));
                }
                Outcome::Success
            }
            SyncSink::Http(sink) => sink.deliver(events).await,
        }
    }
}

fn summarize(e: &NormalizedEvent) -> String {
    if let Some(new) = &e.new_path {
        if let Some(old) = &e.old_path {
            return format!("{} -> {}", old.display(), new.display());
        }
        return new.display().to_string();
    }
    if let Some(file) = &e.file {
        return file.path.display().to_string();
    }
    e.event_id.clone()
}

/// Run-loop control. The owner holds an `Arc<RunControl>` and flips
/// `stop` to true to signal the loop to exit.
#[derive(Default)]
pub struct RunControl {
    pub stop: parking_lot::Mutex<bool>,
}

impl RunControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: Mutex::new(false),
        })
    }

    pub fn signal_stop(self: &Arc<Self>) {
        *self.stop.lock() = true;
    }

    fn should_stop(&self) -> bool {
        *self.stop.lock()
    }
}

/// Main sync loop. Pulls due events from the queue in batches sized by
/// the [`BatchController`], delivers them via `sink`, and asks the
/// controller to update its policy based on the outcome.
///
/// The loop exits when `control.stop` is set OR when the queue is empty
/// for `idle_sleep` and the caller has not enqueued anything new.
pub async fn run_sync(
    queue: Arc<Mutex<Queue>>,
    sink: SyncSink,
    control: Arc<RunControl>,
    idle_sleep: Duration,
) -> Result<()> {
    let mut controller = BatchController::new();
    loop {
        if control.should_stop() {
            log::info!("sync loop: stop signalled");
            return Ok(());
        }

        // If the controller is in backoff, honour it first.
        let backoff = controller.backoff();
        if backoff > Duration::ZERO {
            sleep(backoff).await;
        }

        let batch_size = controller.batch_size();
        let queued = {
            let q = queue.lock();
            q.take_batch(batch_size)?
        };

        if queued.is_empty() {
            sleep(idle_sleep).await;
            continue;
        }

        let events: Vec<NormalizedEvent> = queued.iter().map(|qe| qe.event.clone()).collect();
        let outcome = sink.deliver(&events).await;

        match outcome {
            Outcome::Success => {
                let q = queue.lock();
                for qe in &queued {
                    let _ = q.ack(&qe.event_id);
                }
                controller.record(Outcome::Success);
                log::info!("sync: delivered + acked {} event(s)", queued.len());
            }
            Outcome::TransientFailure => {
                let q = queue.lock();
                for qe in &queued {
                    let _ = q.requeue(&qe.event_id, controller.backoff(), "transient failure");
                }
                controller.record(Outcome::TransientFailure);
                log::warn!(
                    "sync: transient failure on batch of {}, backing off {}s",
                    queued.len(),
                    controller.backoff().as_secs()
                );
            }
            Outcome::PermanentFailure => {
                let q = queue.lock();
                for qe in &queued {
                    // Push out by a minute so we don't tight-loop on a
                    // bad batch. The dead-letter scan in attempts_over
                    // is the real surface for these.
                    let _ = q.requeue(&qe.event_id, Duration::from_secs(60), "permanent failure");
                }
                controller.record(Outcome::PermanentFailure);
                log::error!("sync: permanent failure on batch of {}", queued.len());
            }
        }
    }
}
