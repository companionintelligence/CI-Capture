use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::Deserialize;

use crate::config::AppConfig;
use crate::server_common::{graphql_url, server_origin};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const POLL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const NULL_STATUS_TIMEOUT: Duration = Duration::from_secs(30);

const FILE_PARSE_STATUS_QUERY: &str = r#"
query FileParseStatus($id: ID!) {
  file(id: $id) {
    id
    filename
    parseStatus
    parseError
    parsedAt
  }
}
"#;

// ── Types ─────────────────────────────────────────────────────────────────────

pub struct UploadedFile {
    pub filename: String,
    pub id: String,
    pub already_existed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseStatusValue {
    Completed,
    Failed,
    Skipped,
}

pub struct TerminalParseStatus {
    pub value: ParseStatusValue,
    /// Sanitized for display — no raw file paths or parser internals.
    pub error: Option<String>,
    pub parsed_at: Option<String>,
}

// ── Upload ────────────────────────────────────────────────────────────────────

pub async fn upload_files(config: &AppConfig, paths: &[PathBuf]) -> Result<Vec<UploadedFile>> {
    let auth_token = config
        .server
        .applet_auth_token
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .context("applet auth token is not configured")?;

    let upload_url = server_origin(&config.server.portal_url)?
        .join("/api/files/upload")
        .context("building upload URL")?;

    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("building upload HTTP client")?;

    let mut form = reqwest::multipart::Form::new();
    let mut total_bytes: u64 = 0;

    for path in paths {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("invalid filename: {}", path.display()))?
            .to_string();

        let bytes = std::fs::read(path)
            .with_context(|| format!("reading file {}", path.display()))?;
        total_bytes += bytes.len() as u64;

        let mime = mime_for_filename(&filename);
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(mime)
            .context("setting MIME type on upload part")?;
        form = form.part("files", part);
    }

    log::info!(
        "uploading {} file(s), {:.1} KB total",
        paths.len(),
        total_bytes as f64 / 1024.0
    );

    let response = client
        .post(upload_url)
        .bearer_auth(auth_token)
        .multipart(form)
        .send()
        .await
        .context("sending upload request")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("upload failed with HTTP {status}: {body}"));
    }

    let ids: Vec<UploadIdEntry> = response.json().await.context("decoding upload response")?;

    log::info!("upload succeeded: {} unique file(s) returned", ids.len());

    // The server deduplicates by hash. We detect already-existing files by
    // comparing the count of returned IDs to the number of files we sent.
    // Any ID that maps to more than one path was a duplicate upload.
    let already_existed = ids.len() < paths.len();

    Ok(ids
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            let filename = paths
                .get(i)
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            UploadedFile {
                filename,
                id: entry.id,
                already_existed: already_existed && i > 0,
            }
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct UploadIdEntry {
    id: String,
}

fn mime_for_filename(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "doc" => "application/msword",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        _ => "text/plain",
    }
}

// ── Parse-status polling ──────────────────────────────────────────────────────

pub async fn poll_parse_status(config: &AppConfig, file_id: &str) -> Result<TerminalParseStatus> {
    let auth_token = config
        .server
        .applet_auth_token
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .context("applet auth token is not configured")?;

    let gql_url = graphql_url(&config.server.portal_url)?;
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("building poll HTTP client")?;

    let started = Instant::now();
    let mut first_non_null_deadline: Option<Instant> = None;
    let mut poll_count: u32 = 0;

    loop {
        if started.elapsed() >= POLL_TIMEOUT {
            log::info!(
                "parse-status poll timed out after {} polls for file {file_id}; treating as skipped",
                poll_count
            );
            return Ok(TerminalParseStatus {
                value: ParseStatusValue::Skipped,
                error: None,
                parsed_at: None,
            });
        }

        let body = serde_json::json!({
            "query": FILE_PARSE_STATUS_QUERY,
            "variables": { "id": file_id }
        });

        let response = client
            .post(gql_url.clone())
            .bearer_auth(auth_token)
            .json(&body)
            .send()
            .await
            .context("sending parse-status poll request")?;

        let gql: GqlResponse<FileStatusData> =
            response.json().await.context("decoding parse-status response")?;

        poll_count += 1;

        let file = gql
            .data
            .as_ref()
            .and_then(|d| d.file.as_ref());

        let parse_status = file.and_then(|f| f.parse_status.as_deref());

        match parse_status {
            Some("completed") => {
                log::info!("file {file_id} completed after {poll_count} poll(s)");
                return Ok(TerminalParseStatus {
                    value: ParseStatusValue::Completed,
                    error: None,
                    parsed_at: file.and_then(|f| f.parsed_at.clone()),
                });
            }
            Some("failed") => {
                log::info!("file {file_id} failed after {poll_count} poll(s)");
                let raw_error = file.and_then(|f| f.parse_error.clone());
                return Ok(TerminalParseStatus {
                    value: ParseStatusValue::Failed,
                    error: raw_error.map(sanitize_parse_error),
                    parsed_at: None,
                });
            }
            Some("skipped") => {
                log::info!("file {file_id} skipped after {poll_count} poll(s)");
                return Ok(TerminalParseStatus {
                    value: ParseStatusValue::Skipped,
                    error: None,
                    parsed_at: None,
                });
            }
            None => {
                // null parseStatus — start the 30s null-status timer on first occurrence
                if first_non_null_deadline.is_none() {
                    first_non_null_deadline = Some(Instant::now() + NULL_STATUS_TIMEOUT);
                }
                if Instant::now() >= first_non_null_deadline.unwrap() {
                    log::info!(
                        "file {file_id} still null after {poll_count} poll(s); treating as skipped"
                    );
                    return Ok(TerminalParseStatus {
                        value: ParseStatusValue::Skipped,
                        error: None,
                        parsed_at: None,
                    });
                }
            }
            Some(other) => {
                // pending | processing — reset null timer, keep polling
                log::debug!("file {file_id} status={other} poll={poll_count}");
                first_non_null_deadline = None;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Strip anything that looks like a file path or internal module reference
/// before surfacing parse errors to the user.
fn sanitize_parse_error(raw: String) -> String {
    // If the error is suspiciously long or contains path separators, replace it
    // with a generic message rather than risk leaking server internals.
    if raw.len() > 200 || raw.contains('/') || raw.contains('\\') {
        return "An internal parse error occurred. Contact support if the problem persists."
            .to_string();
    }
    raw
}

// ── GQL response types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GqlResponse<T> {
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct FileStatusData {
    file: Option<FileStatusPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileStatusPayload {
    parse_status: Option<String>,
    parse_error: Option<String>,
    parsed_at: Option<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn supported_extensions() -> &'static [&'static str] {
    &["pdf", "docx", "xlsx", "xls", "doc", "csv", "txt", "md", "markdown"]
}
