//! Event normalization: coalesce noisy OS file events, infer atomic
//! saves, infer renames via inode pairing.
//!
//! The [`Normalizer`] is a synchronous state machine driven by an
//! injected clock — the watcher pushes [`RawObservation`]s in and drains
//! [`NormalizedDecision`]s once their debounce window expires. Decoupling
//! it from the wall clock keeps the logic unit-testable without sleeping
//! the test runtime.
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 6
//! for the design and the canonical event-collapsing rules.
//!
//! Storm detection (rate-limited "summary mode") is *not* wired up in
//! this sub-PR — the constants are exposed but the integration lands in
//! a follow-up to keep this diff focused on the coalescing and rename
//! logic.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Default debounce window per path. Lower bounded by typical editor save
/// sequences (a few hundred ms); upper bounded so a single save doesn't take
/// noticeable time to propagate.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(750);
pub const DEBOUNCE_MAX: Duration = Duration::from_secs(5);

/// How long a `Deleted` event waits for a matching `Created` (same inode)
/// before being committed as a real delete.
pub const RENAME_TTL: Duration = Duration::from_millis(750);

/// Storm threshold — see plan § 9. Not enforced yet (follow-up sub-PR).
pub const STORM_EVENTS_PER_SEC: u32 = 200;
pub const STORM_SUSTAIN: Duration = Duration::from_secs(5);

// ── Public types ──────────────────────────────────────────────────────────────

/// Raw observation that the platform-specific `notify_glue` layer feeds
/// into the normalizer. Already normalized across OSes — the platform
/// glue translates `notify::EventKind` into one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawObservation {
    Created { path: PathBuf, inode: Option<u64> },
    Modified { path: PathBuf, inode: Option<u64> },
    Deleted { path: PathBuf, inode: Option<u64> },
    /// OS-level rename — both source and target known.
    Renamed {
        old_path: PathBuf,
        new_path: PathBuf,
        inode: Option<u64>,
    },
}

/// Decision emitted by the normalizer. The fingerprinter + sync engine
/// turn this into a wire-format `NormalizedEvent` by adding the
/// fingerprint payload, ids, and timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedDecision {
    Created { path: PathBuf },
    Modified { path: PathBuf },
    Deleted { path: PathBuf },
    Renamed { old_path: PathBuf, new_path: PathBuf },
}

// ── Internal state ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone)]
struct Pending {
    kind: PendingKind,
    inode: Option<u64>,
    /// Time we last saw an event for this path. Debounce timer runs from
    /// this point.
    last_seen: Instant,
}

#[derive(Debug, Clone)]
struct RecentDelete {
    inode: u64,
    path: PathBuf,
    deleted_at: Instant,
}

// ── Normalizer ────────────────────────────────────────────────────────────────

pub struct Normalizer {
    pending: HashMap<PathBuf, Pending>,
    recent_deletes: Vec<RecentDelete>,
    /// Decisions whose debounce period has not yet elapsed but which
    /// have already been emitted-out-of-band (e.g. an inferred rename
    /// produces an immediate decision; the underlying pending entries
    /// are torn down so nothing leaks into `drain_due`).
    immediate: Vec<NormalizedDecision>,
    debounce: Duration,
    rename_ttl: Duration,
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new(DEBOUNCE_WINDOW, RENAME_TTL)
    }
}

