//! Redaction for HTTP error responses on their way into an error string.
//!
//! A failed request's body is free-form prose *about our request* — and our
//! requests carry the user's own data. `ingest_file_event` sends the absolute
//! path of the changed file as the event `title` and up to
//! `DESCRIPTION_CONTENT_BYTES` of its extracted Markdown as the `description`;
//! `set_file_metadata` sends the vision model's description of a screenshot;
//! the vision endpoint itself receives a base64 screenshot and the analysis
//! prompt. A 400/422 that quotes the offending field hands any of that straight
//! back, and a locally-run LM Studio or Ollama routinely echoes the whole
//! request on an error.
//!
//! That matters because these strings are **persisted**: `file_tracker` writes
//! `err.to_string()` into the queue DB via `queue.dead_letter(…)`
//! (`dead_letters.reason`) and `queue.requeue(…)` (`events.last_error`), where
//! it outlives the process that produced it.
//!
//! So the body is not summarised, sampled, or truncated — truncation keeps a
//! prefix, and a prefix of a leaked document is still a leaked document. Only a
//! machine-readable *error code* survives: a short token, read from a known
//! field, matched against an allowlist of code-shaped characters. The HTTP
//! status carries the rest of the diagnosis, and the caller keeps it.
//!
//! This generalises `oidc::trim_body` — same allowlist idea, widened from
//! OAuth's single `error` code to the field names CI-Server and
//! OpenAI-compatible endpoints actually use. It deliberately does *not* follow
//! `server_upload::sanitize_parse_error`, which is a denylist (drop if over 200
//! chars or containing `/` or `\`): a 4 KB Markdown body trips that, but two
//! lines of the user's notes do not, and prose-with-no-slashes is exactly the
//! shape of the leak here. An allowlist fails closed; a denylist fails open.

/// Longest token we will accept as an error code. Real codes
/// (`invalid_grant`, `unsupported_media_type`) sit well inside this; anything
/// longer is prose that happens to lack spaces.
const MAX_CODE_LEN: usize = 64;

/// Above this, don't even hand the body to the JSON parser. Bodies come from
/// `.text()` and are unbounded, and a server that returned a megabyte is not
/// about to have a tidy `error` code in it.
const MAX_PARSE_BYTES: usize = 64 * 1024;

/// Message suffix describing a failed HTTP response, with the body removed.
///
/// Returns one of:
/// - `""` — the server sent no body
/// - `" — invalid_grant"` — a code-shaped token from a known error field
/// - `" — response body suppressed (1234 bytes)"` — a body we refuse to quote
///
/// The middle case is the diagnosis worth keeping; the last case still tells an
/// operator that the server *said* something, which distinguishes a silent
/// rejection from a chatty one without repeating a word of it.
pub fn error_body_suffix(body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    match code_from_json(body) {
        Some(code) => format!(" — {code}"),
        None => format!(" — response body suppressed ({} bytes)", body.len()),
    }
}

/// Message suffix for a server-supplied string that is *meant* to be an error
/// code — CI-Server's per-event `rejected[].reason`.
///
/// Kept verbatim when it is code-shaped, dropped entirely when it is not. There
/// is no "suppressed" note here: unlike a response body, the field is small and
/// singular, so its mere presence is not itself a signal worth recording.
pub fn error_code_suffix(raw: &str) -> String {
    let token = raw.trim();
    if is_code_token(token) {
        format!(" — {token}")
    } else {
        String::new()
    }
}

/// Pull a machine-readable error code out of a JSON error body.
///
/// Checks the shapes servers in this app's path actually emit:
/// - `{"error": "invalid_grant"}` — OAuth (RFC 6749 §5.2), Ollama native
/// - `{"error": {"code": "…", "type": "…"}}` — OpenAI-compatible (LM Studio)
/// - `{"code": "…"}` — assorted REST APIs
///
/// A NestJS validation error (`{"statusCode":422,"message":[…],"error":
/// "Unprocessable Entity"}`) yields nothing: `error` there restates the status
/// in prose, and `message` is the free text we are here to drop.
fn code_from_json(body: &str) -> Option<String> {
    if body.len() > MAX_PARSE_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;

    let error = value.get("error");
    let candidates = [
        error.and_then(|e| e.as_str()),
        error.and_then(|e| e.get("code")).and_then(|c| c.as_str()),
        error.and_then(|e| e.get("type")).and_then(|t| t.as_str()),
        value.get("code").and_then(|c| c.as_str()),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|c| is_code_token(c))
        .map(str::to_owned)
}

