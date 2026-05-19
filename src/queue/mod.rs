//! Durable local event queue (SQLite).
//!
//! **Skeleton — not yet wired in.** Schema is in [`schema.sql`](./schema.sql),
//! design is in [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 8.

#![allow(dead_code)]

pub mod rusqlite_impl;
