//! File-activity watcher.
//!
//! **Skeleton — not yet wired into the binary.** See
//! [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md)
//! for the design, and `CHECKLIST.md` at the repo root for the sub-PRs that
//! flesh out each submodule.
//!
//! When complete, this module owns the second capture lane alongside the
//! existing screenshot + vizier lanes in [`crate::worker`]. It watches a
//! user-selected set of folders, normalizes OS-native file events into a
//! stable schema, fingerprints settled files, and enqueues activity events
//! for the sync engine.

#![allow(dead_code)]

pub mod fingerprint;
pub mod ignore;
pub mod normalizer;
pub mod notify_glue;
pub mod settle;
pub mod types;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedReceiver;

use self::types::NormalizedEvent;

/// Handle returned by [`start_watcher`]. Dropping it stops the watcher.
pub struct WatcherHandle {
    pub events: UnboundedReceiver<NormalizedEvent>,
    // shutdown sender, join handle, etc. — to be added in the wire-up PR.
}

/// Configuration passed in from `AppConfig`. Concrete shape TBD; see the plan
/// doc § "Settings UI."
pub struct WatcherConfig {
    pub watched_paths: Vec<std::path::PathBuf>,
    pub ignore_globs: Vec<String>,
}

/// Start a watcher across all configured roots. Returns a handle whose
/// `events` receiver yields normalized events as they're observed.
pub fn start_watcher(_cfg: WatcherConfig) -> Result<WatcherHandle> {
    todo!("implement in the watcher wire-up sub-PR; see CHECKLIST.md")
}
