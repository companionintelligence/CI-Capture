//! Email (.eml) extractor via `mail-parser`.
//!
//! Behaviour:
//! - Parses RFC 5322 mail bytes (also handles MIME multipart).
//! - Subject → top-level `# ` heading; falls back to filename stem.
//! - From / To / Cc / Date / Message-ID → YAML frontmatter.
//! - Body prefers the `text/plain` part; falls back to a stripped-tag
//!   rendering of the `text/html` part when no text body is present.
//!
//! This extractor avoids depending on `html2md` so it can live on its
//! own sub-PR. The eventual conversion runner can post-process the HTML
//! body through the HTML extractor when both extractors are wired up.

use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use mail_parser::{HeaderName, HeaderValue, MessageParser};
use uuid::Uuid;

use super::traits::{ExtractOptions, ExtractedDocument, ExtractionDiagnostics, Extractor, Section};

pub struct EmlExtractor;

impl EmlExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EmlExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for EmlExtractor {
    fn name(&self) -> &'static str {
        "eml-extractor/0.1"
    }

    fn extract(&self, path: &Path, opts: ExtractOptions) -> Result<ExtractedDocument> {
        let started = Instant::now();

        let stem_title = filename_stem(path);
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "format".to_string(),
            serde_json::Value::String("eml".to_string()),
        );

        if opts.metadata_only {
            metadata.insert(
                "title".to_string(),
                serde_json::Value::String(stem_title.clone()),
            );
            return Ok(empty_doc(path, stem_title, metadata, started));
        }

        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;

        let message = MessageParser::default()
            .parse(&raw)
            .context("parsing as RFC5322 email")?;

        let subject = message
            .subject()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| stem_title.clone());

        metadata.insert(
            "title".to_string(),
            serde_json::Value::String(subject.clone()),
        );
        if let Some(from) = render_addresses(message.from()) {
            metadata.insert("from".to_string(), serde_json::Value::String(from));
        }
        if let Some(to) = render_addresses(message.to()) {
            metadata.insert("to".to_string(), serde_json::Value::String(to));
        }
        if let Some(cc) = render_addresses(message.cc()) {
            metadata.insert("cc".to_string(), serde_json::Value::String(cc));
        }
        if let Some(date) = message.date() {
            metadata.insert(
                "date".to_string(),
                serde_json::Value::String(date.to_rfc3339()),
            );
        }
        if let Some(mid) = first_header_string(&message, "Message-ID") {
            metadata.insert("message_id".to_string(), serde_json::Value::String(mid));
        }

        let body_text = message.body_text(0).map(|c| c.to_string());
        let body_html = message.body_html(0).map(|c| c.to_string());

        let body_md = match (body_text, body_html) {
            (Some(t), _) if !t.trim().is_empty() => t.trim().to_string(),
            (_, Some(h)) if !h.trim().is_empty() => strip_html_tags(&h),
            _ => String::new(),
        };

        let markdown = if body_md.is_empty() {
            format!("# {subject}\n")
        } else {
            format!("# {subject}\n\n{body_md}\n")
        };

        let sections = vec![Section {
            heading: subject.clone(),
            anchor: anchorize(&subject),
            markdown: markdown.clone(),
            order: 0,
        }];

        Ok(ExtractedDocument {
            document_id: Uuid::new_v4().to_string(),
            file_id: String::new(),
            version_id: String::new(),
            source_path: path.display().to_string(),
            content_type: "message/rfc822".to_string(),
            markdown,
            sections,
            metadata,
            diagnostics: ExtractionDiagnostics {
                extractor: "eml-extractor/0.1".to_string(),
                ocr_used: false,
                transcription_used: false,
                warnings: Vec::new(),
                extraction_time_ms: started.elapsed().as_millis() as u64,
                confidence: Some(0.95),
            },
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn render_addresses(addr: Option<&mail_parser::Address>) -> Option<String> {
    let addr = addr?;
    let parts: Vec<String> = addr
        .iter()
        .map(|a| {
            let email = a.address().unwrap_or("");
            match a.name() {
                Some(name) if !name.is_empty() => format!("{name} <{email}>"),
                _ => email.to_string(),
            }
        })
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn first_header_string(message: &mail_parser::Message, header: &str) -> Option<String> {
    message
        .headers()
        .iter()
        .find(|h| match &h.name {
            HeaderName::Other(name) => name.eq_ignore_ascii_case(header),
            named => named.as_str().eq_ignore_ascii_case(header),
        })
        .and_then(|h| match &h.value {
            HeaderValue::Text(t) => Some(t.to_string()),
            HeaderValue::TextList(list) => list.first().map(|s| s.to_string()),
            _ => None,
        })
}

/// Very small HTML→text converter for the fallback path (no `html2md`
/// in this PR). Drops tags, decodes a handful of common entities,
/// collapses whitespace.
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
    // Collapse runs of whitespace.
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
        content_type: "message/rfc822".to_string(),
        markdown: String::new(),
        sections: vec![Section {
            heading: title.clone(),
            anchor: anchorize(&title),
            markdown: String::new(),
            order: 0,
        }],
        metadata,
        diagnostics: ExtractionDiagnostics {
            extractor: "eml-extractor/0.1".to_string(),
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

    fn write_eml(body: &[u8]) -> NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".eml")
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

    const PLAIN_MSG: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: Lunch?\r\n\
Date: Wed, 18 May 2026 12:00:00 -0700\r\n\
Message-ID: <abc@example.com>\r\n\
\r\n\
Hey Bob,\r\n\
Are you free for lunch on Friday?\r\n\
-- Alice\r\n";

    #[test]
    fn parses_plain_email() {
        let f = write_eml(PLAIN_MSG);
        let doc = EmlExtractor::new().extract(f.path(), opts()).unwrap();
        assert_eq!(doc.content_type, "message/rfc822");
        assert!(doc.markdown.starts_with("# Lunch?"));
        assert!(doc.markdown.contains("Are you free for lunch"));
        assert_eq!(
            doc.metadata.get("title"),
            Some(&serde_json::Value::String("Lunch?".to_string()))
        );
        assert!(
            doc.metadata
                .get("from")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("alice@example.com"))
                .unwrap_or(false)
        );
        assert!(
            doc.metadata
                .get("to")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("bob@example.com"))
                .unwrap_or(false)
        );
        // Date round-trips to RFC3339.
        let date = doc.metadata.get("date").and_then(|v| v.as_str()).unwrap();
        assert!(date.starts_with("2026-05-18"));
        // mail-parser canonicalizes Message-ID to the id between the angle
        // brackets (per RFC 5322), so we assert on the id itself.
        assert!(
            doc.metadata
                .get("message_id")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("abc@example.com"))
                .unwrap_or(false),
            "message_id = {:?}",
            doc.metadata.get("message_id")
        );
    }

    #[test]
    fn missing_subject_falls_back_to_filename() {
        let body = b"From: a@x\r\nTo: b@y\r\n\r\nbody\r\n";
        let f = write_eml(body);
        let doc = EmlExtractor::new().extract(f.path(), opts()).unwrap();
        let stem = f.path().file_stem().unwrap().to_str().unwrap().to_string();
        assert!(doc.markdown.starts_with(&format!("# {stem}")));
    }

    #[test]
    fn html_only_body_is_stripped_to_text() {
        let body = b"From: a@x\r\nTo: b@y\r\nSubject: HTML test\r\n\
Content-Type: text/html\r\n\
\r\n\
<html><body><p>Hello <b>world</b>!</p></body></html>\r\n";
        let f = write_eml(body);
        let doc = EmlExtractor::new().extract(f.path(), opts()).unwrap();
        assert!(doc.markdown.contains("Hello world"));
        // No raw tag remnants.
        assert!(!doc.markdown.contains("<b>"));
        assert!(!doc.markdown.contains("</p>"));
    }

    #[test]
    fn multipart_prefers_text_body() {
        // RFC 2046 multipart/alternative with text + html parts.
        let body = b"From: a@x\r\nTo: b@y\r\nSubject: Multipart\r\n\
Content-Type: multipart/alternative; boundary=BB\r\n\
\r\n\
--BB\r\n\
Content-Type: text/plain\r\n\
\r\n\
PLAIN_TEXT_BODY\r\n\
--BB\r\n\
Content-Type: text/html\r\n\
\r\n\
<p>HTML_BODY</p>\r\n\
--BB--\r\n";
        let f = write_eml(body);
        let doc = EmlExtractor::new().extract(f.path(), opts()).unwrap();
        // Plain text wins.
        assert!(doc.markdown.contains("PLAIN_TEXT_BODY"));
        // HTML part text should not appear in this case (we picked plain).
        assert!(!doc.markdown.contains("HTML_BODY"));
    }

    #[test]
    fn empty_body_still_renders_subject_heading() {
        let body = b"From: a@x\r\nSubject: Bare\r\n\r\n";
        let f = write_eml(body);
        let doc = EmlExtractor::new().extract(f.path(), opts()).unwrap();
        assert!(doc.markdown.starts_with("# Bare"));
        assert!(!doc.markdown.contains("\n\n"));
    }

    #[test]
    fn metadata_only_yields_empty_body() {
        let f = write_eml(PLAIN_MSG);
        let doc = EmlExtractor::new()
            .extract(
                f.path(),
                ExtractOptions {
                    metadata_only: true,
                    allow_heavy_models: false,
                },
            )
            .unwrap();
        assert_eq!(doc.markdown, "");
        assert!(
            doc.diagnostics
                .warnings
                .contains(&"metadata_only".to_string())
        );
        // Title falls back to filename stem (we don't parse in metadata-only).
        let stem = f.path().file_stem().unwrap().to_str().unwrap();
        assert_eq!(
            doc.metadata.get("title"),
            Some(&serde_json::Value::String(stem.to_string()))
        );
    }

    #[test]
    fn renders_addresses_combines_name_and_email() {
        let body = b"From: \"Alice Example\" <alice@example.com>\r\n\
To: bob@example.com\r\n\
Subject: Hi\r\n\
\r\n\
hello\r\n";
        let f = write_eml(body);
        let doc = EmlExtractor::new().extract(f.path(), opts()).unwrap();
        let from = doc.metadata.get("from").and_then(|v| v.as_str()).unwrap();
        assert!(
            from.contains("Alice Example") && from.contains("alice@example.com"),
            "from should contain both name + email: {from}"
        );
        let to = doc.metadata.get("to").and_then(|v| v.as_str()).unwrap();
        assert_eq!(to, "bob@example.com");
    }

    #[test]
    fn strip_html_handles_basic_entities_and_collapses_whitespace() {
        let html = "<p>Cats   &amp; \n\n dogs</p>";
        let s = strip_html_tags(html);
        assert_eq!(s, "Cats & dogs");
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = EmlExtractor::new()
            .extract(Path::new("/this/path/should/not/exist-eml-3a"), opts())
            .unwrap_err();
        assert!(err.to_string().contains("/this/path/"));
    }
}
