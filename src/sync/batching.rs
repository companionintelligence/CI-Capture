//! Adaptive batch sizing + exponential backoff.
//!
//! Pure logic, no I/O. The HTTP layer in [`super::http`] consults a
//! [`BatchController`] before each request and reports the outcome
//! through [`BatchController::record_success`] /
//! [`BatchController::record_failure`].
//!
//! Behaviour summary (see [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 4.2):
//!
//! - Start at [`INITIAL_BATCH`] (16). Grow by ×2 on every successful
//!   batch, capped at [`MAX_BATCH`] (256).
//! - On 5xx or transport error, halve the batch (min 1) and start an
//!   exponential backoff starting at [`BACKOFF_INITIAL`] (1 s), doubling
//!   each consecutive failure up to [`BACKOFF_MAX`] (5 min).
//! - Per-event retry: after [`DEAD_LETTER_AFTER`] (24 h) on a single
//!   event, the queue moves it to a dead-letter table (handled inside
//!   the queue layer, not here).
//! - On a fresh success the backoff resets to zero.

use std::time::Duration;

pub const INITIAL_BATCH: usize = 16;
pub const MIN_BATCH: usize = 1;
pub const MAX_BATCH: usize = 256;

pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// After this much time on the same event, the queue moves it to dead-letter.
pub const DEAD_LETTER_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Outcome of one batch attempt as reported by the HTTP layer.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    /// Batch succeeded. Server returned 2xx for all events.
    Success,
    /// Batch failed in a way that retrying could succeed (5xx, timeout,
    /// transport error). The batch size shrinks and backoff applies.
    TransientFailure,
    /// 4xx — the batch is malformed in a way retrying won't fix. Batch
    /// size shrinks but backoff does NOT apply (these should surface as
    /// dead-letter, not as exponential retries).
    PermanentFailure,
}

/// Controls batch size + delay between batches. One instance per sync
/// engine; not thread-safe (the sync engine owns it).
#[derive(Debug, Clone)]
pub struct BatchController {
    next_batch: usize,
    consecutive_failures: u32,
    backoff: Duration,
}

impl Default for BatchController {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchController {
    pub fn new() -> Self {
        Self {
            next_batch: INITIAL_BATCH,
            consecutive_failures: 0,
            backoff: Duration::ZERO,
        }
    }

    /// Size of the next batch the sync engine should ask for.
    pub fn batch_size(&self) -> usize {
        self.next_batch
    }

    /// How long to sleep before the next batch. Zero when no backoff is
    /// in effect.
    pub fn backoff(&self) -> Duration {
        self.backoff
    }

    /// Whether the engine is currently inside the backoff window (just a
    /// convenience for status surfaces).
    pub fn is_backing_off(&self) -> bool {
        self.backoff > Duration::ZERO
    }

    pub fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Success => self.on_success(),
            Outcome::TransientFailure => self.on_transient_failure(),
            Outcome::PermanentFailure => self.on_permanent_failure(),
        }
    }

    pub fn record_success(&mut self) {
        self.on_success();
    }

    pub fn record_failure(&mut self) {
        self.on_transient_failure();
    }

    fn on_success(&mut self) {
        self.consecutive_failures = 0;
        self.backoff = Duration::ZERO;
        self.next_batch = (self.next_batch.saturating_mul(2)).min(MAX_BATCH);
    }

    fn on_transient_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.backoff = next_backoff(self.backoff);
        self.next_batch = (self.next_batch / 2).max(MIN_BATCH);
    }

    fn on_permanent_failure(&mut self) {
        // Don't apply backoff — a malformed batch isn't a transient
        // condition, and burning a backoff timer on it would delay
        // legitimate events behind it.
        self.next_batch = (self.next_batch / 2).max(MIN_BATCH);
    }
}

