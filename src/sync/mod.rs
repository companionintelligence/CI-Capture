//! Sync engine — drains the local queue to CI-Server in batches with
//! exponential backoff.
//!
//! **Skeleton — not yet wired in.** See
//! [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 5.

#![allow(dead_code)]

pub mod batching;
pub mod http;
