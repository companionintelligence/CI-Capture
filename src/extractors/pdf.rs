//! PDF extractor: `pdfium-render` text pass; OCR fallback per-page when
//! extracted text density is below a threshold.
//!
//! See [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 4.

#![allow(dead_code)]

/// Characters of extracted text per page below which we treat the page as
/// image-only and run OCR on it.
pub const OCR_TRIGGER_CHARS_PER_PAGE: usize = 50;
