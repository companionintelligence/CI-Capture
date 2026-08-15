//! Local screenshot store: retention + retry planning.
//!
//! The PNG saved by the capture lane *is* the durable queue — a frame on
//! disk is a frame the server hasn't acknowledged (successful ingest deletes
//! it). This module decides, for one sweep over `capture.storage_dir`:
//!
//! - **prune**: frames past the age cap or squeezed out by the total-size
//!   cap (oldest first). This bounds the plaintext screen-content footprint
//!   on disk even when no server is configured.
//! - **retry**: frames old enough that their original upload clearly failed
//!   or was dropped (capacity-full analysis channel, server outage, crash
//!   before ingest), capped per sweep so a big backlog drains politely.
//!
//! Pure planning only — the worker owns the filesystem and network I/O.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::config::CaptureConfig;

/// One stored screenshot, as scanned from `storage_dir`.
#[derive(Debug, Clone)]
pub struct StoredCapture {
    pub path: PathBuf,
    pub len_bytes: u64,
    pub modified: SystemTime,
}

/// What one sweep should do.
#[derive(Debug, Default)]
pub struct SweepPlan {
    /// Delete outright (retention).
    pub prune: Vec<PathBuf>,
    /// Re-attempt upload (oldest first); delete on ack like the fresh path.
    pub retry: Vec<PathBuf>,
}

/// A frame younger than this is presumed still in flight on the fresh
/// capture→analyze→upload path, so a routine sweep leaves it alone.
/// Reconnect sweeps ignore this and retry immediately.
pub const RETRY_MIN_AGE: Duration = Duration::from_secs(2 * 60);

/// Backlog uploads attempted per sweep. Keeps a week-long outage's backlog
/// from monopolizing the analysis loop when the server comes back.
pub const MAX_RETRIES_PER_SWEEP: usize = 8;

/// Decide what to prune and what to retry. `entries` may be in any order.
pub fn plan_sweep(entries: Vec<StoredCapture>, config: &CaptureConfig, now: SystemTime) -> SweepPlan {
    plan_sweep_ex(entries, config, now, false)
}

/// Same as [`plan_sweep`], but a reconnect drain retries every surviving
/// file immediately — there is no in-flight upload to protect.
pub fn plan_reconnect_sweep(
    entries: Vec<StoredCapture>,
    config: &CaptureConfig,
    now: SystemTime,
) -> SweepPlan {
    plan_sweep_ex(entries, config, now, true)
}

fn plan_sweep_ex(
    mut entries: Vec<StoredCapture>,
    config: &CaptureConfig,
    now: SystemTime,
    force_retry: bool,
) -> SweepPlan {
    // Oldest first — pruning and retrying both want chronological order.
    entries.sort_by_key(|e| e.modified);

    let mut plan = SweepPlan::default();

    // 1) Age cap.
    let max_age = match config.retention_max_age_days {
        0 => None,
        days => Some(Duration::from_secs(days * 24 * 60 * 60)),
    };
    let mut kept: Vec<StoredCapture> = Vec::with_capacity(entries.len());
    for entry in entries {
        let age = now.duration_since(entry.modified).unwrap_or_default();
        match max_age {
            Some(cap) if age > cap => plan.prune.push(entry.path),
            _ => kept.push(entry),
        }
    }

    // 2) Size cap: drop oldest until under budget.
    if config.retention_max_total_mb > 0 {
        let budget = config.retention_max_total_mb * 1024 * 1024;
        let mut total: u64 = kept.iter().map(|e| e.len_bytes).sum();
        let mut idx = 0;
        while total > budget && idx < kept.len() {
            total -= kept[idx].len_bytes;
            plan.prune.push(kept[idx].path.clone());
            idx += 1;
        }
        kept.drain(..idx);
    }

    // 3) Retry the survivors. Reconnect ignores the in-flight window.
    plan.retry = kept
        .into_iter()
        .filter(|e| {
            force_retry
                || now.duration_since(e.modified).unwrap_or_default() >= RETRY_MIN_AGE
        })
        .map(|e| e.path)
        .take(MAX_RETRIES_PER_SWEEP)
        .collect();

    plan
}

/// What is sitting in `storage_dir` right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenshotBacklog {
    pub count: usize,
    pub bytes: u64,
    pub oldest: Option<Duration>,
}

impl ScreenshotBacklog {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Old enough that the fresh capture→upload path should already have
    /// deleted the file. Younger frames are presumed still in flight.
    pub fn is_stuck(&self) -> bool {
        self.oldest
            .is_some_and(|age| age >= RETRY_MIN_AGE)
    }
}

/// Count and age the PNGs in `storage_dir`. Missing dir is an empty backlog.
pub fn summarize(storage_dir: &std::path::Path, now: SystemTime) -> ScreenshotBacklog {
    summarize_entries(scan(storage_dir), now)
}

/// Count and age unacked activity events.
pub fn summarize_activity(storage_dir: &std::path::Path, now: SystemTime) -> ScreenshotBacklog {
    summarize_entries(scan_activity(storage_dir), now)
}

fn summarize_entries(entries: Vec<StoredCapture>, now: SystemTime) -> ScreenshotBacklog {
    let count = entries.len();
    let bytes = entries.iter().map(|e| e.len_bytes).sum();
    let oldest = entries
        .iter()
        .filter_map(|e| now.duration_since(e.modified).ok())
        .max();
    ScreenshotBacklog {
        count,
        bytes,
        oldest,
    }
}

