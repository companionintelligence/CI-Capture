//! Settle detection — wait until a file stops being written before
//! fingerprinting it.
//!
//! Skeleton — see [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 6.

#![allow(dead_code)]

use std::time::Duration;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const MAX_WAIT: Duration = Duration::from_secs(10);
