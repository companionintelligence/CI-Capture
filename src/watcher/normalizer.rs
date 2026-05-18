//! Event normalization: debounce, storm collapse, rename inference,
//! settle-detection orchestration.
//!
//! Skeleton — see [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 6.

#![allow(dead_code)]

use std::time::Duration;

/// Default debounce window per path. Lower bounded by typical editor save
/// sequences (a few hundred ms); upper bounded so a single save doesn't take
/// noticeable time to propagate.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(750);
pub const DEBOUNCE_MAX: Duration = Duration::from_secs(5);

/// If sustained for this long above this rate, we drop into "summary mode"
/// and emit per-directory events instead of per-file. See plan doc § 9.
pub const STORM_EVENTS_PER_SEC: u32 = 200;
pub const STORM_SUSTAIN: Duration = Duration::from_secs(5);
