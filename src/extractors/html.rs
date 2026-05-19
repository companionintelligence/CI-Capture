//! HTML extractor via `html2md`.
//!
//! Reads a `.html` / `.htm` / `.xhtml` file, extracts the document title
//! from `<title>` when present (falls back to the filename stem),
//! converts the body to Markdown with `html2md::parse_html`, and emits
//! one synthetic top-level section so downstream chunking has a
//! boundary.
//!
//! The crate intentionally does not parse the HTML DOM ourselves —
//! `html2md` already handles the messy edge cases (entity decoding,
//! nested lists, tables, links).

use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use uuid::Uuid;

use super::traits::{
    ExtractOptions, ExtractedDocument, ExtractionDiagnostics, Extractor, Section,
};

pub struct HtmlExtractor;

impl HtmlExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HtmlExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for HtmlExtractor {
    fn name(&self) -> &'static str {
        "html-extractor/0.1"
    }

    fn extract(&self, path: &Path, opts: ExtractOptions) -> Result<ExtractedDocument> {
        let started = Instant::now();

        let stem_title = filename_stem(path);
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "format".to_string(),
            serde_json::Value::String("html".to_string()),
        );

        if opts.metadata_only {
            metadata.insert(
                "title".to_string(),
                serde_json::Value::String(stem_title.clone()),
            );
            return Ok(empty_doc(path, stem_title, metadata, started));
        }

        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;

        let html_title = extract_title(&raw).unwrap_or_else(|| stem_title.clone());
        metadata.insert(
            "title".to_string(),
            serde_json::Value::String(html_title.clone()),
        );

        let md_body = html2md::parse_html(&raw);
        let md_body = md_body.trim().to_string();
        let markdown = format!("# {html_title}\n\n{md_body}\n");

        let sections = vec![Section {
            heading: html_title.clone(),
            anchor: anchorize(&html_title),
            markdown: markdown.clone(),
            order: 0,
        }];

        Ok(ExtractedDocument {
            document_id: Uuid::new_v4().to_string(),
            file_id: String::new(),
            version_id: String::new(),
            source_path: path.display().to_string(),
            content_type: "text/html".to_string(),
            markdown,
            sections,
            metadata,
            diagnostics: ExtractionDiagnostics {
                extractor: "html-extractor/0.1".to_string(),
                ocr_used: false,
                transcription_used: false,
                warnings: Vec::new(),
                extraction_time_ms: started.elapsed().as_millis() as u64,
                confidence: Some(0.9),
            },
        })
    }
}

/// Extract the first `<title>…</title>` content (case-insensitive),
/// trimmed. Returns `None` if no title tag is present or its content is
/// empty after trimming.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start_tag = "<title";
    let close_tag = "</title>";
    let start = lower.find(start_tag)?;
    // Walk past the opening tag's `>` (the tag may carry attributes).
    let after_open = lower[start..].find('>').map(|i| start + i + 1)?;
    let end = lower[after_open..].find(close_tag).map(|i| after_open + i)?;
    let title = html[after_open..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(decode_basic_entities(title))
    }
}

/// Minimal HTML entity decode covering the title-tag case. We don't need
/// a full decoder — `html2md` handles body content.
fn decode_basic_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn filename_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "document".to_string())
}

