//! Plain-text family: txt, markdown, json, csv, tsv.
//!
//! Five of the formats listed in [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 4
//! land in this module because they share the same shape — the file body
//! becomes the Markdown body, with light per-format wrapping. XML arrives
//! in a follow-up sub-PR.
//!
//! Runtime deps: `serde_json` (workspace), `csv` (added for CSV/TSV).

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
    Csv,
    Tsv,
    /// Source code / config files. Rendered as a language-fenced block so
    /// the AI sees the file as code, not prose. The language tag is derived
    /// from the extension at render time.
    Code,
}

impl TextKind {
    /// Pick a [`TextKind`] from the file extension. Returns `None` for
    /// non-text-family extensions.
    pub fn from_extension(ext: &str) -> Option<Self> {
        let lower = ext.to_ascii_lowercase();
        match lower.as_str() {
            "txt" | "log" | "text" => Some(Self::Txt),
            "md" | "markdown" | "mdown" | "mkd" => Some(Self::Markdown),
            "json" | "ndjson" | "jsonl" => Some(Self::Json),
            "csv" => Some(Self::Csv),
            "tsv" => Some(Self::Tsv),
            _ if language_for_extension(&lower).is_some() => Some(Self::Code),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Txt => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Json => "application/json",
            Self::Csv => "text/csv",
            Self::Tsv => "text/tab-separated-values",
            Self::Code => "text/plain",
        }
    }

    fn extractor_name(self) -> &'static str {
        match self {
            Self::Txt => "text-extractor/txt/0.1",
            Self::Markdown => "text-extractor/markdown/0.1",
            Self::Json => "text-extractor/json/0.1",
            Self::Csv => "text-extractor/csv/0.1",
            Self::Tsv => "text-extractor/tsv/0.1",
            Self::Code => "text-extractor/code/0.1",
        }
    }

    fn format_tag(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Code => "code",
        }
    }

    fn csv_delimiter(self) -> Option<u8> {
        match self {
            Self::Csv => Some(b','),
            Self::Tsv => Some(b'\t'),
            _ => None,
        }
    }
}

/// Map a (lowercased) file extension to a Markdown code-fence language tag.
/// `Some(..)` means "this is a source/config file the text extractor can
/// render as a fenced code block." Covers the languages and config formats
/// developers edit day-to-day.
pub fn language_for_extension(ext: &str) -> Option<&'static str> {
    let lang = match ext {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "clj" | "cljs" => "clojure",
        "sql" => "sql",
        "proto" => "protobuf",
        "graphql" | "gql" => "graphql",
        // Markup / config the AI benefits from seeing verbatim.
        "html" | "htm" | "xhtml" => "html",
        "xml" | "svg" | "plist" => "xml",
        "css" => "css",
        "scss" | "sass" => "scss",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "ini" | "cfg" | "conf" | "properties" | "env" => "ini",
        "dockerfile" => "dockerfile",
        "makefile" | "mk" => "makefile",
        "tf" | "tfvars" => "hcl",
        _ => return None,
    };
    Some(lang)
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
            TextKind::Csv | TextKind::Tsv => {
                let delim = self.kind.csv_delimiter().unwrap_or(b',');
                render_csv(&title, &raw, delim)
            }
            TextKind::Code => {
                let lang = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .as_deref()
                    .and_then(language_for_extension)
                    .unwrap_or("");
                render_code(&title, &raw, lang)
            }
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

fn render_code(title: &str, body: &str, lang: &str) -> (String, Vec<Section>) {
    let trimmed = body.trim_end_matches('\n');
    let markdown = format!("# {title}\n\n```{lang}\n{trimmed}\n```\n");
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

/// Maximum rows to render in a CSV table. Larger files are sampled:
/// first half + last half, with a `…` row in the middle.
const CSV_MAX_RENDERED_ROWS: usize = 100;

fn render_csv(title: &str, body: &str, delim: u8) -> (String, Vec<Section>) {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delim)
        .has_headers(true)
        .flexible(true)
        .from_reader(body.as_bytes());

    // Headers — fall back to numeric column labels if the file has none
    // (or the first record was unreadable).
    let headers: Vec<String> = match reader.headers() {
        Ok(rec) => rec.iter().map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    };

    // Collect all rows up to a sane upper bound so we know whether to
    // sample.
    let mut rows: Vec<Vec<String>> = Vec::new();
    for result in reader.records() {
        match result {
            Ok(rec) => {
                rows.push(rec.iter().map(|s| s.to_string()).collect());
                // Avoid blowing memory on multi-GB CSVs. The conversion
                // pool already gates total file size, but defense in
                // depth is cheap.
                if rows.len() > CSV_MAX_RENDERED_ROWS * 10 {
                    break;
                }
            }
            Err(_) => continue, // skip malformed row, keep going
        }
    }

    let total_rows = rows.len();
    let sampled = total_rows > CSV_MAX_RENDERED_ROWS;
    let cols = headers.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0));

    // Synthesize column headers if the file had none.
    let header_cells: Vec<String> = if !headers.is_empty() {
        headers
    } else {
        (1..=cols).map(|i| format!("col_{i}")).collect()
    };

    let mut md = String::new();
    md.push_str(&format!("# {title}\n\n"));
    if sampled {
        md.push_str(&format!(
            "_{total_rows} rows total; showing first/last {} each._\n\n",
            CSV_MAX_RENDERED_ROWS / 2
        ));
    }

    // Header line
    md.push('|');
    for h in &header_cells {
        md.push(' ');
        md.push_str(&escape_table_cell(h));
        md.push_str(" |");
    }
    md.push('\n');
    md.push('|');
    for _ in &header_cells {
        md.push_str(" --- |");
    }
    md.push('\n');

    let render_row = |out: &mut String, row: &[String]| {
        out.push('|');
        for i in 0..header_cells.len() {
            out.push(' ');
            out.push_str(&escape_table_cell(row.get(i).map(String::as_str).unwrap_or("")));
            out.push_str(" |");
        }
        out.push('\n');
    };

    if sampled {
        let half = CSV_MAX_RENDERED_ROWS / 2;
        for row in rows.iter().take(half) {
            render_row(&mut md, row);
        }
        md.push('|');
        for _ in &header_cells {
            md.push_str(" … |");
        }
        md.push('\n');
        for row in rows.iter().skip(total_rows.saturating_sub(half)) {
            render_row(&mut md, row);
        }
    } else {
        for row in &rows {
            render_row(&mut md, row);
        }
    }

    let sections = vec![Section {
        heading: title.to_string(),
        anchor: anchorize(title),
        markdown: md.clone(),
        order: 0,
    }];
    (md, sections)
}

