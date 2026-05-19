//! Plain-text family: txt, json, md.
//!
//! Three of the formats listed in [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 4
//! land in this module because they share the same shape — the file body
//! becomes the Markdown body, with light per-format wrapping. CSV / XML
//! arrive in follow-up sub-PRs (they need extra crates).
//!
//! All three extractors are pure-Rust, dep-free beyond `serde_json` (already
//! in the workspace) and `chrono` (already in the workspace).

use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use uuid::Uuid;

use super::traits::{
    ExtractOptions, ExtractedDocument, ExtractionDiagnostics, Extractor, Section,
};

// ── Detect ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Txt,
    Markdown,
    Json,
}

impl TextKind {
    /// Pick a [`TextKind`] from the file extension. Returns `None` for
    /// non-text-family extensions.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "txt" | "log" => Some(Self::Txt),
            "md" | "markdown" => Some(Self::Markdown),
            "json" | "ndjson" | "jsonl" => Some(Self::Json),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Txt => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Json => "application/json",
        }
    }

    fn extractor_name(self) -> &'static str {
        match self {
            Self::Txt => "text-extractor/txt/0.1",
            Self::Markdown => "text-extractor/markdown/0.1",
            Self::Json => "text-extractor/json/0.1",
        }
    }
}

// ── Extractor ─────────────────────────────────────────────────────────────────

pub struct TextExtractor {
    kind: TextKind,
}

impl TextExtractor {
    pub fn new(kind: TextKind) -> Self {
        Self { kind }
    }
}

impl Extractor for TextExtractor {
    fn name(&self) -> &'static str {
        self.kind.extractor_name()
    }

    fn extract(&self, path: &Path, opts: ExtractOptions) -> Result<ExtractedDocument> {
        let started = Instant::now();

        let title = filename_stem(path);
        let metadata = base_metadata(path, self.kind);

        if opts.metadata_only {
            // No body content — caller asked for metadata-only ingest.
            return Ok(empty_doc(path, self.kind, title, metadata, started));
        }

        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;

        let (markdown, sections) = match self.kind {
            TextKind::Txt => render_txt(&title, &raw),
            TextKind::Markdown => render_markdown_passthrough(&title, &raw),
            TextKind::Json => render_json(&title, &raw),
        };

        Ok(ExtractedDocument {
            document_id: Uuid::new_v4().to_string(),
            file_id: String::new(),
            version_id: String::new(),
            source_path: path.display().to_string(),
            content_type: self.kind.content_type().to_string(),
            markdown,
            sections,
            metadata,
            diagnostics: ExtractionDiagnostics {
                extractor: self.kind.extractor_name().to_string(),
                ocr_used: false,
                transcription_used: false,
                warnings: Vec::new(),
                extraction_time_ms: started.elapsed().as_millis() as u64,
                confidence: Some(1.0),
            },
        })
    }
}

// ── Per-format rendering ──────────────────────────────────────────────────────

fn render_txt(title: &str, body: &str) -> (String, Vec<Section>) {
    let trimmed = body.trim_end_matches('\n');
    let markdown = format!("# {title}\n\n```\n{trimmed}\n```\n");
    let sections = vec![Section {
        heading: title.to_string(),
        anchor: anchorize(title),
        markdown: markdown.clone(),
        order: 0,
    }];
    (markdown, sections)
}

fn render_markdown_passthrough(title: &str, body: &str) -> (String, Vec<Section>) {
    // Markdown files are already structured; pass through as-is. We still
    // populate a synthetic top-level section so downstream chunking has a
    // boundary to work with.
    let sections = vec![Section {
        heading: title.to_string(),
        anchor: anchorize(title),
        markdown: body.to_string(),
        order: 0,
    }];
    (body.to_string(), sections)
}

fn render_json(title: &str, body: &str) -> (String, Vec<Section>) {
    // Try to pretty-print so the embedded code block is human-readable.
    // If parsing fails (malformed JSON), fall back to the raw body — we'd
    // rather ship "something" than refuse the file.
    let pretty = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| body.to_string()),
        Err(_) => body.to_string(),
    };
    let markdown = format!("# {title}\n\n```json\n{pretty}\n```\n");
    let sections = vec![Section {
        heading: title.to_string(),
        anchor: anchorize(title),
        markdown: markdown.clone(),
        order: 0,
    }];
    (markdown, sections)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn filename_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "document".to_string())
}

fn base_metadata(
    path: &Path,
    kind: TextKind,
) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "title".to_string(),
        serde_json::Value::String(filename_stem(path)),
    );
    m.insert(
        "format".to_string(),
        serde_json::Value::String(match kind {
            TextKind::Txt => "txt",
            TextKind::Markdown => "markdown",
            TextKind::Json => "json",
        }
        .to_string()),
    );
    m
}

