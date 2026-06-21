//! File-activity watcher — the second capture lane alongside the existing
//! screenshot + vizier lanes in [`crate::worker`].
//!
//! It watches a user-selected set of folders, normalizes OS-native file
//! events into a stable schema ([`types::NormalizedEvent`]), waits for each
//! changed file to settle, fingerprints it (sha256 + size + mtime + inode),
//! and streams the events out on a bounded channel for the file-tracker
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender, channel, unbounded_channel};
use tokio::time::{Duration, interval};
use uuid::Uuid;

/// Per-path cache of the last content hash we shipped, used to drop
/// identical re-saves (a `touch`, a no-op editor save, a metadata change)
/// before they re-hash, re-extract, and re-ship the same bytes. In-memory
/// and per-watcher-session — cleared on restart, which is fine.
type HashCache = Arc<Mutex<HashMap<PathBuf, String>>>;

use self::fingerprint::fingerprint;
use self::ignore::IgnoreRules;
use self::normalizer::{NormalizedDecision, Normalizer};
use self::settle::{SettleStatus, wait_for_settle};
use self::types::{EventType, FileFingerprint, NormalizedEvent};

/// How often the driver drains debounce-expired decisions from the
/// normalizer. Tighter than the debounce window so events propagate
/// promptly once they settle.
const DRAIN_TICK: Duration = Duration::from_millis(250);

/// Max files settled + fingerprinted concurrently. Bounds total in-flight
/// dispatch work so a burst of edits (or a Time Machine restore) can't fan
/// out into thousands of concurrent settle/hash tasks. Excess decisions
/// park cheaply on the semaphore until a slot frees.
const MAX_CONCURRENT_FINGERPRINTS: usize = 4;

/// Bound on the outbound event channel. Far above any normal edit rate;
/// the cap exists so a long sync stall (server outage) can't grow memory
/// without limit — overflow is dropped with a warning, not buffered forever.
const EVENT_CHANNEL_CAPACITY: usize = 16_384;

/// Handle returned by [`start_watcher`]. Owns the underlying OS watcher —
/// dropping it stops file-system notifications and lets the driver task
/// wind down (its raw channel closes).
pub struct WatcherHandle {
    /// Normalized events as they're observed and fingerprinted.
    pub events: Receiver<NormalizedEvent>,
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

/// Outcome of settling + fingerprinting a path. Distinguishing these is
/// what keeps an oversized or transiently-unreadable file from being
/// misreported as a deletion.
enum SettleOutcome {
    /// The file is gone (settle saw it removed, or it vanished after).
    Deleted,
    /// Settled and hashed successfully.
    Fingerprinted(FileFingerprint),
    /// The file exists but we deliberately or transiently couldn't hash it
    /// (over the size cap, unreadable, settle error). Still a real
    /// create/modify — just without content.
    Unhashable,
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

