//! Section-aware Markdown chunking with token budget + overlap.
//!
//! Splits a list of [`crate::extractors::traits::Section`]s into
//! [`Chunk`]s the downstream embedding API ingests:
//!
//! - Section boundaries are respected — a chunk never crosses sections.
//! - Sections smaller than [`TARGET_TOKENS`] become a single chunk.
//! - Larger sections are split on paragraph boundaries with
//!   [`OVERLAP_TOKENS`] of overlap between adjacent chunks so embeddings
//!   don't lose context at the cut point.
//!
//! ## Token counting
//!
//! [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 7
//! specifies `tiktoken-rs` with `cl100k_base`. To keep this sub-PR free
//! of new runtime deps, [`count_tokens`] currently uses a chars/4
//! approximation that matches `cl100k_base` to within ~10% on English
//! text. Swapping in the real tokenizer is one function body away —
//! see the TODO inside [`count_tokens`].

use serde::{Deserialize, Serialize};

use crate::extractors::traits::Section;

/// Target chunk size in tokens (cl100k_base via tiktoken-rs eventually).
pub const TARGET_TOKENS: usize = 800;

/// Token overlap between consecutive chunks within the same section.
pub const OVERLAP_TOKENS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Chunk {
    /// Position in the output stream. Monotonically increasing across the
    /// whole document, not per-section.
    pub order: u32,
    /// Anchor of the section this chunk came from.
    pub section_anchor: String,
    /// Markdown body. Already includes the heading line as context when
    /// the chunk is the first slice of its section.
    pub markdown: String,
    /// Approximate token count by [`count_tokens`].
    pub token_count: usize,
}

/// Split a list of sections into chunks.
///
/// Use [`TARGET_TOKENS`] / [`OVERLAP_TOKENS`] by default; pass custom
/// values for tests or per-format tuning.
pub fn chunk_sections(sections: &[Section]) -> Vec<Chunk> {
    chunk_sections_with(sections, TARGET_TOKENS, OVERLAP_TOKENS)
}

pub fn chunk_sections_with(
    sections: &[Section],
    target_tokens: usize,
    overlap_tokens: usize,
) -> Vec<Chunk> {
    debug_assert!(target_tokens > 0, "target_tokens must be > 0");
    debug_assert!(
        overlap_tokens < target_tokens,
        "overlap_tokens must be < target_tokens"
    );

    let mut out = Vec::new();
    let mut order: u32 = 0;

    for section in sections {
        let body = section.markdown.trim();
        if body.is_empty() {
            continue;
        }

        let total = count_tokens(body);
        if total <= target_tokens {
            out.push(Chunk {
                order,
                section_anchor: section.anchor.clone(),
                markdown: body.to_string(),
                token_count: total,
            });
            order += 1;
            continue;
        }

        // Split the section on paragraph boundaries, accumulating until
        // we'd exceed `target_tokens`. Overlap is implemented by carrying
        // the trailing `overlap_tokens` of the previous chunk into the
        // next chunk's prefix.
        let paragraphs = split_paragraphs(body);
        let mut current = String::new();
        let mut current_tokens = 0usize;
        let mut prev_tail: Option<String> = None;

        for para in &paragraphs {
            let para_tokens = count_tokens(para);
            // Edge case: a single paragraph larger than target_tokens.
            // Hard-split it by word run.
            if para_tokens > target_tokens {
                if !current.is_empty() {
                    let chunk_md = finalize(&current, &prev_tail);
                    let token_count = count_tokens(&chunk_md);
                    out.push(Chunk {
                        order,
                        section_anchor: section.anchor.clone(),
                        markdown: chunk_md.clone(),
                        token_count,
                    });
                    order += 1;
                    prev_tail = Some(tail_by_tokens(&chunk_md, overlap_tokens));
                    current.clear();
                    current_tokens = 0;
                }
                for sub in hard_split(para, target_tokens) {
                    let chunk_md = finalize(&sub, &prev_tail);
                    let token_count = count_tokens(&chunk_md);
                    out.push(Chunk {
                        order,
                        section_anchor: section.anchor.clone(),
                        markdown: chunk_md.clone(),
                        token_count,
                    });
                    order += 1;
                    prev_tail = Some(tail_by_tokens(&chunk_md, overlap_tokens));
                }
                continue;
            }

            if current_tokens + para_tokens > target_tokens && !current.is_empty() {
                let chunk_md = finalize(&current, &prev_tail);
                let token_count = count_tokens(&chunk_md);
                out.push(Chunk {
                    order,
                    section_anchor: section.anchor.clone(),
                    markdown: chunk_md.clone(),
                    token_count,
                });
                order += 1;
                prev_tail = Some(tail_by_tokens(&chunk_md, overlap_tokens));
                current.clear();
                current_tokens = 0;
            }

            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);
            current_tokens += para_tokens;
        }

        if !current.is_empty() {
            let chunk_md = finalize(&current, &prev_tail);
            let token_count = count_tokens(&chunk_md);
            out.push(Chunk {
                order,
                section_anchor: section.anchor.clone(),
                markdown: chunk_md,
                token_count,
            });
            order += 1;
        }
    }

    out
}