fn empty_doc(
    path: &Path,
    kind: TextKind,
    title: String,
    metadata: serde_json::Map<String, serde_json::Value>,
    started: Instant,
) -> ExtractedDocument {
    ExtractedDocument {
        document_id: Uuid::new_v4().to_string(),
        file_id: String::new(),
        version_id: String::new(),
        source_path: path.display().to_string(),
        content_type: kind.content_type().to_string(),
        markdown: String::new(),
        sections: vec![Section {
            heading: title.clone(),
            anchor: anchorize(&title),
            markdown: String::new(),
            order: 0,
        }],
        metadata,
        diagnostics: ExtractionDiagnostics {
            extractor: kind.extractor_name().to_string(),
            ocr_used: false,
            transcription_used: false,
            warnings: vec!["metadata_only".to_string()],
            extraction_time_ms: started.elapsed().as_millis() as u64,
            confidence: Some(1.0),
        },
    }
}

/// GitHub-flavored slugifier: lowercase, non-alphanumerics → `-`, collapse
/// runs, trim. Used for `Section.anchor`.
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_named(name: &str, body: &[u8]) -> NamedTempFile {
        // tempfile crate doesn't give us control over the extension on
        // NamedTempFile::new, but we can use the `Builder` for that.
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{name}"))
            .tempfile()
            .expect("tempfile");
        f.write_all(body).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn text_kind_from_extension() {
        assert_eq!(TextKind::from_extension("txt"), Some(TextKind::Txt));
        assert_eq!(TextKind::from_extension("TXT"), Some(TextKind::Txt));
        assert_eq!(TextKind::from_extension("log"), Some(TextKind::Txt));
        assert_eq!(TextKind::from_extension("md"), Some(TextKind::Markdown));
        assert_eq!(TextKind::from_extension("markdown"), Some(TextKind::Markdown));
        assert_eq!(TextKind::from_extension("json"), Some(TextKind::Json));
        assert_eq!(TextKind::from_extension("jsonl"), Some(TextKind::Json));
        assert_eq!(TextKind::from_extension("pdf"), None);
    }

    #[test]
    fn txt_wraps_body_in_fence() {
        let f = write_named("txt", b"hello\nworld\n");
        let ex = TextExtractor::new(TextKind::Txt);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert_eq!(doc.content_type, "text/plain");
        assert!(doc.markdown.starts_with("# "));
        assert!(doc.markdown.contains("```\nhello\nworld\n```"));
        assert_eq!(doc.sections.len(), 1);
        assert!(!doc.diagnostics.ocr_used);
    }

    #[test]
    fn markdown_is_passed_through() {
        let body = "# Real heading\n\nA paragraph.\n";
        let f = write_named("md", body.as_bytes());
        let ex = TextExtractor::new(TextKind::Markdown);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert_eq!(doc.markdown, body);
        assert_eq!(doc.content_type, "text/markdown");
        // No double-wrapping in a code fence.
        assert!(!doc.markdown.contains("```"));
    }

    #[test]
    fn json_is_pretty_printed_in_code_fence() {
        let body = br#"{"a":1,"b":[2,3]}"#;
        let f = write_named("json", body);
        let ex = TextExtractor::new(TextKind::Json);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert_eq!(doc.content_type, "application/json");
        assert!(doc.markdown.contains("```json"));
        // Pretty-printed output has a newline between the keys.
        assert!(doc.markdown.contains("\"a\": 1"));
        assert!(doc.markdown.contains("\"b\": ["));
    }

    #[test]
    fn malformed_json_falls_back_to_raw_body() {
        let body = b"{this is not, json}";
        let f = write_named("json", body);
        let ex = TextExtractor::new(TextKind::Json);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        // We shipped the file rather than refusing it.
        assert!(doc.markdown.contains("{this is not, json}"));
    }

    #[test]
    fn metadata_only_yields_empty_body() {
        let f = write_named("txt", b"sensitive content do not ship");
        let ex = TextExtractor::new(TextKind::Txt);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: true,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert_eq!(doc.markdown, "");
        // Section still exists for downstream uniformity, but its body is empty.
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].markdown, "");
        // And we flagged it.
        assert!(doc.diagnostics.warnings.contains(&"metadata_only".to_string()));
    }

    #[test]
    fn metadata_includes_title_and_format() {
        let f = write_named("md", b"# title\n");
        let ex = TextExtractor::new(TextKind::Markdown);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert!(doc.metadata.contains_key("title"));
        assert_eq!(
            doc.metadata.get("format"),
            Some(&serde_json::Value::String("markdown".to_string()))
        );
    }

    #[test]
    fn anchorize_handles_common_cases() {
        assert_eq!(anchorize("Hello World"), "hello-world");
        assert_eq!(anchorize("  Tabs\tand   spaces  "), "tabs-and-spaces");
        assert_eq!(anchorize("!!!"), "");
        assert_eq!(anchorize("CES 2026: keynote"), "ces-2026-keynote");
    }

    #[test]
    fn missing_file_is_an_error() {
        let ex = TextExtractor::new(TextKind::Txt);
        let err = ex
            .extract(
                Path::new("/this/path/should/not/exist-3a8f"),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap_err();
        // The path appears in the wrapped error context.
        assert!(err.to_string().contains("/this/path/"));
    }
}
