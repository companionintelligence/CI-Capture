//! Adapter from `notify::Event` to [`super::normalizer::RawObservation`].
//!
//! `notify` reports OS file-system events with platform-specific
//! `EventKind` shapes. This layer collapses them into the stable
//! [`RawObservation`] variants the normalizer consumes.
//!
//! Path-level ignore filtering happens here too — an ignored path never
//! travels further down the pipeline, so the normalizer + queue + sync
//! never see it.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc as tokio_mpsc;

use super::ignore::IgnoreRules;
use super::normalizer::RawObservation;

/// Handle returned by [`spawn_notify_watcher`]. Dropping it stops the
/// background thread and releases the OS watcher.
pub struct NotifyHandle {
    _watcher: RecommendedWatcher,
    _bridge: thread::JoinHandle<()>,
}

/// Start an OS file-system watcher on every path in `roots`.
///
/// Events from the OS are translated into [`RawObservation`]s and sent
/// down the tokio channel `tx`. Paths matching `ignore` are dropped
/// before normalization sees them.
///
/// Returns a handle that owns the watcher and the bridge thread; both
/// stop when the handle is dropped.
pub fn spawn_notify_watcher(
    roots: &[PathBuf],
    ignore: IgnoreRules,
    tx: tokio_mpsc::UnboundedSender<RawObservation>,
) -> Result<NotifyHandle> {
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = raw_tx.send(res);
    })
    .context("creating recommended_watcher")?;

    for root in roots {
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.display()))?;
    }

    let bridge = thread::spawn(move || {
        for res in raw_rx {
            match res {
                Ok(event) => {
                    for obs in event_to_observations(&event, &ignore) {
                        if tx.send(obs).is_err() {
                            // Receiver dropped — worker is shutting down.
                            return;
                        }
                    }
                }
                Err(err) => {
                    log::warn!("notify error: {err}");
                }
            }
        }
    });

    Ok(NotifyHandle {
        _watcher: watcher,
        _bridge: bridge,
    })
}

/// Pure translator: one `notify::Event` → zero or more
/// `RawObservation`s. Public + side-effect-free so it can be unit tested
/// without any actual filesystem.
pub fn event_to_observations(event: &Event, ignore: &IgnoreRules) -> Vec<RawObservation> {
    let mut out = Vec::new();
    let paths = &event.paths;
    match &event.kind {
        EventKind::Create(_) => {
            for path in paths {
                if accept(path, ignore) {
                    out.push(RawObservation::Created {
                        path: path.clone(),
                        inode: inode_of(path),
                    });
                }
            }
        }
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any) => {
            for path in paths {
                if accept(path, ignore) {
                    out.push(RawObservation::Modified {
                        path: path.clone(),
                        inode: inode_of(path),
                    });
                }
            }
        }
        EventKind::Modify(ModifyKind::Metadata(_)) => {
            // Metadata-only changes are not interesting for content sync.
            // We could surface them as `FileMetadataChanged` events later
            // but the MVP ignores them.
        }
        EventKind::Modify(ModifyKind::Name(rename_mode)) => match rename_mode {
            RenameMode::Both if paths.len() == 2 => {
                let old_path = paths[0].clone();
                let new_path = paths[1].clone();
                let both_ignored = !accept(&old_path, ignore) && !accept(&new_path, ignore);
                if !both_ignored {
                    out.push(RawObservation::Renamed {
                        // We pass the inode of whichever path now exists.
                        inode: inode_of(&new_path).or_else(|| inode_of(&old_path)),
                        old_path,
                        new_path,
                    });
                }
            }
            RenameMode::From => {
                // First half of a rename; record as a delete carrying
                // the inode so the normalizer can pair it up.
                for path in paths {
                    if accept(path, ignore) {
                        out.push(RawObservation::Deleted {
                            inode: inode_of(path),
                            path: path.clone(),
                        });
                    }
                }
            }
            RenameMode::To => {
                // Second half of a rename; record as a create. The
                // normalizer's recent-delete window will pair the two.
                for path in paths {
                    if accept(path, ignore) {
                        out.push(RawObservation::Created {
                            path: path.clone(),
                            inode: inode_of(path),
                        });
                    }
                }
            }
            // `Any` / `Other` — no portable mapping; surface as Modified
            // so we at least pick up content changes.
            _ => {
                for path in paths {
                    if accept(path, ignore) {
                        out.push(RawObservation::Modified {
                            path: path.clone(),
                            inode: inode_of(path),
                        });
                    }
                }
            }
        },
        EventKind::Remove(_) => {
            for path in paths {
                if accept(path, ignore) {
                    out.push(RawObservation::Deleted {
                        path: path.clone(),
                        // Inode unread-able after removal; the normalizer
                        // falls back to path-pair heuristics.
                        inode: None,
                    });
                }
            }
        }
        // Access events, Other, Any — not interesting.
        _ => {}
    }
    out
}

