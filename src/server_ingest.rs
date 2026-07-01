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
/// The server runs its inventory pipeline over the uploaded image
/// (`inventoryCapture=true`). When `skip_description` is true the applet has
/// generated its own title/description (via its local vision endpoint) and will
/// write them with [`patch_file_description`], so the upload sets
/// `skipDescription=true` to suppress the server's redundant Ollama description
/// pass. When false, the server describes the image as before. Returns the
/// server-assigned file ID on success.
pub async fn ingest_capture(
    config: &AppConfig,
    filename: &str,
    png_bytes: &[u8],
    skip_description: bool,
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

    let mut form = reqwest::multipart::Form::new()
        .part("files", part)
        .text("inventoryCapture", "true")
        .text("model", config.analysis.model.clone());

    if skip_description {
        form = form.text("skipDescription", "true");
    }

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

/// Write applet-generated title/description to an uploaded file via
/// `PATCH /files/:id`.
///
/// Used on the self-analyzed capture path, where the upload set
/// `skipDescription=true` so the server did not run its own image description.
/// The server accepts either or both of `title`/`description`; passing neither
/// is treated as a no-op here rather than sent as an empty request.
pub async fn patch_file_description(
    config: &AppConfig,
    file_id: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> Result<()> {
    let mut body = serde_json::Map::new();
    if let Some(title) = title {
        body.insert("title".to_string(), Value::String(title.to_string()));
    }
    if let Some(description) = description {
        body.insert(
            "description".to_string(),
            Value::String(description.to_string()),
        );
    }
    if body.is_empty() {
        return Ok(());
    }

    let auth_token = config
        .server
        .applet_auth_token
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .context("applet auth token is not configured")?;

    let patch_url = server_origin(&config.server.portal_url)?
        .join(&format!("/api/files/{file_id}"))
        .context("building file metadata patch URL")?;

    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("building file metadata patch client")?;

    let response = client
        .patch(patch_url)
        .bearer_auth(auth_token)
        .json(&Value::Object(body))
        .send()
        .await
        .context("sending file metadata patch request")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "file metadata patch failed with HTTP {status}: {body}"
        ));
    }

    log::debug!("capture description patched: file_id={file_id}");

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