fn escape_table_cell(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
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
        serde_json::Value::String(kind.format_tag().to_string()),
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
        assert_eq!(TextKind::from_extension("csv"), Some(TextKind::Csv));
        assert_eq!(TextKind::from_extension("tsv"), Some(TextKind::Tsv));
        assert_eq!(TextKind::from_extension("pdf"), None);
    }

    #[test]
    fn csv_renders_gfm_table_with_header() {
        let body = "name,age\nAlice,30\nBob,25\n";
        let f = write_named("csv", body.as_bytes());
        let ex = TextExtractor::new(TextKind::Csv);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert_eq!(doc.content_type, "text/csv");
        assert!(doc.markdown.starts_with("# "));
        assert!(doc.markdown.contains("| name | age |"));
        assert!(doc.markdown.contains("| --- | --- |"));
        assert!(doc.markdown.contains("| Alice | 30 |"));
        assert!(doc.markdown.contains("| Bob | 25 |"));
        assert_eq!(
            doc.metadata.get("format"),
            Some(&serde_json::Value::String("csv".to_string()))
        );
    }

    #[test]
    fn tsv_uses_tab_delimiter() {
        let body = "name\tage\nAlice\t30\nBob\t25\n";
        let f = write_named("tsv", body.as_bytes());
        let ex = TextExtractor::new(TextKind::Tsv);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert!(doc.markdown.contains("| name | age |"));
        assert!(doc.markdown.contains("| Alice | 30 |"));
        // Content type is the TSV one, not CSV.
        assert_eq!(doc.content_type, "text/tab-separated-values");
    }

    #[test]
    fn csv_escapes_pipes_and_newlines_in_cells() {
        // Quoted cell with embedded newline + pipe.
        let body = "h\n\"line1\nline2 | mid\"\n";
        let f = write_named("csv", body.as_bytes());
        let ex = TextExtractor::new(TextKind::Csv);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert!(doc.markdown.contains("line1 line2 \\| mid"));
        // Body remains a single Markdown row.
        let body_rows: Vec<&str> = doc
            .markdown
            .lines()
            .filter(|l| l.starts_with("| ") && !l.contains("---"))
            .collect();
        // header + 1 data row = 2.
        assert_eq!(body_rows.len(), 2);
    }

    #[test]
    fn csv_large_file_is_sampled() {
        // Build a 250-row CSV: more than CSV_MAX_RENDERED_ROWS (100).
        let mut body = String::from("n,v\n");
        for i in 0..250 {
            body.push_str(&format!("{i},x\n"));
        }
        let f = write_named("csv", body.as_bytes());
        let ex = TextExtractor::new(TextKind::Csv);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        // Sampling banner is present.
        assert!(doc.markdown.contains("250 rows total"));
        // Sampling separator row appears.
        assert!(doc.markdown.contains("| … |"));
        // First few and last few rows are present.
        assert!(doc.markdown.contains("| 0 | x |"));
        assert!(doc.markdown.contains("| 249 | x |"));
    }

    #[test]
    fn csv_with_only_header_renders_empty_body() {
        let body = "h1,h2\n";
        let f = write_named("csv", body.as_bytes());
        let ex = TextExtractor::new(TextKind::Csv);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert!(doc.markdown.contains("| h1 | h2 |"));
        // No data rows beyond the separator.
        let data_rows: Vec<&str> = doc
            .markdown
            .lines()
            .filter(|l| l.starts_with("| ") && !l.contains("---"))
            .collect();
        assert_eq!(data_rows.len(), 1);
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
    fn code_files_render_as_language_fence() {
        assert_eq!(TextKind::from_extension("rs"), Some(TextKind::Code));
        assert_eq!(TextKind::from_extension("py"), Some(TextKind::Code));
        assert_eq!(TextKind::from_extension("yaml"), Some(TextKind::Code));
        let f = write_named("rs", b"fn main() {\n    println!(\"hi\");\n}\n");
        let ex = TextExtractor::new(TextKind::Code);
        let doc = ex
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: false,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert!(doc.markdown.contains("```rust"));
        assert!(doc.markdown.contains("fn main()"));
        assert_eq!(
            doc.metadata.get("format"),
            Some(&serde_json::Value::String("code".to_string()))
        );
    }

    #[test]
    fn language_map_covers_common_and_skips_unknown() {
        assert_eq!(language_for_extension("rs"), Some("rust"));
        assert_eq!(language_for_extension("toml"), Some("toml"));
        assert_eq!(language_for_extension("mp3"), None);
        assert_eq!(language_for_extension("pptx"), None);
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
