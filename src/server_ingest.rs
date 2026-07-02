use anyhow::{Context, Result, anyhow};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::VizierEventRecord;
use crate::server_common::{AuthCredential, events_urls, files_urls, upload_urls};

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct CaptureEvent {
    source_system: &'static str,
    source_id: String,
    kind: String,
    occurred_at: String,
    observed_at: Option<String>,
    title: Option<String>,
    description: Option<String>,
    payload: Value,
}

#[derive(Debug, Serialize)]
struct IngestEventsRequest {
    events: Vec<CaptureEvent>,
}

#[derive(Debug, Deserialize)]
struct IngestEventsResponse {
    ingested: u32,
    duplicate: u32,
    rejected: Vec<RejectedEvent>,
}

#[derive(Debug, Deserialize)]
struct RejectedEvent {
    index: usize,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct UploadResponse {
    id: String,
}

// ── Public API ─────────────────────────────────────────────────────────────────

pub fn is_configured(config: &AppConfig) -> bool {
    config
        .server
        .applet_auth_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
}

/// Upload a captured screenshot to the server via `POST /files/upload`.
///
/// The server runs its own vision analysis on the uploaded image via the
/// inventory pipeline (`inventoryCapture=true`). Returns the server-assigned
/// file ID on success.
pub async fn ingest_capture(
    config: &AppConfig,
    filename: &str,
    png_bytes: &[u8],
    inventory: bool,
    model: Option<&str>,
) -> Result<String> {
    let credential = AuthCredential::from_config(config)?;

    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("building capture upload client")?;

    let mut last_error = None;
    for upload_url in upload_urls(&config.server.portal_url)? {
        let form = capture_upload_form(filename, png_bytes, inventory, model)?;
        let response = credential
            .apply(client.post(upload_url.clone()))
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("sending capture upload request to {upload_url}"))?;

        let status = response.status();
        if should_try_next_endpoint(status) {
            let body = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!("capture upload failed with HTTP {status}: {body}"));
            continue;
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("capture upload failed with HTTP {status}: {body}"));
        }

        let entries: Vec<UploadResponse> = response
            .json()
            .await
            .context("decoding capture upload response")?;

        let file_id = entries
            .into_iter()
            .next()
            .context("capture upload response was empty")?
            .id;

        log::debug!("capture uploaded: file_id={file_id} filename={filename}");

        return Ok(file_id);
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no capture upload endpoint candidates")))
}

/// Content extracted from a changed file, attached to a file event so the
/// user's AI can remember the file body — not just that it changed.
#[derive(Debug, Default, Serialize)]
pub struct FileEventContent {
    pub markdown: Option<String>,
    pub section_count: usize,
    pub extractor: Option<String>,
    pub content_truncated: bool,
    /// Set when extraction of a *supported* format failed, so the server can
    /// tell "no content because unsupported" (this is `None`) from "no
    /// content because extraction broke" (this carries the error).
    pub extraction_error: Option<String>,
}

/// Ship a single file-activity event to CI-Server.
///
/// File events use CI-Server's unified capture ingest endpoint (canonical
/// `POST /v1/events`, with `/api/v1/events` as a compatibility fallback) with
/// converted Markdown carried in the event payload.
pub async fn ingest_file_event(
    config: &AppConfig,
    device_id: Uuid,
    session_id: Uuid,
    event: &crate::watcher::types::NormalizedEvent,
    content: &FileEventContent,
) -> Result<()> {
    let path = event
        .new_path
        .as_ref()
        .or(event.old_path.as_ref())
        .map(|p| p.display().to_string());
    let cwd = event
        .new_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|p| p.display().to_string());

    let payload = serde_json::json!({
        "device_id": device_id,
        "session_id": session_id,
        "event": serde_json::to_value(event).unwrap_or(Value::Null),
        "content": serde_json::to_value(content).unwrap_or(Value::Null),
        "cwd": cwd,
    });

    post_capture_event(
        config,
        CaptureEvent {
            source_system: "ci-applet",
            source_id: event.event_id.clone(),
            kind: "file_event".to_string(),
            occurred_at: event.occurred_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            observed_at: Some(event.observed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            title: path,
            description: Some(format!("{:?}", event.event_type)),
            payload,
        },
    )
    .await
}