/// Whether `s` looks like a machine-readable error code rather than prose.
///
/// Allowlist: ASCII alphanumerics, `_`, and `-`. That admits `invalid_grant`
/// and `rate-limit-exceeded` while excluding everything a leak is made of —
/// spaces, quotes, newlines, `/` and `\` path separators, `.` in filenames, and
/// every non-ASCII character.
///
/// Note the length check compares bytes but never *slices* by them. The
/// ancestor of this function sliced `&b[..300]` on a byte index, which panicked
/// outright when byte 300 landed inside a multi-byte character — and Rust's
/// boundary panic quotes the offending string, turning the whole response body
/// into a panic payload. Comparing is safe; slicing is what was not.
fn is_code_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_CODE_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-ins for the two payloads that actually reach these endpoints: the
    /// path + body of a file the user edited, and a screenshot analysis.
    const SECRETS: &[&str] = &[
        "/Users/liam/notes/q3-comp-review.md",
        "salary band",
        "liam.broza@gmail.com",
        "iVBORw0KGgoAAAANSUhEUg",
    ];

    fn assert_clean(out: &str) {
        for secret in SECRETS {
            assert!(!out.contains(secret), "leaked {secret:?} in {out:?}");
        }
    }

    #[test]
    fn a_validation_rejection_quoting_the_document_leaks_none_of_it() {
        // CI-Server rejecting the event and echoing the offending fields —
        // `title` is the absolute path, `description` is the file's markdown.
        let body = serde_json::json!({
            "statusCode": 422,
            "error": "Unprocessable Entity",
            "message": [
                "title must be shorter than 255 characters: /Users/liam/notes/q3-comp-review.md",
                "description contains disallowed content: # Q3 review\n\nsalary band 4, liam.broza@gmail.com",
            ],
        })
        .to_string();

        let out = error_body_suffix(&body);
        assert_clean(&out);
        // Nothing survives: `error` restates the status in prose, and the
        // status is already in the caller's message.
        assert!(
            out.starts_with(" — response body suppressed ("),
            "got: {out}"
        );
    }

    #[test]
    fn a_vision_endpoint_echoing_the_request_leaks_neither_image_nor_prompt() {
        // LM Studio / Ollama commonly reflect the request back on a 400. The
        // request holds the base64 screenshot and the analysis prompt.
        let body = serde_json::json!({
            "error": {
                "message": format!(
                    "invalid image: data:image/png;base64,{}",
                    "iVBORw0KGgoAAAANSUhEUg".repeat(50)
                ),
                "type": "invalid_request_error",
                "param": "messages[1].content[1].image_url",
            }
        })
        .to_string();

        let out = error_body_suffix(&body);
        assert_clean(&out);
        // The `type` is the diagnosis and it is code-shaped, so it survives.
        assert_eq!(out, " — invalid_request_error");
    }

    #[test]
    fn known_code_fields_survive() {
        assert_eq!(
            error_body_suffix(r#"{"error":"invalid_grant"}"#),
            " — invalid_grant"
        );
        assert_eq!(
            error_body_suffix(r#"{"error":{"code":"context_length_exceeded"}}"#),
            " — context_length_exceeded"
        );
        assert_eq!(
            error_body_suffix(r#"{"code":"rate-limit-exceeded"}"#),
            " — rate-limit-exceeded"
        );
        // `error.code` is preferred over the less specific `error.type`.
        assert_eq!(
            error_body_suffix(r#"{"error":{"code":"model_not_found","type":"not_found"}}"#),
            " — model_not_found"
        );
        // A non-code `error` string falls through to a usable sibling rather
        // than blocking it.
        assert_eq!(
            error_body_suffix(r#"{"error":"Unprocessable Entity","code":"schema_invalid"}"#),
            " — schema_invalid"
        );
    }

    #[test]
    fn a_body_with_no_code_shaped_field_is_suppressed_wholesale() {
        for body in [
            // Ollama native: prose in `error`.
            r#"{"error":"model 'qwen3-vl' not found, pull it with: ollama pull qwen3-vl"}"#,
            // Reverse proxy in the way.
            "<html><head><title>502 Bad Gateway</title></head></html>",
            // A code field that is really a sentence.
            r#"{"code":"the file /Users/liam/notes/q3-comp-review.md was rejected"}"#,
            // Nested object where a string was expected.
            r#"{"error":{"nested":{"detail":"salary band"}}}"#,
            // Plain text.
            "salary band 4",
        ] {
            let out = error_body_suffix(body);
            assert_clean(&out);
            assert!(
                out.starts_with(" — response body suppressed ("),
                "body {body:?} produced {out:?}"
            );
        }
    }

    #[test]
    fn an_empty_body_contributes_nothing() {
        assert_eq!(error_body_suffix(""), "");
        assert_eq!(error_body_suffix("   \n\t "), "");
    }

    #[test]
    fn an_over_long_token_is_not_a_code() {
        // No spaces, so a "does it look like prose" heuristic would pass it —
        // but 300 characters of anything is not an error code.
        let long = "a".repeat(MAX_CODE_LEN + 1);
        let body = format!(r#"{{"error":"{long}"}}"#);
        assert!(error_body_suffix(&body).starts_with(" — response body suppressed ("));

        let ok = "a".repeat(MAX_CODE_LEN);
        assert_eq!(error_body_suffix(&format!(r#"{{"error":"{ok}"}}"#)), {
            format!(" — {ok}")
        });
    }

    #[test]
    fn a_non_ascii_body_neither_panics_nor_leaks() {
        // The ancestor sliced `&b[..300]` by byte index and panicked here, and
        // the panic message quotes the string — so the crash *was* the leak.
        let prose = "el archivo señalado contiene información confidencial ".repeat(20);
        assert_eq!(error_body_suffix(&prose), {
            format!(" — response body suppressed ({} bytes)", prose.len())
        });

        let body = format!(r#"{{"error":"invalid_grant","error_description":"{prose}"}}"#);
        assert_eq!(error_body_suffix(&body), " — invalid_grant");

        // Non-ASCII is not code-shaped even when it is the whole `error`.
        assert!(
            error_body_suffix(r#"{"error":"señal"}"#).starts_with(" — response body suppressed (")
        );
    }

    #[test]
    fn an_enormous_body_is_suppressed_without_being_parsed() {
        // Bigger than MAX_PARSE_BYTES and valid JSON with a real code in it —
        // we still decline, because the guard runs first.
        let filler = "salary band ".repeat(MAX_PARSE_BYTES / 4);
        let body = format!(r#"{{"error":"invalid_grant","detail":"{filler}"}}"#);
        assert!(body.len() > MAX_PARSE_BYTES);

        let out = error_body_suffix(&body);
        assert_clean(&out);
        assert!(
            out.starts_with(" — response body suppressed ("),
            "got: {out}"
        );
    }

    #[test]
    fn a_rejection_reason_is_kept_only_when_it_is_a_code() {
        assert_eq!(
            error_code_suffix("duplicate_source_id"),
            " — duplicate_source_id"
        );
        assert_eq!(error_code_suffix("  schema_invalid  "), " — schema_invalid");

        for reason in [
            "",
            "   ",
            "title too long: /Users/liam/notes/q3-comp-review.md",
            "description quoted salary band 4",
            "razón no válida",
        ] {
            let out = error_code_suffix(reason);
            assert_clean(&out);
            assert_eq!(out, "", "reason {reason:?} produced {out:?}");
        }
    }
}
