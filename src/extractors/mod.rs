//! Document extractors that turn changed files into LLM-ready Markdown.
//!
//! Each format module owns one [`traits::Extractor`] impl. The router
//! [`classify`] picks an extractor based on file extension (plus an
//! optional MIME hint).
//!
//! Today only the text family ([`text::TextExtractor`]) ships with a
//! working implementation. Every other format slot exists as a doc-
//! comment stub waiting for its sub-PR per
//! [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md).

#![allow(dead_code)]

pub mod audio;
pub mod docx;
pub mod image;
pub mod pdf;
pub mod pptx;
pub mod text;
pub mod traits;
pub mod xlsx;
pub mod zip;

// Rich extractors pull heavier third-party parsers; opt in with
// `--features rich-extractors`.
#[cfg(feature = "rich-extractors")]
pub mod eml;
#[cfg(feature = "rich-extractors")]
pub mod epub;
#[cfg(feature = "rich-extractors")]
pub mod html;

use std::path::Path;

use self::text::{TextExtractor, TextKind};
use self::traits::Extractor;

/// Look up an extractor for the given path.
///
/// The decision uses:
/// - File extension (lowercased, stripped of leading dot).
/// - `mime_hint`, if the caller already sniffed one. Currently unused
///   beyond logging hooks, but the signature is in place so MIME-based
///   override can land without a router rewrite.
///
/// Returns `None` for unsupported formats — the caller should fall back
/// to metadata-only ingest in that case.
pub fn classify(path: &Path, _mime_hint: Option<&str>) -> Option<Box<dyn Extractor>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    // Rich formats win for the extensions they specialize in (e.g. html2md
    // produces real Markdown from HTML, better than a fenced-code fallback).
    // Only compiled in with `--features rich-extractors`.
    #[cfg(feature = "rich-extractors")]
    {
        match ext.as_str() {
            "html" | "htm" | "xhtml" => return Some(Box::new(self::html::HtmlExtractor::new())),
            "eml" => return Some(Box::new(self::eml::EmlExtractor::new())),
            "epub" => return Some(Box::new(self::epub::EpubExtractor::new())),
            _ => {}
        }
    }

    // Text family (txt/md/json/csv/tsv/code) — the formats users actually
    // edit. Always available. Without the rich-extractors feature, html/xml
    // fall through here and render as fenced code.
    if let Some(kind) = TextKind::from_extension(&ext) {
        return Some(Box::new(TextExtractor::new(kind)));
    }

    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn classifies_text_family() {
        assert!(classify(Path::new("/tmp/notes.txt"), None).is_some());
        assert!(classify(Path::new("/tmp/readme.md"), None).is_some());
        assert!(classify(Path::new("/tmp/data.json"), None).is_some());
        assert!(classify(Path::new("/tmp/log.LOG"), None).is_some());
        assert!(classify(Path::new("/tmp/rows.csv"), None).is_some());
        assert!(classify(Path::new("/tmp/rows.tsv"), None).is_some());
    }

    #[test]
    fn unsupported_returns_none() {
        assert!(classify(Path::new("/tmp/song.mp3"), None).is_none());
        assert!(classify(Path::new("/tmp/deck.pptx"), None).is_none());
        assert!(classify(Path::new("/tmp/whatever"), None).is_none());
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert!(classify(Path::new("/tmp/Notes.TXT"), None).is_some());
        assert!(classify(Path::new("/tmp/Data.JSON"), None).is_some());
    }

    #[test]
    fn picks_the_right_kind_name() {
        let txt = classify(Path::new("/tmp/a.txt"), None).unwrap();
        assert_eq!(txt.name(), "text-extractor/txt/0.1");
        let md = classify(Path::new("/tmp/a.md"), None).unwrap();
        assert_eq!(md.name(), "text-extractor/markdown/0.1");
        let json = classify(Path::new("/tmp/a.json"), None).unwrap();
        assert_eq!(json.name(), "text-extractor/json/0.1");
        let csv = classify(Path::new("/tmp/a.csv"), None).unwrap();
        assert_eq!(csv.name(), "text-extractor/csv/0.1");
    }
}