    // Raw notify events stay unbounded: the OS callback can't apply
    // backpressure. Volume is bounded downstream by the fingerprint
    // semaphore and the normalizer's coalescing.
    let (raw_tx, mut raw_rx) = unbounded_channel::<notify::Event>();
    let (event_tx, event_rx) = channel::<NormalizedEvent>(EVENT_CHANNEL_CAPACITY);

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
    let cache: HashCache = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(async move {
        let mut normalizer = Normalizer::default();
        let mut tick = interval(DRAIN_TICK);

        loop {
            tokio::select! {
                maybe = raw_rx.recv() => {
                    match maybe {
                        Some(event) => {
                            for obs in notify_glue::to_raw_observations(&event) {
                                // Privacy gate: drop the whole observation if
                                // ANY of its paths is ignored. For a rename
                                // that means BOTH old and new — so a sensitive
                                // source path can never leak through `old_path`.
                                let paths = obs_paths(&obs);
                                if paths.iter().any(|p| ignore.is_ignored(p)) {
                                    for p in &paths {
                                        if ignore.is_sensitive(p) {
                                            log::debug!("refusing sensitive path: {}", p.display());
                                        }
                                    }
                                    continue;
                                }
                                normalizer.observe(obs, Instant::now());
                            }
                        }
                        None => {
                            // Watcher dropped — flush and exit.
                            for decision in normalizer.flush() {
                                dispatch(decision, &ignore, &permits, &event_tx, &cache);
                            }
                            break;
                        }
                    }
                }
                _ = tick.tick() => {
                    for decision in normalizer.drain_due(Instant::now()) {
                        dispatch(decision, &ignore, &permits, &event_tx, &cache);
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

/// Turn a settled decision into a `NormalizedEvent` and send it. The
/// settle + fingerprint work runs on a spawned task holding a semaphore
/// permit, so total in-flight work is bounded and a slow settle never
/// stalls the driver loop.
fn dispatch(
    decision: NormalizedDecision,
    ignore: &IgnoreRules,
    permits: &Arc<Semaphore>,
    event_tx: &Sender<NormalizedEvent>,
    cache: &HashCache,
) {
    let ignore = ignore.clone();
    let permits = permits.clone();
    let event_tx = event_tx.clone();
    let cache = cache.clone();

    tokio::spawn(async move {
        // Bound concurrency over the whole settle+hash, not just the hash.
        let _permit = match permits.acquire_owned().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed — shutting down
        };

        // Defensive privacy guard: never emit an event touching an
        // ignored/sensitive path. Observation-time gating already covers
        // this; this is belt-and-suspenders for the privacy boundary.
        if decision_paths(&decision).iter().any(|p| ignore.is_ignored(p)) {
            return;
        }

        let observed_at = Utc::now();
        let event = match decision {
            NormalizedDecision::Created { path } => {
                match settle_and_fingerprint(&path, &ignore).await {
                    SettleOutcome::Deleted => {
                        cache.lock().remove(&path);
                        delete_event(observed_at, path)
                    }
                    SettleOutcome::Fingerprinted(fp) => {
                        if is_duplicate(&cache, &path, &fp.content_hash) {
                            return; // identical content already shipped
                        }
                        change_event(EventType::FileCreated, observed_at, path, Some(fp))
                    }
                    SettleOutcome::Unhashable => {
                        change_event(EventType::FileCreated, observed_at, path, None)
                    }
                }
            }
            NormalizedDecision::Modified { path } => {
                match settle_and_fingerprint(&path, &ignore).await {
                    SettleOutcome::Deleted => {
                        cache.lock().remove(&path);
                        delete_event(observed_at, path)
                    }
                    SettleOutcome::Fingerprinted(fp) => {
                        if is_duplicate(&cache, &path, &fp.content_hash) {
                            return; // no-op save / touch — content unchanged
                        }
                        change_event(EventType::FileModified, observed_at, path, Some(fp))
                    }
                    SettleOutcome::Unhashable => {
                        change_event(EventType::FileModified, observed_at, path, None)
                    }
                }
            }
            NormalizedDecision::Deleted { path } => {
                cache.lock().remove(&path);
                delete_event(observed_at, path)
            }
            NormalizedDecision::Renamed { old_path, new_path } => {
                cache.lock().remove(&old_path);
                // A rename is structural; the target may already be gone,
                // which doesn't undo the rename. Attach a fingerprint only
                // when the target settled cleanly.
                let file = match settle_and_fingerprint(&new_path, &ignore).await {
                    SettleOutcome::Fingerprinted(fp) => {
                        cache.lock().insert(new_path.clone(), fp.content_hash.clone());
                        Some(fp)
                    }
                    _ => None,
                };
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

        match event_tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                log::warn!("watcher event channel full; dropping a file event (sync is stalled)")
            }
            Err(TrySendError::Closed(_)) => {} // consumer gone — shutting down
        }
    });
}

/// True when `hash` matches the last hash we shipped for `path`. When it
/// differs (or is new), the cache is updated so the next identical save is
/// suppressed. Deletes/renames evict the entry separately.
fn is_duplicate(cache: &HashCache, path: &Path, hash: &str) -> bool {
    let mut guard = cache.lock();
    if guard.get(path).map(String::as_str) == Some(hash) {
        true
    } else {
        guard.insert(path.to_path_buf(), hash.to_string());
        false
    }
}

fn change_event(
    event_type: EventType,
    observed_at: chrono::DateTime<Utc>,
    path: PathBuf,
    file: Option<FileFingerprint>,
) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Uuid::new_v4().to_string(),
        event_type,
        occurred_at: observed_at,
        observed_at,
        file,
        old_path: None,
        new_path: Some(path),
        correlation_id: None,
    }
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

/// Every path an observation touches. A rename touches two — both must be
/// gated so a sensitive *source* can't leak through `old_path`.
fn obs_paths(obs: &normalizer::RawObservation) -> Vec<&Path> {
    match obs {
        normalizer::RawObservation::Created { path, .. }
        | normalizer::RawObservation::Modified { path, .. }
        | normalizer::RawObservation::Deleted { path, .. } => vec![path.as_path()],
        normalizer::RawObservation::Renamed { old_path, new_path, .. } => {
            vec![old_path.as_path(), new_path.as_path()]
        }
    }
}

/// Every path a normalized decision touches (used by the defensive guard).
fn decision_paths(decision: &NormalizedDecision) -> Vec<&Path> {
    match decision {
        NormalizedDecision::Created { path }
        | NormalizedDecision::Modified { path }
        | NormalizedDecision::Deleted { path } => vec![path.as_path()],
        NormalizedDecision::Renamed { old_path, new_path } => {
            vec![old_path.as_path(), new_path.as_path()]
        }
    }
}

/// Settle then fingerprint, returning a [`SettleOutcome`] that distinguishes
/// a genuine deletion from "exists but no hash" (oversized / unreadable).
/// The caller already holds a concurrency permit.
async fn settle_and_fingerprint(path: &std::path::Path, ignore: &IgnoreRules) -> SettleOutcome {
    match wait_for_settle(path).await {
        Ok(SettleStatus::Deleted) => return SettleOutcome::Deleted,
        Ok(_) => {}
        Err(err) => {
            log::debug!("settle failed for {}: {err}", path.display());
            return SettleOutcome::Unhashable;
        }
    }

    match std::fs::metadata(path) {
        Ok(meta) => {
            if ignore.exceeds_size_limit(meta.len()) {
                log::debug!(
                    "skipping hash for oversized file ({} bytes): {}",
                    meta.len(),
                    path.display()
                );
                return SettleOutcome::Unhashable;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return SettleOutcome::Deleted,
        Err(err) => {
            log::debug!("stat failed for {}: {err}", path.display());
            return SettleOutcome::Unhashable;
        }
    }

    let path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || fingerprint(&path)).await {
        Ok(Ok(fp)) => SettleOutcome::Fingerprinted(fp),
        Ok(Err(err)) => {
            log::debug!("fingerprint failed: {err}");
            SettleOutcome::Unhashable
        }
        Err(err) => {
            log::debug!("fingerprint task panicked: {err}");
            SettleOutcome::Unhashable
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tokio::time::{Duration as TDur, timeout};

    use super::*;

    fn cfg(dir: &std::path::Path) -> WatcherConfig {
        WatcherConfig {
            watched_paths: vec![dir.to_path_buf()],
            ignore_globs: Vec::new(),
            max_file_bytes: 100 * 1024 * 1024,
        }
    }

    #[test]
    fn obs_paths_covers_both_halves_of_a_rename() {
        let obs = normalizer::RawObservation::Renamed {
            old_path: PathBuf::from("/a/old"),
            new_path: PathBuf::from("/a/new"),
            inode: Some(1),
        };
        let paths = obs_paths(&obs);
        // Both halves must be gated, so a sensitive source path is caught.
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&Path::new("/a/old")));
        assert!(paths.contains(&Path::new("/a/new")));
    }

    #[test]
    fn is_duplicate_suppresses_identical_content() {
        let cache: HashCache = Arc::new(Mutex::new(HashMap::new()));
        let p = Path::new("/x/notes.md");
        // First sight of a hash is never a duplicate; it gets cached.
        assert!(!is_duplicate(&cache, p, "sha256:aaa"));
        // Same hash again → duplicate (a no-op save / touch).
        assert!(is_duplicate(&cache, p, "sha256:aaa"));
        // Real content change → not a duplicate, re-cached.
        assert!(!is_duplicate(&cache, p, "sha256:bbb"));
        assert!(is_duplicate(&cache, p, "sha256:bbb"));
    }

    /// End-to-end smoke test of the whole glue: a real `notify` watcher on a
    /// temp dir → normalizer → settle → fingerprint → emitted event. Uses a
    /// generous timeout because FSEvents/inotify delivery + settle polling is
    /// inherently latency-bound.
    #[tokio::test]
    async fn detects_a_created_file_end_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut handle = start_watcher(cfg(dir.path())).expect("start watcher");

        // Let the OS watch register before we touch the tree.
        tokio::time::sleep(TDur::from_millis(500)).await;

        let path = dir.path().join("notes.md");
        let body = b"# hello\n\nworld\n"; // 15 bytes
        {
            let mut f = std::fs::File::create(&path).expect("create");
            f.write_all(body).expect("write");
            f.flush().expect("flush");
        }

        let event = timeout(TDur::from_secs(20), handle.events.recv())
            .await
            .expect("timed out waiting for a file event")
            .expect("event channel closed unexpectedly");

        assert!(
            matches!(
                event.event_type,
                EventType::FileCreated | EventType::FileModified
            ),
            "unexpected event type: {:?}",
            event.event_type
        );
        let p = event.new_path.as_ref().expect("event carries a path");
        assert!(p.ends_with("notes.md"), "unexpected path: {p:?}");

        let fp = event.file.expect("created file carries a fingerprint");
        assert!(fp.content_hash.starts_with("sha256:"));
        assert_eq!(fp.size_bytes, body.len() as u64);
    }

    /// A path matching the (non-disableable) sensitive list must never produce
    /// an event, even when it lives inside a watched folder.
    #[tokio::test]
    async fn ignored_paths_produce_no_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Build a config whose user globs ignore *.secret on top of defaults.
        let mut config = cfg(dir.path());
        config.ignore_globs.push("**/*.secret".to_string());
        let mut handle = start_watcher(config).expect("start watcher");
        tokio::time::sleep(TDur::from_millis(500)).await;

        // Write an ignored file, then a tracked file. We must observe the
        // tracked one and never the ignored one.
        std::fs::write(dir.path().join("creds.secret"), b"do not ship").unwrap();
        tokio::time::sleep(TDur::from_millis(200)).await;
        std::fs::write(dir.path().join("keep.txt"), b"track me").unwrap();

        let event = timeout(TDur::from_secs(20), handle.events.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        let p = event.new_path.as_ref().unwrap();
        assert!(
            p.ends_with("keep.txt"),
            "should only see the tracked file, got {p:?}"
        );
    }
}
