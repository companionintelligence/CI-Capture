//! Adaptive batching: start small, grow on success, halve on 5xx.
//!
//! Skeleton — see [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 4.2.

#![allow(dead_code)]

use std::time::Duration;

pub const INITIAL_BATCH: usize = 16;
pub const MAX_BATCH: usize = 256;

pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// After this much time on the same event, move it to the dead-letter table.
pub const DEAD_LETTER_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
