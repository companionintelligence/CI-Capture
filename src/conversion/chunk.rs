//! Section-aware Markdown chunking with token budget + overlap.
//!
//! See [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 7.

#![allow(dead_code)]

/// Target chunk size in tokens (cl100k_base via tiktoken-rs).
pub const TARGET_TOKENS: usize = 800;

/// Token overlap between consecutive chunks.
pub const OVERLAP_TOKENS: usize = 100;
