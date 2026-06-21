//! EPUB extractor via the `epub` crate.
//!
//! Walks the spine in reading order. Each spine entry becomes one
//! [`Section`]; the body content is XHTML which we strip to plain text
//! (the inline `strip_html_tags` helper here mirrors the one in
//! `eml.rs` so this PR doesn't take a runtime dep on `html2md`).
//!
//! Metadata extracted via `epub::doc::EpubDoc::mdata`:
//! - `title` → frontmatter `title` (falls back to filename stem)
//! - `creator` → frontmatter `author`
//! - `language` → frontmatter `language`
//! - `publisher` → frontmatter `publisher`
//! - `date` → frontmatter `date`
//!
//! See [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 4.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use uuid::Uuid;

use super::traits::{
    ExtractOptions, ExtractedDocument, ExtractionDiagnostics, Extractor, Section,
};

pub struct EpubExtractor;

impl EpubExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EpubExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for EpubExtractor {
    fn name(&self) -> &'static str {
        "epub-extractor/0.1"
    }

    fn extract(&self, path: &Path, opts: ExtractOptions) -> Result<ExtractedDocument> {
        let started = Instant::now();

        let stem_title = filename_stem(path);
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "format".to_string(),
            serde_json::Value::String("epub".to_string()),
        );

        if opts.metadata_only {
            metadata.insert(
                "title".to_string(),
                serde_json::Value::String(stem_title.clone()),
            );
            return Ok(empty_doc(path, stem_title, metadata, started));
        }

        let mut doc = epub::doc::EpubDoc::new(path)
            .map_err(|e| anyhow!("opening EPUB at {}: {e}", path.display()))?;

        let title = doc
            .mdata("title")
            .map(|m| m.value.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| stem_title.clone());
        metadata.insert(
            "title".to_string(),
            serde_json::Value::String(title.clone()),
        );
        for (epub_key, our_key) in &[
            ("creator", "author"),
            ("language", "language"),
            ("publisher", "publisher"),
            ("date", "date"),
            ("description", "description"),
        ] {
            if let Some(item) = doc.mdata(epub_key) {
                let v = item.value.trim();
                if !v.is_empty() {
                    metadata.insert(
                        our_key.to_string(),
                        serde_json::Value::String(v.to_string()),
                    );
                }
            }
        }

        let total_chapters = doc.get_num_chapters();
        let mut sections: Vec<Section> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for i in 0..total_chapters {
            if !doc.set_current_chapter(i) {
                warnings.push(format!("set_current_chapter({i}) failed"));
                continue;
            }
            let (raw, _mime) = match doc.get_current_str() {
                Some(x) => x,
                None => {
                    warnings.push(format!("page {i}: no content"));
                    continue;
                }
            };

            let body = strip_html_tags(&raw);
            if body.is_empty() {
                continue;
            }

            let heading = chapter_heading(&raw, i + 1);
            sections.push(Section {
                heading: heading.clone(),
                anchor: anchorize(&heading),
                markdown: format!("## {heading}\n\n{body}\n"),
                order: sections.len() as u32,
            });
        }

        // Assemble final markdown: top-level title + per-chapter sections.
        let mut markdown = String::new();
        markdown.push_str(&format!("# {title}\n\n"));
        for sec in &sections {
            markdown.push_str(&sec.markdown);
            markdown.push('\n');
        }

        Ok(ExtractedDocument {
            document_id: Uuid::new_v4().to_string(),
            file_id: String::new(),
            version_id: String::new(),
            source_path: path.display().to_string(),
            content_type: "application/epub+zip".to_string(),
            markdown,
            sections,
            metadata,
            diagnostics: ExtractionDiagnostics {
                extractor: "epub-extractor/0.1".to_string(),
                ocr_used: false,
                transcription_used: false,
                warnings,
                extraction_time_ms: started.elapsed().as_millis() as u64,
                confidence: Some(0.9),
            },
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Best-effort chapter title — peek the first `<h1>`/`<h2>`/`<h3>` in
/// the XHTML; fall back to `Chapter N`.
fn chapter_heading(xhtml: &str, default_n: usize) -> String {
    for tag in &["h1", "h2", "h3", "title"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let lower = xhtml.to_ascii_lowercase();
        if let Some(start) = lower.find(&open) {
            // Walk past the opening tag's `>` (the tag may carry attributes).
            if let Some(rel) = lower[start..].find('>') {
                let after_open = start + rel + 1;
                if let Some(end_rel) = lower[after_open..].find(&close) {
                    let end = after_open + end_rel;
                    let text = strip_html_tags(&xhtml[after_open..end]);
                    let text = text.trim();
                    if !text.is_empty() {
                        return text.to_string();
                    }
                }
            }
        }
    }
    format!("Chapter {default_n}")
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    let mut collapsed = String::with_capacity(decoded.len());
    let mut last_ws = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if !last_ws && !collapsed.is_empty() {
                collapsed.push(' ');
            }
            last_ws = true;
        } else {
            collapsed.push(ch);
            last_ws = false;
        }
    }
    collapsed.trim().to_string()
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
        content_type: "application/epub+zip".to_string(),
        markdown: String::new(),
        sections: vec![Section {
            heading: title.clone(),
            anchor: anchorize(&title),
            markdown: String::new(),
            order: 0,
        }],
        metadata,
        diagnostics: ExtractionDiagnostics {
            extractor: "epub-extractor/0.1".to_string(),
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
    use super::*;

    #[test]
    fn strip_html_tags_basic() {
        let s = strip_html_tags("<p>Hello   <b>world</b>!</p>");
        assert_eq!(s, "Hello world!");
    }

    #[test]
    fn strip_html_tags_decodes_entities() {
        let s = strip_html_tags("<p>Cats &amp; Dogs &lt;3&gt;</p>");
        assert_eq!(s, "Cats & Dogs <3>");
    }

    #[test]
    fn strip_html_tags_collapses_runs_of_whitespace() {
        let s = strip_html_tags("a\n\n\n  b\t\tc");
        assert_eq!(s, "a b c");
    }

    #[test]
    fn chapter_heading_prefers_h1() {
        let xhtml =
            "<html><body><h1>First Heading</h1><p>body</p><h2>Section</h2></body></html>";
        assert_eq!(chapter_heading(xhtml, 1), "First Heading");
    }

    #[test]
    fn chapter_heading_falls_back_to_h2_then_h3_then_title() {
        let xhtml_h2 = "<html><body><h2>An H2</h2><p>body</p></body></html>";
        assert_eq!(chapter_heading(xhtml_h2, 1), "An H2");

        let xhtml_h3 = "<html><body><h3>An H3</h3><p>body</p></body></html>";
        assert_eq!(chapter_heading(xhtml_h3, 1), "An H3");

        let xhtml_title = "<html><head><title>Doc Title</title></head><body>x</body></html>";
        assert_eq!(chapter_heading(xhtml_title, 1), "Doc Title");
    }

    #[test]
    fn chapter_heading_defaults_to_chapter_n() {
        let xhtml = "<html><body><p>no heading anywhere</p></body></html>";
        assert_eq!(chapter_heading(xhtml, 7), "Chapter 7");
    }

    #[test]
    fn chapter_heading_tolerates_attributes_and_case() {
        let xhtml = "<HTML><BODY><H1 class='c'>Attributed</H1></BODY></HTML>";
        assert_eq!(chapter_heading(xhtml, 1), "Attributed");
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = EpubExtractor::new()
            .extract(
                Path::new("/this/path/should/not/exist-epub-7c"),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("epub"));
    }

    #[test]
    fn anchorize_handles_common_cases() {
        assert_eq!(anchorize("Chapter 1: Opening"), "chapter-1-opening");
        assert_eq!(anchorize("---weird---"), "weird");
        assert_eq!(anchorize("!!!"), "");
    }

    #[test]
    fn filename_stem_extracts_stem() {
        assert_eq!(filename_stem(Path::new("/x/foo.epub")), "foo");
        assert_eq!(filename_stem(Path::new("noext")), "noext");
    }
}