/// Compute the next backoff value: double the previous, bounded by
/// [`BACKOFF_INITIAL`] and [`BACKOFF_MAX`].
fn next_backoff(prev: Duration) -> Duration {
    if prev.is_zero() {
        return BACKOFF_INITIAL;
    }
    let doubled = prev.saturating_mul(2);
    doubled.min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_controller_starts_at_initial_batch() {
        let c = BatchController::new();
        assert_eq!(c.batch_size(), INITIAL_BATCH);
        assert_eq!(c.backoff(), Duration::ZERO);
        assert!(!c.is_backing_off());
    }

    #[test]
    fn success_doubles_batch_up_to_cap() {
        let mut c = BatchController::new();
        let mut sizes = Vec::new();
        for _ in 0..10 {
            c.record_success();
            sizes.push(c.batch_size());
        }
        // First step doubles from 16 → 32 → 64 → 128 → 256.
        assert_eq!(sizes[0], 32);
        assert_eq!(sizes[1], 64);
        assert_eq!(sizes[2], 128);
        assert_eq!(sizes[3], 256);
        // And then caps.
        for s in &sizes[4..] {
            assert_eq!(*s, MAX_BATCH);
        }
    }

    #[test]
    fn success_clears_backoff() {
        let mut c = BatchController::new();
        c.record_failure();
        assert!(c.is_backing_off());
        c.record_success();
        assert_eq!(c.backoff(), Duration::ZERO);
        assert!(!c.is_backing_off());
    }

    #[test]
    fn transient_failure_halves_batch_and_backs_off() {
        let mut c = BatchController::new();
        // Grow first so we can see the halving.
        c.record_success(); // 32
        c.record_success(); // 64
        assert_eq!(c.batch_size(), 64);

        c.record_failure();
        assert_eq!(c.batch_size(), 32);
        assert_eq!(c.backoff(), BACKOFF_INITIAL);

        c.record_failure();
        assert_eq!(c.batch_size(), 16);
        assert_eq!(c.backoff(), Duration::from_secs(2));

        c.record_failure();
        assert_eq!(c.batch_size(), 8);
        assert_eq!(c.backoff(), Duration::from_secs(4));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut c = BatchController::new();
        // 10 failures: 1, 2, 4, 8, 16, 32, 64, 128, 256, 300 (capped).
        let expected = [1u64, 2, 4, 8, 16, 32, 64, 128, 256, 300];
        for (i, want) in expected.iter().enumerate() {
            c.record_failure();
            let got = c.backoff().as_secs();
            assert_eq!(
                got, *want,
                "iter {i}: got {got}s want {want}s ({:?})",
                c.backoff()
            );
        }
        // Keep failing — stays at cap.
        for _ in 0..5 {
            c.record_failure();
            assert_eq!(c.backoff(), BACKOFF_MAX);
        }
    }

    #[test]
    fn batch_floor_is_one() {
        let mut c = BatchController::new();
        // Hammer it with failures until size collapses.
        for _ in 0..20 {
            c.record_failure();
        }
        assert!(c.batch_size() >= MIN_BATCH);
        assert_eq!(c.batch_size(), MIN_BATCH);
    }

    #[test]
    fn permanent_failure_halves_batch_but_no_backoff() {
        let mut c = BatchController::new();
        c.record_success(); // 32
        c.record_success(); // 64

        c.record(Outcome::PermanentFailure);
        assert_eq!(c.batch_size(), 32);
        // Crucially, no backoff.
        assert_eq!(c.backoff(), Duration::ZERO);
        assert!(!c.is_backing_off());

        // And a subsequent success resumes growth.
        c.record_success();
        assert_eq!(c.batch_size(), 64);
    }

    #[test]
    fn record_dispatches_to_outcome_branch() {
        let mut c = BatchController::new();
        c.record(Outcome::Success);
        assert_eq!(c.batch_size(), 32);

        c.record(Outcome::TransientFailure);
        assert_eq!(c.batch_size(), 16);
        assert_eq!(c.backoff(), BACKOFF_INITIAL);

        c.record(Outcome::Success);
        assert_eq!(c.backoff(), Duration::ZERO);
    }

    #[test]
    fn next_backoff_zero_returns_initial() {
        assert_eq!(next_backoff(Duration::ZERO), BACKOFF_INITIAL);
    }

    #[test]
    fn next_backoff_caps_at_max() {
        let huge = Duration::from_secs(1_000_000);
        assert_eq!(next_backoff(huge), BACKOFF_MAX);
    }
}