impl Normalizer {
    pub fn new(debounce: Duration, rename_ttl: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            recent_deletes: Vec::new(),
            immediate: Vec::new(),
            debounce,
            rename_ttl,
        }
    }

    /// Consume one raw observation. Some observations produce an
    /// immediate decision (inferred renames, cancel-each-other pairs);
    /// most just update the pending map and wait for the debounce timer.
    ///
    /// Returns an iterator yielding any decisions that were emitted
    /// immediately by this call. The caller can ignore the return value
    /// and just rely on [`drain_due`] / [`drain_immediate`].
    pub fn observe(&mut self, ev: RawObservation, now: Instant) {
        self.expire_recent_deletes(now);

        match ev {
            RawObservation::Created { path, inode } => self.on_created(path, inode, now),
            RawObservation::Modified { path, inode } => self.on_modified(path, inode, now),
            RawObservation::Deleted { path, inode } => self.on_deleted(path, inode, now),
            RawObservation::Renamed { old_path, new_path, inode } => {
                self.on_renamed(old_path, new_path, inode, now);
            }
        }
    }

    /// Return decisions whose debounce window has expired AND any
    /// immediate decisions queued since the last drain.
    pub fn drain_due(&mut self, now: Instant) -> Vec<NormalizedDecision> {
        self.expire_recent_deletes(now);

        let mut out = std::mem::take(&mut self.immediate);

        let ready_paths: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_seen) >= self.debounce)
            .map(|(k, _)| k.clone())
            .collect();

        for path in ready_paths {
            if let Some(pending) = self.pending.remove(&path) {
                let decision = match pending.kind {
                    PendingKind::Created => NormalizedDecision::Created { path },
                    PendingKind::Modified => NormalizedDecision::Modified { path },
                    PendingKind::Deleted => {
                        // A Delete that's lived past rename_ttl without
                        // a matching Create is a real delete.
                        NormalizedDecision::Deleted { path }
                    }
                };
                out.push(decision);
            }
        }
        out
    }

    /// Drain only the immediate-decision queue (without scanning the
    /// pending map for expirations). Useful for tests.
    pub fn drain_immediate(&mut self) -> Vec<NormalizedDecision> {
        std::mem::take(&mut self.immediate)
    }

    /// Force-drain every pending decision regardless of debounce. The
    /// watcher calls this on shutdown so nothing is lost.
    pub fn flush(&mut self) -> Vec<NormalizedDecision> {
        let mut out = std::mem::take(&mut self.immediate);
        for (path, pending) in self.pending.drain() {
            out.push(match pending.kind {
                PendingKind::Created => NormalizedDecision::Created { path },
                PendingKind::Modified => NormalizedDecision::Modified { path },
                PendingKind::Deleted => NormalizedDecision::Deleted { path },
            });
        }
        self.recent_deletes.clear();
        out
    }

    /// True if no pending state remains. Mostly for tests.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.immediate.is_empty()
    }

    // ── Handlers ──────────────────────────────────────────────────────────────

    fn on_created(&mut self, path: PathBuf, inode: Option<u64>, now: Instant) {
        // Rename inference: if a recent Delete matches our inode, treat
        // this Created as the second half of a rename.
        if let Some(inode) = inode
            && let Some(idx) = self
                .recent_deletes
                .iter()
                .position(|d| d.inode == inode && d.path != path)
        {
            let prior = self.recent_deletes.remove(idx);
            // Tear down the pending Delete entry (if still around).
            self.pending.remove(&prior.path);
            self.immediate.push(NormalizedDecision::Renamed {
                old_path: prior.path,
                new_path: path,
            });
            return;
        }

        match self.pending.get(&path).map(|p| p.kind) {
            Some(PendingKind::Deleted) => {
                // Same path was just deleted, now created again. Treat
                // as a modify — content may differ.
                self.upsert(path, PendingKind::Modified, inode, now);
            }
            _ => {
                self.upsert(path, PendingKind::Created, inode, now);
            }
        }
    }

    fn on_modified(&mut self, path: PathBuf, inode: Option<u64>, now: Instant) {
        match self.pending.get(&path).map(|p| p.kind) {
            Some(PendingKind::Created) => {
                // Created + Modified collapses to Created.
                self.upsert(path, PendingKind::Created, inode, now);
            }
            Some(PendingKind::Deleted) => {
                // Modified after Delete is weird (rename race?). Latest
                // wins.
                self.upsert(path, PendingKind::Modified, inode, now);
            }
            _ => {
                self.upsert(path, PendingKind::Modified, inode, now);
            }
        }
    }

    fn on_deleted(&mut self, path: PathBuf, inode: Option<u64>, now: Instant) {
        if let Some(PendingKind::Created) = self.pending.get(&path).map(|p| p.kind) {
            // Created + Deleted cancels — the file never existed from the
            // outside world's perspective.
            self.pending.remove(&path);
            return;
        }

        if let Some(inode) = inode {
            self.recent_deletes.push(RecentDelete {
                inode,
                path: path.clone(),
                deleted_at: now,
            });
        }
        self.upsert(path, PendingKind::Deleted, inode, now);
    }

    fn on_renamed(
        &mut self,
        old_path: PathBuf,
        new_path: PathBuf,
        inode: Option<u64>,
        now: Instant,
    ) {
        // Atomic-save pattern: tmp file was Created, then OS renamed
        // tmp → target. Drop the pending Created on tmp, emit a Modified
        // on the target.
        if matches!(
            self.pending.get(&old_path).map(|p| p.kind),
            Some(PendingKind::Created)
        ) {
            self.pending.remove(&old_path);
            self.upsert(new_path, PendingKind::Modified, inode, now);
            return;
        }

        // Otherwise, plain rename. Tear down any pending state on the
        // old path; emit immediately so callers see the structural
        // change without waiting for debounce.
        self.pending.remove(&old_path);
        self.pending.remove(&new_path);
        self.immediate
            .push(NormalizedDecision::Renamed { old_path, new_path });
    }

    fn upsert(
        &mut self,
        path: PathBuf,
        kind: PendingKind,
        inode: Option<u64>,
        now: Instant,
    ) {
        self.pending.insert(
            path,
            Pending {
                kind,
                inode,
                last_seen: now,
            },
        );
    }

    fn expire_recent_deletes(&mut self, now: Instant) {
        self.recent_deletes
            .retain(|d| now.duration_since(d.deleted_at) <= self.rename_ttl);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn fixed_now() -> Instant {
        Instant::now()
    }

    fn debounced() -> Normalizer {
        Normalizer::new(Duration::from_millis(100), Duration::from_millis(500))
    }

    #[test]
    fn modify_burst_collapses_to_one() {
        let mut n = debounced();
        let t0 = fixed_now();
        for i in 0..10 {
            n.observe(
                RawObservation::Modified { path: pb("/a"), inode: Some(7) },
                t0 + Duration::from_millis(i * 5),
            );
        }
        // Before debounce expires, nothing is due.
        assert!(n.drain_due(t0 + Duration::from_millis(50)).is_empty());

        // After debounce expires, exactly one Modified comes out.
        let due = n.drain_due(t0 + Duration::from_millis(200));
        assert_eq!(due.len(), 1);
        assert!(matches!(
            &due[0],
            NormalizedDecision::Modified { path } if path == &pb("/a")
        ));
        assert!(n.is_empty());
    }

    #[test]
    fn created_then_modified_collapses_to_created() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Created { path: pb("/a"), inode: Some(7) },
            t0,
        );
        n.observe(
            RawObservation::Modified { path: pb("/a"), inode: Some(7) },
            t0 + Duration::from_millis(20),
        );

        let due = n.drain_due(t0 + Duration::from_millis(200));
        assert_eq!(due.len(), 1);
        assert!(matches!(&due[0], NormalizedDecision::Created { path } if path == &pb("/a")));
    }

    #[test]
    fn created_then_deleted_cancels_out() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Created { path: pb("/a"), inode: Some(7) },
            t0,
        );
        n.observe(
            RawObservation::Deleted { path: pb("/a"), inode: Some(7) },
            t0 + Duration::from_millis(10),
        );
        let due = n.drain_due(t0 + Duration::from_millis(200));
        assert!(due.is_empty());
        assert!(n.is_empty());
    }

    #[test]
    fn modified_then_deleted_emits_delete() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Modified { path: pb("/a"), inode: Some(7) },
            t0,
        );
        n.observe(
            RawObservation::Deleted { path: pb("/a"), inode: Some(7) },
            t0 + Duration::from_millis(10),
        );
        let due = n.drain_due(t0 + Duration::from_millis(700));
        // Past rename_ttl + debounce, the delete is a real delete.
        assert_eq!(due.len(), 1);
        assert!(matches!(&due[0], NormalizedDecision::Deleted { path } if path == &pb("/a")));
    }

    #[test]
    fn delete_then_create_with_matching_inode_is_rename() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Deleted { path: pb("/old"), inode: Some(42) },
            t0,
        );
        n.observe(
            RawObservation::Created { path: pb("/new"), inode: Some(42) },
            t0 + Duration::from_millis(10),
        );

        // Rename emits IMMEDIATELY (not after debounce).
        let immediate = n.drain_immediate();
        assert_eq!(immediate.len(), 1);
        assert!(matches!(
            &immediate[0],
            NormalizedDecision::Renamed { old_path, new_path }
                if old_path == &pb("/old") && new_path == &pb("/new")
        ));
        // And nothing remains pending.
        let due = n.drain_due(t0 + Duration::from_millis(500));
        assert!(due.is_empty());
    }

    #[test]
    fn delete_then_create_with_different_inode_are_separate_events() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Deleted { path: pb("/old"), inode: Some(1) },
            t0,
        );
        n.observe(
            RawObservation::Created { path: pb("/new"), inode: Some(2) },
            t0 + Duration::from_millis(10),
        );
        let due = n.drain_due(t0 + Duration::from_millis(700));
        // Both should appear, neither as a rename.
        assert_eq!(due.len(), 2);
        assert!(due.iter().any(|d| matches!(
            d,
            NormalizedDecision::Deleted { path } if path == &pb("/old")
        )));
        assert!(due.iter().any(|d| matches!(
            d,
            NormalizedDecision::Created { path } if path == &pb("/new")
        )));
    }

    #[test]
    fn delete_then_late_create_does_not_become_rename() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Deleted { path: pb("/old"), inode: Some(7) },
            t0,
        );
        // Create arrives after rename_ttl has expired.
        n.observe(
            RawObservation::Created { path: pb("/new"), inode: Some(7) },
            t0 + Duration::from_millis(800),
        );
        let due = n.drain_due(t0 + Duration::from_millis(1500));
        // Both treated as separate events.
        assert_eq!(due.len(), 2);
        assert!(due
            .iter()
            .any(|d| matches!(d, NormalizedDecision::Deleted { path } if path == &pb("/old"))));
        assert!(due
            .iter()
            .any(|d| matches!(d, NormalizedDecision::Created { path } if path == &pb("/new"))));
    }

    #[test]
    fn os_rename_emits_renamed_immediately() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Renamed {
                old_path: pb("/a"),
                new_path: pb("/b"),
                inode: Some(7),
            },
            t0,
        );
        let imm = n.drain_immediate();
        assert_eq!(imm.len(), 1);
        assert!(matches!(
            &imm[0],
            NormalizedDecision::Renamed { old_path, new_path }
                if old_path == &pb("/a") && new_path == &pb("/b")
        ));
    }

    #[test]
    fn atomic_save_create_tmp_then_rename_emits_modified_on_target() {
        let mut n = debounced();
        let t0 = fixed_now();
        // Editor writes tmp file…
        n.observe(
            RawObservation::Created { path: pb("/doc.tmp"), inode: Some(7) },
            t0,
        );
        n.observe(
            RawObservation::Modified { path: pb("/doc.tmp"), inode: Some(7) },
            t0 + Duration::from_millis(5),
        );
        // …then atomically renames into target.
        n.observe(
            RawObservation::Renamed {
                old_path: pb("/doc.tmp"),
                new_path: pb("/doc"),
                inode: Some(7),
            },
            t0 + Duration::from_millis(20),
        );

        // The pending Created on the tmp file is gone.
        assert!(n.drain_immediate().is_empty());
        // After debounce, exactly one Modified comes out for the target.
        let due = n.drain_due(t0 + Duration::from_millis(200));
        assert_eq!(due.len(), 1, "got {due:?}");
        assert!(matches!(
            &due[0],
            NormalizedDecision::Modified { path } if path == &pb("/doc")
        ));
    }

    #[test]
    fn flush_drains_all_pending() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Modified { path: pb("/a"), inode: None },
            t0,
        );
        n.observe(
            RawObservation::Created { path: pb("/b"), inode: None },
            t0,
        );
        let out = n.flush();
        assert_eq!(out.len(), 2);
        assert!(n.is_empty());
    }

    #[test]
    fn drain_due_respects_debounce() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Modified { path: pb("/a"), inode: None },
            t0,
        );
        // Slightly before the window — still pending.
        assert!(n.drain_due(t0 + Duration::from_millis(99)).is_empty());
        // At exactly the window — due.
        let due = n.drain_due(t0 + Duration::from_millis(100));
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn delete_without_inode_is_just_a_delete() {
        let mut n = debounced();
        let t0 = fixed_now();
        n.observe(
            RawObservation::Deleted { path: pb("/x"), inode: None },
            t0,
        );
        n.observe(
            RawObservation::Created { path: pb("/y"), inode: None },
            t0 + Duration::from_millis(10),
        );
        // Without inodes we can't pair them — both fire after debounce.
        let due = n.drain_due(t0 + Duration::from_millis(700));
        assert_eq!(due.len(), 2);
    }
}
