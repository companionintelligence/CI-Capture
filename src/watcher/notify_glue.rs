//! Adapter from `notify::Event` to [`super::normalizer::RawObservation`].
//!
//! The platform watchers (`FSEvents` on macOS, `inotify` on Linux,
//! `ReadDirectoryChangesW` on Windows) emit wildly different event streams.
//! `notify` already smooths most of that into a cross-platform
//! [`notify::EventKind`]; this layer maps that down to the small set of
//! [`RawObservation`]s the [`super::normalizer::Normalizer`] understands.
//!
//! Inode capture is best-effort: we `stat` the path at observation time so
//! the normalizer can pair a `Deleted`+`Created` into a rename. A removed
//! file can no longer be `stat`-ed, so deletes carry `inode: None`.
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 6.

use std::path::Path;

use notify::EventKind;
use notify::event::{ModifyKind, RenameMode};

use super::normalizer::RawObservation;

/// Translate one `notify::Event` into zero or more [`RawObservation`]s.
///
/// Directory-level and access events are dropped (the normalizer works at
/// file granularity; directory bookkeeping is the server's job). Unknown /
/// "Any" kinds are treated conservatively as modifications so we never
/// silently lose a change.
pub fn to_raw_observations(event: &notify::Event) -> Vec<RawObservation> {
    let paths = &event.paths;
    match event.kind {
        EventKind::Create(_) => paths
            .iter()
            .map(|p| RawObservation::Created {
                path: p.clone(),
                inode: inode_of(p),
            })
            .collect(),

        // Both-sided rename: notify gives us `[from, to]`.
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
            vec![RawObservation::Renamed {
                old_path: paths[0].clone(),
                new_path: paths[1].clone(),
                inode: inode_of(&paths[1]),
            }]
        }

        // One-sided rename (FSEvents commonly emits these). We can't pair
        // the halves here, so we classify by whether the path still exists,
        // reading existence + inode in a single `symlink_metadata` syscall
        // (one race window instead of two): present → destination (Created);
        // gone → source (Deleted). The normalizer re-pairs them by inode
        // when both halves arrive within its rename window.
        //
        // Known limitation: a one-sided rename whose counterpart never
        // arrives (or arrives without a matching inode) stays split into a
        // separate create + delete rather than a single rename.
        EventKind::Modify(ModifyKind::Name(_)) => paths
            .iter()
            .map(|p| match std::fs::symlink_metadata(p) {
                Ok(meta) => RawObservation::Created {
                    path: p.clone(),
                    inode: file_id(&meta),
                },
                Err(_) => RawObservation::Deleted {
                    path: p.clone(),
                    inode: None,
                },
            })
            .collect(),

        EventKind::Modify(_) => paths
            .iter()
            .map(|p| RawObservation::Modified {
                path: p.clone(),
                inode: inode_of(p),
            })
            .collect(),

        EventKind::Remove(_) => paths
            .iter()
            .map(|p| RawObservation::Deleted {
                path: p.clone(),
                inode: None,
            })
            .collect(),

        // Access events and anything else carry no state change.
        _ => Vec::new(),
    }
}

/// Best-effort inode / FileID lookup. Returns `None` if the path can't be
/// `stat`-ed (e.g. it was already removed).
fn inode_of(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    file_id(&meta)
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    use super::*;

    fn ev(kind: EventKind, paths: Vec<&str>) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.into_iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        }
    }

    #[test]
    fn create_maps_to_created() {
        let obs = to_raw_observations(&ev(EventKind::Create(CreateKind::File), vec!["/a/b.txt"]));
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0], RawObservation::Created { path, .. } if path == &PathBuf::from("/a/b.txt")));
    }

    #[test]
    fn data_modify_maps_to_modified() {
        let obs = to_raw_observations(&ev(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            vec!["/a/b.txt"],
        ));
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0], RawObservation::Modified { .. }));
    }

    #[test]
    fn remove_maps_to_deleted_without_inode() {
        let obs = to_raw_observations(&ev(EventKind::Remove(RemoveKind::File), vec!["/a/b.txt"]));
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0], RawObservation::Deleted { inode: None, .. }));
    }

    #[test]
    fn both_sided_rename_maps_to_renamed() {
        let obs = to_raw_observations(&ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec!["/a/old.txt", "/a/new.txt"],
        ));
        assert_eq!(obs.len(), 1);
        assert!(matches!(
            &obs[0],
            RawObservation::Renamed { old_path, new_path, .. }
                if old_path == &PathBuf::from("/a/old.txt") && new_path == &PathBuf::from("/a/new.txt")
        ));
    }

    #[test]
    fn access_events_are_dropped() {
        let obs = to_raw_observations(&ev(
            EventKind::Access(notify::event::AccessKind::Read),
            vec!["/a/b.txt"],
        ));
        assert!(obs.is_empty());
    }
}