/// Scan `storage_dir` for stored captures (`capture_*.png`). Missing dir is
/// an empty store, not an error.
pub fn scan(storage_dir: &std::path::Path) -> Vec<StoredCapture> {
    scan_named(storage_dir, "capture_", ".png")
}

/// Activity-lane events waiting on disk (`vizier_*.json`).
pub fn scan_activity(storage_dir: &std::path::Path) -> Vec<StoredCapture> {
    scan_named(storage_dir, "vizier_", ".json")
}

/// Directory next to screenshots that holds unacked activity events.
pub fn activity_dir(screenshot_dir: &std::path::Path) -> PathBuf {
    screenshot_dir
        .parent()
        .map(|p| p.join("activity"))
        .unwrap_or_else(|| screenshot_dir.join("activity"))
}

fn scan_named(storage_dir: &std::path::Path, prefix: &str, suffix: &str) -> Vec<StoredCapture> {
    let Ok(read) = std::fs::read_dir(storage_dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(prefix) || !name.ends_with(suffix) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            Some(StoredCapture {
                len_bytes: meta.len(),
                modified: meta.modified().ok()?,
                path,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(age_days: u64, total_mb: u64) -> CaptureConfig {
        CaptureConfig {
            retention_max_age_days: age_days,
            retention_max_total_mb: total_mb,
            ..CaptureConfig::default()
        }
    }

    fn entry(name: &str, len_bytes: u64, age: Duration, now: SystemTime) -> StoredCapture {
        StoredCapture {
            path: PathBuf::from(name),
            len_bytes,
            modified: now - age,
        }
    }

    const HOUR: Duration = Duration::from_secs(60 * 60);
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn fresh_frames_are_left_alone() {
        let now = SystemTime::now();
        let plan = plan_sweep(
            vec![entry("a.png", 100, Duration::from_secs(30), now)],
            &cfg(14, 2048),
            now,
        );
        assert!(plan.prune.is_empty());
        assert!(plan.retry.is_empty(), "in-flight frame must not be retried");
    }

    #[test]
    fn old_unsent_frames_are_retried_oldest_first_and_capped() {
        let now = SystemTime::now();
        let entries: Vec<_> = (0..20)
            .map(|i| entry(&format!("f{i:02}.png"), 100, HOUR + Duration::from_secs(i), now))
            .collect();
        let plan = plan_sweep(entries, &cfg(14, 2048), now);
        assert!(plan.prune.is_empty());
        assert_eq!(plan.retry.len(), MAX_RETRIES_PER_SWEEP);
        // Oldest (largest age) first: f19 has the biggest age offset.
        assert_eq!(plan.retry[0], PathBuf::from("f19.png"));
    }

    #[test]
    fn age_cap_prunes_and_zero_disables() {
        let now = SystemTime::now();
        let entries = vec![
            entry("ancient.png", 100, 20 * DAY, now),
            entry("recent.png", 100, DAY, now),
        ];
        let plan = plan_sweep(entries.clone(), &cfg(14, 2048), now);
        assert_eq!(plan.prune, vec![PathBuf::from("ancient.png")]);
        assert_eq!(plan.retry, vec![PathBuf::from("recent.png")]);

        let plan = plan_sweep(entries, &cfg(0, 2048), now);
        assert!(plan.prune.is_empty(), "0 must disable age pruning");
    }

    #[test]
    fn size_cap_evicts_oldest_first_and_zero_disables() {
        let now = SystemTime::now();
        let mb = 1024 * 1024;
        let entries = vec![
            entry("oldest.png", 3 * mb, 3 * HOUR, now),
            entry("middle.png", 3 * mb, 2 * HOUR, now),
            entry("newest.png", 3 * mb, HOUR, now),
        ];
        // 9 MB stored, 5 MB budget → evict the two oldest.
        let plan = plan_sweep(entries.clone(), &cfg(14, 5), now);
        assert_eq!(
            plan.prune,
            vec![PathBuf::from("oldest.png"), PathBuf::from("middle.png")]
        );
        assert_eq!(plan.retry, vec![PathBuf::from("newest.png")]);

        let plan = plan_sweep(entries, &cfg(14, 0), now);
        assert!(plan.prune.is_empty(), "0 must disable the size cap");
        assert_eq!(plan.retry.len(), 3);
    }

    #[test]
    fn summarize_counts_just_written_frames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("capture_a.png"), vec![0u8; 50]).unwrap();
        std::fs::write(dir.path().join("capture_b.png"), vec![0u8; 20]).unwrap();
        let backlog = summarize(dir.path(), SystemTime::now());
        assert_eq!(backlog.count, 2);
        assert_eq!(backlog.bytes, 70);
        assert!(!backlog.is_stuck(), "just-written frames are in flight");
    }

    #[test]
    fn reconnect_sweep_retries_fresh_frames() {
        let now = SystemTime::now();
        let plan = plan_reconnect_sweep(
            vec![entry("a.png", 100, Duration::from_secs(5), now)],
            &cfg(14, 2048),
            now,
        );
        assert!(plan.prune.is_empty());
        assert_eq!(plan.retry, vec![PathBuf::from("a.png")]);
    }

    #[test]
    fn scan_ignores_foreign_files_and_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("capture_x.png"), b"png").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"nope").unwrap();
        std::fs::write(dir.path().join("shot.png"), b"nope").unwrap();
        let entries = scan(dir.path());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].path.ends_with("capture_x.png"));

        assert!(scan(std::path::Path::new("/nonexistent/ci-applet-test")).is_empty());
    }
}