pub async fn ingest_vizier_event(
    config: &AppConfig,
    device_id: Uuid,
    event: &VizierEventRecord,
) -> Result<()> {
    let payload = serde_json::json!({
        "device_id": device_id,
        "session_id": event.session_id,
        "snapshot_kind": event.event_kind,
        "monotonic_ms": event.monotonic_ms,
        "payload": event.payload,
        "focus_app": event.focus_app,
        "focus_title": event.focus_title,
        "cwd": event.cwd,
        "fs_event_count": event.fs_event_count,
    });

    post_capture_event(
        config,
        CaptureEvent {
            source_system: "ci-applet",
            source_id: format!(
                "vizier:{}:{}:{}",
                event.session_id, event.monotonic_ms, event.event_kind
            ),
            kind: format!("vizier_{}", event.event_kind),
            occurred_at: event.observed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            observed_at: Some(event.observed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            title: event
                .focus_title
                .clone()
                .or_else(|| event.focus_app.clone()),
            description: event.focus_app.clone(),
            payload,
        },
    )
    .await
}

// ── Internal ──────────────────────────────────────────────────────────────────

async fn post_capture_event(config: &AppConfig, event: CaptureEvent) -> Result<()> {
    let credential = AuthCredential::from_config(config)?;
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building event ingest client")?;
    let request = IngestEventsRequest {
        events: vec![event],
    };

    let mut last_error = None;
    for url in events_urls(&config.server.portal_url)? {
        let response = credential
            .apply(client.post(url.clone()))
            .json(&request)
            .send()
            .await
            .with_context(|| format!("sending event ingest request to {url}"))?;

        let status = response.status();
        if should_try_next_endpoint(status) {
            let body = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!("event ingest failed with HTTP {status}: {body}"));
            continue;
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("event ingest failed with HTTP {status}: {body}"));
        }

        let body: IngestEventsResponse = response
            .json()
            .await
            .context("decoding event ingest response")?;
        if let Some(rejected) = body.rejected.first() {
            return Err(anyhow!(
                "event ingest rejected event {}: {}",
                rejected.index,
                rejected.reason
            ));
        }

        log::debug!(
            "event ingest accepted: ingested={} duplicate={}",
            body.ingested,
            body.duplicate
        );

        return Ok(());
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no event ingest endpoint candidates")))
}

fn capture_upload_form(
    filename: &str,
    png_bytes: &[u8],
    inventory: bool,
    model: Option<&str>,
) -> Result<reqwest::multipart::Form> {
    let part = reqwest::multipart::Part::bytes(png_bytes.to_vec())
        .file_name(filename.to_string())
        .mime_str("image/png")
        .context("setting MIME type on capture upload part")?;

    let mut form = reqwest::multipart::Form::new()
        .part("files", part)
        .text("inventoryCapture", if inventory { "true" } else { "false" });
    // Only forward a model when we want CI-Server to run its own (Ollama)
    // analysis — sending the applet's own vision-model name would make the
    // server reject the job. When the applet analyses locally, omit it.
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        form = form.text("model", model.to_string());
    }
    Ok(form)
}

/// Set the title and/or description on an uploaded file. Used when the applet
/// ran its own vision analysis (its configured endpoint) and wants that result
/// stored on the file instead of CI-Server's Ollama description.
pub async fn set_file_metadata(
    config: &AppConfig,
    file_id: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> Result<()> {
    if title.is_none() && description.is_none() {
        return Ok(());
    }
    let credential = AuthCredential::from_config(config)?;
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building file metadata client")?;

    let mut body = serde_json::Map::new();
    if let Some(title) = title {
        body.insert("title".into(), Value::String(title.to_string()));
    }
    if let Some(description) = description {
        body.insert("description".into(), Value::String(description.to_string()));
    }
    let body = Value::Object(body);

    let mut last_error = None;
    for url in files_urls(&config.server.portal_url, file_id)? {
        let response = credential
            .apply(client.patch(url.clone()))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("sending file metadata update to {url}"))?;

        let status = response.status();
        if should_try_next_endpoint(status) {
            let text = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!("file metadata update failed with HTTP {status}: {text}"));
            continue;
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("file metadata update failed with HTTP {status}: {text}"));
        }
        return Ok(());
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no file metadata endpoint candidates")))
}

fn should_try_next_endpoint(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    )
}
