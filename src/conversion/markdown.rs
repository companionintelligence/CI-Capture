//! Shared Markdown assembly helpers used by the heavier extractors
//! (DOCX, PPTX, XLSX, HTML, EML, etc.).
//!
//! Pure-Rust, no external deps beyond `serde_json` (already in the
//! workspace). The goal is one canonical implementation of these
//! primitives so per-format extractors don't reinvent escaping rules.

use serde_json::{Map, Value};

/// GitHub-flavored slugifier: lowercase, non-alphanumerics → `-`, collapse
/// runs, trim trailing dashes. Use this when generating
/// [`crate::extractors::traits::Section::anchor`] from a heading.
pub fn slugify(s: &str) -> String {
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

/// Escape Markdown special characters in inline text so a downstream
/// reader doesn't render plain content as markup.
pub fn escape_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '|' | '#' | '~' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// `## Heading\n\n`. Levels are clamped to 1..=6. Text is **not** escaped
/// — the caller usually wants the heading to render even if it contains
/// `*` etc.
pub fn heading(level: u8, text: &str) -> String {
    let level = level.clamp(1, 6) as usize;
    let mut out = "#".repeat(level);
    out.push(' ');
    out.push_str(text.trim());
    out.push_str("\n\n");
    out
}

/// Prefix every line with `> `.
pub fn blockquote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("> ");
        out.push_str(line);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Bulleted list.
pub fn list_unordered<I, S>(items: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    for item in items {
        out.push_str("- ");
        out.push_str(item.as_ref().trim());
        out.push('\n');
    }
    out
}

/// Fenced code block. `lang` may be empty for an untyped fence.
pub fn code_block(lang: &str, body: &str) -> String {
    let body = body.trim_end_matches('\n');
    if lang.is_empty() {
        format!("```\n{body}\n```\n")
    } else {
        format!("```{lang}\n{body}\n```\n")
    }
}

/// GitHub-flavored Markdown table.
///
/// Cells are escaped: `|` becomes `\|` and `\n` becomes a space, matching
/// what most renderers expect. Returns the empty string if `headers` is
/// empty.
pub fn table<H, R, S>(headers: H, rows: R) -> String
where
    H: IntoIterator<Item = S>,
    R: IntoIterator,
    R::Item: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let headers: Vec<String> = headers
        .into_iter()
        .map(|h| escape_cell(h.as_ref()))
        .collect();
    if headers.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push('|');
    for h in &headers {
        out.push(' ');
        out.push_str(h);
        out.push_str(" |");
    }
    out.push('\n');
    out.push('|');
    for _ in &headers {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in rows {
        let cells: Vec<String> = row.into_iter().map(|c| escape_cell(c.as_ref())).collect();
        out.push('|');
        for i in 0..headers.len() {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            out.push(' ');
            out.push_str(cell);
            out.push_str(" |");
        }
        out.push('\n');
    }
    out
}

fn escape_cell(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

/// YAML frontmatter block from a JSON-shaped metadata map.
///
/// Supports the subset of JSON we actually emit from extractors:
/// strings, numbers, booleans, null, and flat arrays of those. Nested
/// objects are rendered as inline JSON (preserves round-trip, callers
/// can refine later if a real YAML dep gets added).
///
/// Returns the empty string if `metadata` is empty.
pub fn yaml_frontmatter(metadata: &Map<String, Value>) -> String {
    if metadata.is_empty() {
        return String::new();
    }
    let mut out = String::from("---\n");
    for (key, value) in metadata {
        out.push_str(&yaml_quote_key(key));
        out.push_str(": ");
        out.push_str(&yaml_value(value));
        out.push('\n');
    }
    out.push_str("---\n");
    out
}

fn yaml_quote_key(key: &str) -> String {
    if key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !key.is_empty()
    {
        key.to_string()
    } else {
        format!("\"{}\"", key.replace('"', "\\\""))
    }
}

fn yaml_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => yaml_quote_string(s),
        Value::Array(arr) => {
            // Flat arrays of simple scalars → inline list. Nested →
            // inline JSON for safety.
            let all_scalar = arr.iter().all(|v| {
                matches!(
                    v,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            });
            if all_scalar {
                let parts: Vec<String> = arr.iter().map(yaml_value).collect();
                format!("[{}]", parts.join(", "))
            } else {
                value.to_string()
            }
        }
        Value::Object(_) => value.to_string(),
    }
}

fn yaml_quote_string(s: &str) -> String {
    // Always quote strings — avoids the YAML "Norway problem" (`no` → false)
    // and a host of other implicit-typing surprises.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn slugify_handles_common_cases() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("CES 2026: keynote"), "ces-2026-keynote");
        assert_eq!(slugify("   spaced  out  "), "spaced-out");
        assert_eq!(slugify("---weird---"), "weird");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn escape_inline_escapes_special_chars() {
        assert_eq!(escape_inline("*bold*"), "\\*bold\\*");
        assert_eq!(escape_inline("a_b_c"), "a\\_b\\_c");
        assert_eq!(escape_inline("`code`"), "\\`code\\`");
        assert_eq!(escape_inline("a|b"), "a\\|b");
        // Plain text passes through.
        assert_eq!(escape_inline("Hello world"), "Hello world");
        // Backslash itself is escaped.
        assert_eq!(escape_inline(r"\n"), "\\\\n");
    }

    #[test]
    fn heading_clamps_levels() {
        assert_eq!(heading(1, "Title"), "# Title\n\n");
        assert_eq!(heading(6, "Deep"), "###### Deep\n\n");
        // Above 6 clamps.
        assert_eq!(heading(9, "Very deep"), "###### Very deep\n\n");
        // Zero clamps up.
        assert_eq!(heading(0, "Top"), "# Top\n\n");
    }

    #[test]
    fn blockquote_prefixes_each_line() {
        let q = blockquote("hello\nworld");
        assert_eq!(q, "> hello\n> world\n");
        let single = blockquote("just one");
        assert_eq!(single, "> just one\n");
    }

    #[test]
    fn list_unordered_basic() {
        let out = list_unordered(["one", "two", "three"]);
        assert_eq!(out, "- one\n- two\n- three\n");
    }

    #[test]
    fn code_block_with_and_without_lang() {
        let with = code_block("rust", "fn main() {}");
        assert_eq!(with, "```rust\nfn main() {}\n```\n");
        let without = code_block("", "raw");
        assert_eq!(without, "```\nraw\n```\n");
        // Trailing newlines in body are absorbed (we add exactly one).
        let trim = code_block("", "raw\n\n\n");
        assert_eq!(trim, "```\nraw\n```\n");
    }

    #[test]
    fn table_renders_gfm() {
        let out = table(["A", "B"], vec![vec!["1", "2"], vec!["3", "4"]]);
        assert_eq!(
            out,
            "\
| A | B |
| --- | --- |
| 1 | 2 |
| 3 | 4 |
"
        );
    }

    #[test]
    fn table_escapes_pipes_and_newlines_in_cells() {
        let out = table(["Header"], vec![vec!["has | pipe"], vec!["multi\nline"]]);
        assert!(out.contains("has \\| pipe"));
        assert!(out.contains("multi line"));
        // No literal newline mid-cell.
        let body_lines: Vec<_> = out.lines().collect();
        // 1 header + 1 separator + 2 data rows = 4 lines.
        assert_eq!(body_lines.len(), 4);
    }

    #[test]
    fn table_pads_short_rows() {
        let out = table(["A", "B", "C"], vec![vec!["1"]]);
        // Row has 3 cells, two empty.
        assert!(out.contains("| 1 |  |  |"));
    }

    #[test]
    fn table_with_no_headers_is_empty() {
        let empty: [&str; 0] = [];
        let rows: Vec<Vec<&str>> = vec![];
        let out = table(empty, rows);
        assert_eq!(out, "");
    }

    #[test]
    fn yaml_frontmatter_renders_basic_map() {
        let mut m = Map::new();
        m.insert("title".to_string(), json!("Hello"));
        m.insert("count".to_string(), json!(3));
        m.insert("draft".to_string(), json!(false));
        let out = yaml_frontmatter(&m);
        assert!(out.starts_with("---\n"));
        assert!(out.ends_with("---\n"));
        assert!(out.contains("title: \"Hello\""));
        assert!(out.contains("count: 3"));
        assert!(out.contains("draft: false"));
    }

    #[test]
    fn yaml_frontmatter_empty_map_is_empty_string() {
        let m = Map::new();
        assert_eq!(yaml_frontmatter(&m), "");
    }

    #[test]
    fn yaml_quotes_strings_to_avoid_norway_problem() {
        let mut m = Map::new();
        m.insert("country".to_string(), json!("no"));
        m.insert("ok".to_string(), json!("yes"));
        let out = yaml_frontmatter(&m);
        // Both should be quoted strings, never bare → boolean.
        assert!(out.contains("country: \"no\""));
        assert!(out.contains("ok: \"yes\""));
    }

    #[test]
    fn yaml_quotes_keys_with_special_chars() {
        let mut m = Map::new();
        m.insert("with space".to_string(), json!("v"));
        let out = yaml_frontmatter(&m);
        assert!(out.contains("\"with space\": \"v\""));
    }

    #[test]
    fn yaml_renders_flat_arrays_inline() {
        let mut m = Map::new();
        m.insert("tags".to_string(), json!(["a", "b", "c"]));
        let out = yaml_frontmatter(&m);
        assert!(out.contains("tags: [\"a\", \"b\", \"c\"]"));
    }

    #[test]
    fn yaml_renders_nested_objects_as_inline_json() {
        let mut m = Map::new();
        m.insert("nested".to_string(), json!({"k": "v"}));
        let out = yaml_frontmatter(&m);
        // We don't promise YAML for nested — but we do promise round-trippable JSON.
        assert!(out.contains("nested: {\"k\":\"v\"}"));
    }

    #[test]
    fn yaml_escapes_control_chars_in_strings() {
        let mut m = Map::new();
        m.insert("messy".to_string(), json!("line1\nline2\ttab\"quote"));
        let out = yaml_frontmatter(&m);
        assert!(out.contains("messy: \"line1\\nline2\\ttab\\\"quote\""));
    }
}
