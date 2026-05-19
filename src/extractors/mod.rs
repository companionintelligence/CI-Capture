//! Document extractors that turn changed files into LLM-ready Markdown.
//!
//! **Skeleton — not yet wired into the binary.** See
//! [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) for the
//! design and `CHECKLIST.md` for the sub-PRs that land each extractor.
//!
//! Each format module owns one [`traits::Extractor`] impl. The router in
//! [`classify`] picks an extractor based on MIME + extension + sniffed bytes.

#![allow(dead_code)]

pub mod audio;
pub mod docx;
pub mod eml;
pub mod epub;
pub mod html;
pub mod image;
pub mod pdf;
pub mod pptx;
pub mod text;
pub mod traits;
pub mod xlsx;
pub mod zip;

use std::path::Path;

use self::traits::Extractor;

/// Look up an extractor for the given path. Returns `None` if the file type
/// is unsupported — the caller falls back to metadata-only ingest.
pub fn classify(_path: &Path, _mime_hint: Option<&str>) -> Option<Box<dyn Extractor>> {
    todo!("wire up in the router sub-PR; see CHECKLIST.md")
}
