//! Document extractors that turn changed files into LLM-ready Markdown.
//!
//! Each format module owns one [`traits::Extractor`] impl. The router
//! [`classify`] picks an extractor based on file extension (plus an
//! optional MIME hint).
//!
//! Currently shipping (testable on-device):
//! - Text family ([`text::TextExtractor`]): txt, md, json, csv, tsv
//! - HTML ([`html::HtmlExtractor`]): html, htm, xhtml
//! - Email ([`eml::EmlExtractor`]): eml
//! - EPUB ([`epub::EpubExtractor`]): epub
//!
//! Deferred (heavier dep / sidecar): docx, pptx, xlsx, pdf, image, audio,
//! zip — see [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md).

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

use self::eml::EmlExtractor;
use self::epub::EpubExtractor;
use self::html::HtmlExtractor;
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

    // Text family (txt, md, json, csv, tsv, log, jsonl, ndjson, markdown).
    if let Some(kind) = TextKind::from_extension(&ext) {
        return Some(Box::new(TextExtractor::new(kind)));
    }

    match ext.as_str() {
        "html" | "htm" | "xhtml" => Some(Box::new(HtmlExtractor::new())),
        "eml" => Some(Box::new(EmlExtractor::new())),
        "epub" => Some(Box::new(EpubExtractor::new())),
        _ => None,
    }
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
    fn classifies_html_family() {
        let html = classify(Path::new("/tmp/page.html"), None).unwrap();
        assert_eq!(html.name(), "html-extractor/0.1");
        assert!(classify(Path::new("/tmp/page.HTM"), None).is_some());
        assert!(classify(Path::new("/tmp/page.xhtml"), None).is_some());
    }

    #[test]
    fn classifies_eml() {
        let eml = classify(Path::new("/tmp/inbox.eml"), None).unwrap();
        assert_eq!(eml.name(), "eml-extractor/0.1");
    }

    #[test]
    fn classifies_epub() {
        let epub = classify(Path::new("/tmp/book.epub"), None).unwrap();
        assert_eq!(epub.name(), "epub-extractor/0.1");
    }

    #[test]
    fn unsupported_returns_none() {
        // Formats not yet shipping — these become Some(_) when their
        // extractors land.
        assert!(classify(Path::new("/tmp/song.mp3"), None).is_none());
        assert!(classify(Path::new("/tmp/deck.pptx"), None).is_none());
        assert!(classify(Path::new("/tmp/doc.docx"), None).is_none());
        assert!(classify(Path::new("/tmp/sheet.xlsx"), None).is_none());
        assert!(classify(Path::new("/tmp/scan.pdf"), None).is_none());
        assert!(classify(Path::new("/tmp/whatever"), None).is_none());
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert!(classify(Path::new("/tmp/Notes.TXT"), None).is_some());
        assert!(classify(Path::new("/tmp/Data.JSON"), None).is_some());
        assert!(classify(Path::new("/tmp/Page.HTML"), None).is_some());
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
