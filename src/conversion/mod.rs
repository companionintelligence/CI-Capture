//! Conversion job runner.
//!
//! Pulls events from Phase 2's queue, looks up the right
//! [`super::extractors::Extractor`], runs it under a bounded worker pool,
//! produces an `ExtractedDocument` + Markdown + chunks, and hands the
//! result to the sync engine.
//!
//! **Skeleton — not yet wired in.** See
//! [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 6.

#![allow(dead_code)]

pub mod chunk;
pub mod markdown;
