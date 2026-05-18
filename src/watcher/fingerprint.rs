//! Stream-hash the settled file, capture inode/size/mtime, and cache the
//! result so we don't re-hash unchanged files.
//!
//! Skeleton — see [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 6.

#![allow(dead_code)]
