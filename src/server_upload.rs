use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::config::AppConfig;
use crate::server_common::{AuthCredential, graphql_urls, upload_urls};

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
    let credential = AuthCredential::from_config(config)?;

    let client = Client::builder()
        .no_proxy()
        .use_rustls_tls()
        .build()
        .context("building upload HTTP client")?;

    let total_bytes = total_upload_bytes(paths)?;
    log::info!(
        "uploading {} file(s), {:.1} KB total",
        paths.len(),
        total_bytes as f64 / 1024.0
    );

    let mut last_error = None;
    for upload_url in upload_urls(&config.server.portal_url)? {
        let form = build_upload_form(paths)?;
        let response = credential
            .apply(client.post(upload_url.clone()))
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("sending upload request to {upload_url}"))?;

        let status = response.status();
        if should_try_next_endpoint(status) {
            let body = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!("upload failed with HTTP {status}: {body}"));
            continue;
        }

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

        return Ok(ids
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
            .collect());
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no upload endpoint candidates")))
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

fn total_upload_bytes(paths: &[PathBuf]) -> Result<u64> {
    paths.iter().try_fold(0u64, |total, path| {
        let metadata =
            std::fs::metadata(path).with_context(|| format!("stat file {}", path.display()))?;
        Ok(total + metadata.len())
    })
}

fn build_upload_form(paths: &[PathBuf]) -> Result<reqwest::multipart::Form> {
    let mut form = reqwest::multipart::Form::new();

    for path in paths {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("invalid filename: {}", path.display()))?
            .to_string();

        let bytes =
            std::fs::read(path).with_context(|| format!("reading file {}", path.display()))?;
        let mime = mime_for_filename(&filename);
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(mime)
            .context("setting MIME type on upload part")?;
        form = form.part("files", part);
    }

    Ok(form)
}

// ── Parse-status polling ──────────────────────────────────────────────────────

pub async fn poll_parse_status(config: &AppConfig, file_id: &str) -> Result<TerminalParseStatus> {
    let credential = AuthCredential::from_config(config)?;

    let gql_urls = graphql_urls(&config.server.portal_url)?;
    let client = Client::builder()
        .no_proxy()
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

        let mut response = None;
        let mut last_error = None;
        for gql_url in &gql_urls {
            let candidate = credential
                .apply(client.post(gql_url.clone()))
                .json(&body)
                .send()
                .await
                .with_context(|| format!("sending parse-status poll request to {gql_url}"))?;
            let status = candidate.status();
            if should_try_next_endpoint(status) {
                let text = candidate.text().await.unwrap_or_default();
                last_error = Some(anyhow!(
                    "parse-status poll failed with HTTP {status}: {text}"
                ));
                continue;
            }
            if !status.is_success() {
                let text = candidate.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "parse-status poll failed with HTTP {status}: {text}"
                ));
            }
            response = Some(candidate);
            break;
        }
        let response = response.ok_or_else(|| {
            last_error.unwrap_or_else(|| anyhow!("no GraphQL endpoint candidates"))
        })?;

        let gql: GqlResponse<FileStatusData> = response
            .json()
            .await
            .context("decoding parse-status response")?;

        poll_count += 1;

        let file = gql.data.as_ref().and_then(|d| d.file.as_ref());

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

fn should_try_next_endpoint(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    )
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
    &[
        "pdf", "docx", "xlsx", "xls", "doc", "csv", "txt", "md", "markdown",
    ]
}