fn accept(path: &Path, ignore: &IgnoreRules) -> bool {
    !ignore.is_ignored(path)
}

/// Read the inode (Unix) / FileID (Windows) of a path. Returns `None`
/// when the path is gone or the platform doesn't expose it.
fn inode_of(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    file_id(&meta)
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(windows)]
fn file_id(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::windows::fs::MetadataExt;
    meta.file_index()
}

#[cfg(not(any(unix, windows)))]
fn file_id(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use notify::event::{AccessKind, CreateKind, EventAttributes, ModifyKind, RemoveKind};

    use super::*;

    fn defaults() -> IgnoreRules {
        IgnoreRules::with_defaults().unwrap()
    }

    fn evt(kind: EventKind, paths: Vec<PathBuf>) -> Event {
        Event {
            kind,
            paths,
            attrs: EventAttributes::default(),
        }
    }

    #[test]
    fn create_file_event_becomes_created_observation() {
        let e = evt(EventKind::Create(CreateKind::File), vec![PathBuf::from("/tmp/a.txt")]);
        let obs = event_to_observations(&e, &defaults());
        assert_eq!(obs.len(), 1);
        assert!(matches!(
            &obs[0],
            RawObservation::Created { path, .. } if path == &PathBuf::from("/tmp/a.txt")
        ));
    }

    #[test]
    fn modify_data_event_becomes_modified() {
        let e = evt(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            vec![PathBuf::from("/tmp/a.txt")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0], RawObservation::Modified { .. }));
    }

    #[test]
    fn rename_both_becomes_renamed_with_two_paths() {
        let e = evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert_eq!(obs.len(), 1);
        assert!(matches!(
            &obs[0],
            RawObservation::Renamed { old_path, new_path, .. }
                if old_path == &PathBuf::from("/tmp/a") && new_path == &PathBuf::from("/tmp/b")
        ));
    }

    #[test]
    fn rename_from_becomes_delete() {
        let e = evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec![PathBuf::from("/tmp/x")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0], RawObservation::Deleted { .. }));
    }

    #[test]
    fn rename_to_becomes_create() {
        let e = evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            vec![PathBuf::from("/tmp/y")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0], RawObservation::Created { .. }));
    }

    #[test]
    fn remove_event_becomes_delete() {
        let e = evt(
            EventKind::Remove(RemoveKind::File),
            vec![PathBuf::from("/tmp/x")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert_eq!(obs.len(), 1);
        assert!(matches!(&obs[0], RawObservation::Deleted { inode: None, .. }));
    }

    #[test]
    fn access_event_is_dropped() {
        let e = evt(
            EventKind::Access(AccessKind::Read),
            vec![PathBuf::from("/tmp/a")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert!(obs.is_empty());
    }

    #[test]
    fn metadata_only_modify_is_dropped() {
        let e = evt(
            EventKind::Modify(ModifyKind::Metadata(
                notify::event::MetadataKind::Permissions,
            )),
            vec![PathBuf::from("/tmp/a")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert!(obs.is_empty());
    }

    #[test]
    fn ignored_paths_are_dropped() {
        let e = evt(
            EventKind::Create(CreateKind::File),
            vec![PathBuf::from("/tmp/proj/node_modules/.bin/tsc")],
        );
        let obs = event_to_observations(&e, &defaults());
        assert!(obs.is_empty());
    }

    #[test]
    fn rename_with_one_path_falls_back_to_modify() {
        let e = evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![PathBuf::from("/tmp/onlyone")],
        );
        let obs = event_to_observations(&e, &defaults());
        // Only one path → can't build a Renamed; we fall through to the
        // catch-all arm and emit a Modified, so the normalizer's rename
        // inference can still pair via inode if it happens to match.
        assert_eq!(obs.len(), 1);
        assert!(
            matches!(&obs[0], RawObservation::Modified { .. }),
            "single-path Rename(Both) should fall back to Modified: {obs:?}"
        );
    }
}