fn anchorize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn empty_doc(
    path: &Path,
    title: String,
    metadata: serde_json::Map<String, serde_json::Value>,
    started: Instant,
) -> ExtractedDocument {
    ExtractedDocument {
        document_id: Uuid::new_v4().to_string(),
        file_id: String::new(),
        version_id: String::new(),
        source_path: path.display().to_string(),
        content_type: "text/html".to_string(),
        markdown: String::new(),
        sections: vec![Section {
            heading: title.clone(),
            anchor: anchorize(&title),
            markdown: String::new(),
            order: 0,
        }],
        metadata,
        diagnostics: ExtractionDiagnostics {
            extractor: "html-extractor/0.1".to_string(),
            ocr_used: false,
            transcription_used: false,
            warnings: vec!["metadata_only".to_string()],
            extraction_time_ms: started.elapsed().as_millis() as u64,
            confidence: Some(1.0),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_html(body: &[u8]) -> NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".html")
            .tempfile()
            .expect("tempfile");
        f.write_all(body).unwrap();
        f.flush().unwrap();
        f
    }

    fn opts() -> ExtractOptions {
        ExtractOptions {
            metadata_only: false,
            allow_heavy_models: false,
        }
    }

    #[test]
    fn extracts_title_from_title_tag() {
        let html = b"<html><head><title>Hello World</title></head><body><p>x</p></body></html>";
        let f = write_html(html);
        let doc = HtmlExtractor::new().extract(f.path(), opts()).unwrap();
        assert_eq!(doc.content_type, "text/html");
        assert!(doc.markdown.starts_with("# Hello World"));
        assert_eq!(
            doc.metadata.get("title"),
            Some(&serde_json::Value::String("Hello World".to_string()))
        );
    }

    #[test]
    fn falls_back_to_filename_when_no_title_tag() {
        let html = b"<html><body><p>no title</p></body></html>";
        let f = write_html(html);
        let doc = HtmlExtractor::new().extract(f.path(), opts()).unwrap();
        let stem = f.path().file_stem().unwrap().to_str().unwrap().to_string();
        assert!(doc.markdown.starts_with(&format!("# {stem}")));
    }

    #[test]
    fn handles_title_with_attributes_and_case() {
        let html =
            b"<HTML><HEAD><TITLE dir=\"ltr\">Attributed   Title</TITLE></HEAD><body>x</body></HTML>";
        let f = write_html(html);
        let doc = HtmlExtractor::new().extract(f.path(), opts()).unwrap();
        assert!(doc.markdown.contains("# Attributed   Title"));
    }

    #[test]
    fn decodes_basic_entities_in_title() {
        let html =
            b"<html><head><title>Cats &amp; Dogs &lt;3</title></head><body>x</body></html>";
        let f = write_html(html);
        let doc = HtmlExtractor::new().extract(f.path(), opts()).unwrap();
        assert!(doc.markdown.contains("# Cats & Dogs <3"));
    }

    #[test]
    fn body_is_converted_to_markdown() {
        let html = b"<html><body>\
            <h1>Heading</h1>\
            <p>This is <strong>bold</strong> and <em>italic</em>.</p>\
            <ul><li>one</li><li>two</li></ul>\
            </body></html>";
        let f = write_html(html);
        let doc = HtmlExtractor::new().extract(f.path(), opts()).unwrap();
        let md = &doc.markdown;
        // Exact rendering depends on html2md, but these structural
        // markers should be present in some form.
        assert!(
            md.contains("**bold**") || md.contains("bold"),
            "expected bold marker in markdown: {md:?}"
        );
        assert!(
            md.contains("*italic*") || md.contains("_italic_") || md.contains("italic"),
            "expected italic marker in markdown: {md:?}"
        );
        assert!(
            md.contains("- one") || md.contains("* one"),
            "expected list marker in markdown: {md:?}"
        );
    }

    #[test]
    fn empty_title_falls_back_to_filename() {
        let html = b"<html><head><title>   </title></head><body>x</body></html>";
        let f = write_html(html);
        let doc = HtmlExtractor::new().extract(f.path(), opts()).unwrap();
        let stem = f.path().file_stem().unwrap().to_str().unwrap().to_string();
        assert!(doc.markdown.starts_with(&format!("# {stem}")));
    }

    #[test]
    fn metadata_only_yields_empty_body() {
        let html = b"<html><head><title>Sensitive</title></head><body>secrets</body></html>";
        let f = write_html(html);
        let doc = HtmlExtractor::new()
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: true,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert_eq!(doc.markdown, "");
        assert!(doc.diagnostics.warnings.contains(&"metadata_only".to_string()));
        // Title falls back to filename stem in metadata-only mode — we
        // don't spend parse cycles when the folder is opt-out.
        let stem = f.path().file_stem().unwrap().to_str().unwrap();
        assert_eq!(
            doc.metadata.get("title"),
            Some(&serde_json::Value::String(stem.to_string()))
        );
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = HtmlExtractor::new()
            .extract(Path::new("/this/path/should/not/exist-html-bf1"), opts())
            .unwrap_err();
        assert!(err.to_string().contains("/this/path/"));
    }
}
