use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::VizierEventRecord;
use crate::server_common::{graphql_url, server_origin};

const INGEST_USER_SNAPSHOT_MUTATION: &str = r#"
mutation IngestUserSnapshot($input: IngestUserSnapshotInput!) {
  ingestUserSnapshot(input: $input) {
    success
    errorMessage
  }
}
"#;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct GraphqlRequest<TVariables> {
    query: &'static str,
    variables: TVariables,
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse<TData> {
    data: Option<TData>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VizierVariables {
    input: VizierEventInput,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VizierEventInput {
    device_id: String,
    session_id: String,
    snapshot_kind: String,
    observed_at: String,
    monotonic_ms: i64,
    payload: Value,
    focus_app: Option<String>,
    focus_title: Option<String>,
    cwd: Option<String>,
    fs_event_count: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VizierEnvelope {
    ingest_user_snapshot: VizierIngestPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VizierIngestPayload {
    success: bool,
    error_message: Option<String>,
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
) -> Result<String> {
    let auth_token = config
        .server
        .applet_auth_token
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .context("applet auth token is not configured")?;

    let upload_url = server_origin(&config.server.portal_url)?
        .join("/api/files/upload")
        .context("building capture upload URL")?;

    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("building capture upload client")?;

    let part = reqwest::multipart::Part::bytes(png_bytes.to_vec())
        .file_name(filename.to_string())
        .mime_str("image/png")
        .context("setting MIME type on capture upload part")?;

    let form = reqwest::multipart::Form::new()
        .part("files", part)
        .text("inventoryCapture", "true")
        .text("model", config.analysis.model.clone());

    let response = client
        .post(upload_url)
        .bearer_auth(auth_token)
        .multipart(form)
        .send()
        .await
        .context("sending capture upload request")?;

    let status = response.status();
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

    Ok(file_id)
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
/// CI-Server has no dedicated file-event endpoint today, so we ride the
/// proven `ingestUserSnapshot` channel the vizier lane already uses:
/// `snapshot_kind = "file_event"` with the normalized event (and any
/// converted Markdown) carried in `payload`. This keeps the applet working
/// against the same server it already talks to — when a first-class
/// `/history/file-events` endpoint lands, only this function changes.
pub async fn ingest_file_event(
    config: &AppConfig,
    device_id: Uuid,
    session_id: Uuid,
    event: &crate::watcher::types::NormalizedEvent,
    content: &FileEventContent,
) -> Result<()> {
    let client = build_gql_client(config)?;

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
        "event": serde_json::to_value(event).unwrap_or(Value::Null),
        "content": serde_json::to_value(content).unwrap_or(Value::Null),
    });

    let response = graphql::<_, VizierEnvelope>(
        &client,
        GraphqlRequest {
            query: INGEST_USER_SNAPSHOT_MUTATION,
            variables: VizierVariables {
                input: VizierEventInput {
                    device_id: device_id.to_string(),
                    session_id: session_id.to_string(),
                    snapshot_kind: "file_event".to_string(),
                    observed_at: event.observed_at.to_rfc3339(),
                    monotonic_ms: 0,
                    payload,
                    focus_app: Some("ci-applet/file-tracker".to_string()),
                    focus_title: path,
                    cwd,
                    fs_event_count: 1,
                },
            },
        },
    )
    .await?;

    let payload = response.ingest_user_snapshot;
    if !payload.success {
        return Err(anyhow!(
            "{}",
            payload
                .error_message
                .unwrap_or_else(|| "Applet file-event ingest failed.".to_string())
        ));
    }

    Ok(())
}

pub async fn ingest_vizier_event(
    config: &AppConfig,
    device_id: Uuid,
    event: &VizierEventRecord,
) -> Result<()> {
    let client = build_gql_client(config)?;
    let response = graphql::<_, VizierEnvelope>(
        &client,
        GraphqlRequest {
            query: INGEST_USER_SNAPSHOT_MUTATION,
            variables: VizierVariables {
                input: VizierEventInput {
                    device_id: device_id.to_string(),
                    session_id: event.session_id.to_string(),
                    snapshot_kind: event.event_kind.clone(),
                    observed_at: event.observed_at.to_rfc3339(),
                    monotonic_ms: event.monotonic_ms,
                    payload: event.payload.clone(),
                    focus_app: event.focus_app.clone(),
                    focus_title: event.focus_title.clone(),
                    cwd: event.cwd.clone(),
                    fs_event_count: event.fs_event_count,
                },
            },
        },
    )
    .await?;

    let payload = response.ingest_user_snapshot;
    if !payload.success {
        return Err(anyhow!(
            "{}",
            payload
                .error_message
                .unwrap_or_else(|| "Applet vizier ingest failed.".to_string())
        ));
    }

    Ok(())
}

// ── Internal ──────────────────────────────────────────────────────────────────

struct GraphqlClient {
    client: Client,
    url: Url,
    auth_token: String,
}

fn build_gql_client(config: &AppConfig) -> Result<GraphqlClient> {
    let auth_token = config
        .server
        .applet_auth_token
        .clone()
        .filter(|token| !token.trim().is_empty())
        .context("server applet auth token is not configured")?;
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building ingest GraphQL client")?;

    Ok(GraphqlClient {
        client,
        url: graphql_url(&config.server.portal_url)?,
        auth_token,
    })
}

async fn graphql<TVariables, TData>(
    client: &GraphqlClient,
    request: GraphqlRequest<TVariables>,
) -> Result<TData>
where
    TVariables: Serialize,
    TData: for<'de> Deserialize<'de>,
{
    let response = client
        .client
        .post(client.url.clone())
        .bearer_auth(&client.auth_token)
        .json(&request)
        .send()
        .await
        .context("sending GraphQL ingest request")?;

    let status = response.status();
    let body: GraphqlResponse<TData> = response
        .json()
        .await
        .context("decoding GraphQL ingest response")?;

    if !status.is_success() {
        let message = body
            .errors
            .as_ref()
            .and_then(|errors| errors.first())
            .and_then(|error| error.message.clone())
            .unwrap_or_else(|| format!("GraphQL ingest failed with HTTP {status}."));
        return Err(anyhow!(message));
    }

    if let Some(error) = body
        .errors
        .as_ref()
        .and_then(|errors| errors.first())
        .and_then(|error| error.message.clone())
    {
        return Err(anyhow!(error));
    }

    body.data.context("GraphQL ingest response missing data")
}