/// Approximate token count.
///
/// **TODO:** swap to `tiktoken_rs::cl100k_base().encode_with_special_tokens(...)`
/// once `tiktoken-rs` is added to the workspace (Phase-3 plan § 7).
/// The current chars/4 heuristic matches cl100k_base to within ~10% on
/// English prose — good enough for chunking, not for billing.
pub fn count_tokens(s: &str) -> usize {
    (s.len() + 3) / 4
}

fn split_paragraphs(body: &str) -> Vec<String> {
    body.split("\n\n")
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn finalize(body: &str, prev_tail: &Option<String>) -> String {
    match prev_tail {
        Some(tail) if !tail.is_empty() => format!("{tail}\n\n{body}"),
        _ => body.to_string(),
    }
}

/// Return the trailing `~tokens` of `s`, snapped to a word boundary so we
/// don't slice mid-word in the overlap region.
fn tail_by_tokens(s: &str, tokens: usize) -> String {
    if tokens == 0 || s.is_empty() {
        return String::new();
    }
    let approx_chars = tokens.saturating_mul(4);
    if s.len() <= approx_chars {
        return s.to_string();
    }
    let start = s.len().saturating_sub(approx_chars);
    let safe_start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    let mut slice = &s[safe_start..];
    if let Some(space_idx) = slice.find(char::is_whitespace) {
        slice = &slice[space_idx..];
    }
    slice.trim_start().to_string()
}

/// Hard-split a single oversized paragraph into chunks of ~target_tokens.
/// Splits on whitespace.
fn hard_split(para: &str, target_tokens: usize) -> Vec<String> {
    let mut out = Vec::new();
    let target_chars = target_tokens.saturating_mul(4).max(1);

    let mut current = String::new();
    for word in para.split_whitespace() {
        if current.len() + 1 + word.len() > target_chars && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(anchor: &str, markdown: &str) -> Section {
        Section {
            heading: anchor.to_string(),
            anchor: anchor.to_string(),
            markdown: markdown.to_string(),
            order: 0,
        }
    }

    #[test]
    fn count_tokens_is_chars_over_four_rounded_up() {
        assert_eq!(count_tokens(""), 0);
        assert_eq!(count_tokens("a"), 1);
        assert_eq!(count_tokens("abcd"), 1);
        assert_eq!(count_tokens("abcde"), 2);
        assert_eq!(count_tokens(&"a".repeat(800)), 200);
    }

    #[test]
    fn small_section_becomes_one_chunk() {
        let secs = vec![section("intro", "hello world")];
        let chunks = chunk_sections(&secs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].order, 0);
        assert_eq!(chunks[0].section_anchor, "intro");
        assert_eq!(chunks[0].markdown, "hello world");
    }

    #[test]
    fn empty_section_is_skipped() {
        let secs = vec![
            section("blank", "   \n  \n "),
            section("real", "content here"),
        ];
        let chunks = chunk_sections(&secs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].section_anchor, "real");
    }

    #[test]
    fn multiple_sections_dont_merge() {
        let secs = vec![
            section("a", "alpha body"),
            section("b", "beta body"),
            section("c", "gamma body"),
        ];
        let chunks = chunk_sections(&secs);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].section_anchor, "a");
        assert_eq!(chunks[1].section_anchor, "b");
        assert_eq!(chunks[2].section_anchor, "c");
        assert_eq!(chunks[0].order, 0);
        assert_eq!(chunks[1].order, 1);
        assert_eq!(chunks[2].order, 2);
    }

    #[test]
    fn large_section_splits_on_paragraph_boundary() {
        let para = "x".repeat(100);
        let body = format!("{para}\n\n{para}\n\n{para}\n\n{para}\n\n{para}");
        let secs = vec![section("big", &body)];
        let chunks = chunk_sections_with(&secs, 50, 5);
        assert!(
            chunks.len() >= 2,
            "expected splits, got {} chunk(s)",
            chunks.len()
        );
        assert!(chunks.iter().all(|c| c.section_anchor == "big"));
    }

    #[test]
    fn overlap_carries_tail_of_previous_chunk() {
        let p1 = "ALPHA_PARAGRAPH_FILLER_FILLER_FILLER ".repeat(20);
        let p2 = "BRAVO_PARAGRAPH_FILLER_FILLER_FILLER ".repeat(20);
        let body = format!("{p1}\n\n{p2}");
        let secs = vec![section("two", &body)];
        let chunks = chunk_sections_with(&secs, 100, 30);
        assert!(chunks.len() >= 2);

        // Each paragraph exceeds the target, so it hard-splits into several
        // chunks. The property we care about is overlap *across the
        // paragraph boundary*: the first chunk that introduces BRAVO content
        // must carry an ALPHA overlap tail from the preceding chunk.
        let bravo_idx = chunks
            .iter()
            .position(|c| c.markdown.contains("BRAVO_PARAGRAPH_FILLER"))
            .expect("some chunk should contain BRAVO content");
        assert!(bravo_idx > 0, "BRAVO appeared in the very first chunk");
        assert!(
            chunks[bravo_idx].markdown.contains("ALPHA_PARAGRAPH_FILLER"),
            "overlap not carried into first BRAVO chunk = {:?}",
            chunks[bravo_idx].markdown
        );
    }

    #[test]
    fn paragraph_larger_than_target_is_hard_split() {
        let huge = "word ".repeat(2000);
        let secs = vec![section("huge", &huge)];
        let chunks = chunk_sections_with(&secs, 100, 0);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(
                c.token_count <= 200,
                "chunk {} has {} tokens (target 100)",
                c.order,
                c.token_count
            );
        }
    }

    #[test]
    fn order_is_monotonic_across_splits() {
        let para = "p".repeat(400);
        let body = format!("{para}\n\n{para}\n\n{para}");
        let secs = vec![section("a", &body), section("b", "tiny")];
        let chunks = chunk_sections_with(&secs, 50, 10);
        let mut prev = -1i64;
        for c in &chunks {
            assert!(
                (c.order as i64) > prev,
                "order not monotonic: prev={prev}, current={}",
                c.order
            );
            prev = c.order as i64;
        }
    }

    #[test]
    fn tail_by_tokens_respects_char_boundary_on_unicode() {
        let s = "lots of ascii prose 你好世界 more ascii ".repeat(20);
        let tail = tail_by_tokens(&s, 5);
        assert!(!tail.is_empty());
        let _ = tail.chars().count();
    }
}
