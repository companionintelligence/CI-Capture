use anyhow::{Context, Result, anyhow};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::VizierEventRecord;
use crate::error_body::{error_body_suffix, error_code_suffix};
use crate::server_common::{AuthCredential, events_urls, files_urls, upload_urls};

// ── Types ─────────────────────────────────────────────────────────────────────

/// One row of CI-Server's `POST /v1/events` envelope. `pub(crate)` so lanes
/// whose per-row mapping is large enough to deserve its own module (the
/// `knowledge` lane's per-stream table) can build events themselves and hand
/// them to [`post_capture_events`]; the smaller lanes keep their mapping in
/// the `ingest_*` functions below.
#[derive(Debug, Serialize)]
pub(crate) struct CaptureEvent {
    pub(crate) source_system: &'static str,
    pub(crate) source_id: String,
    pub(crate) kind: String,
    pub(crate) occurred_at: String,
    /// When the event ended — CI-Server materializes it as `events.end_time`
    /// and derives `duration_minutes` from it. Only the `knowledge` lane's
    /// interval streams (app usage, Bluetooth spans, …) carry one today.
    /// Same zod-null trap as `description` below: omitted, never `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) observed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// CI-Server's `/v1/events` schema types this `z.string().optional()`,
    /// which — a common zod trap — accepts an *absent* key but rejects an
    /// explicit JSON `null` ("Expected string, received null"). `None` must
    /// therefore be omitted, not serialized, same as `capture_tick_id`
    /// below. Found live: an iMessage row with `text IS NULL` (see
    /// `imessage::reader`'s `attributedBody` gap) sent `description: null`
    /// and every such message in the batch was rejected — silently, until
    /// something actually read the response body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) payload: Value,
    /// Per-tick correlator shared with the screenshot uploaded via the same
    /// worker tick (see `WorkerState::capture_once`'s `captureTickId` form
    /// field) — lets CI-Server link `ci.events` back to the `ci.files` row it
    /// was captured alongside (`ci.events.capture_tick_id` /
    /// `ci.files.capture_tick_id`; CI-Server migration
    /// 20260830042159_add_capture_correlation_to_files_and_events). Only
    /// vizier events carry one today.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) capture_tick_id: Option<String>,
}

/// Borrows the slice so a caller can bisect a failing batch by re-posting
/// sub-ranges of the same owned `Vec` without cloning events.
#[derive(Debug, Serialize)]
struct IngestEventsRequest<'a> {
    events: &'a [CaptureEvent],
}

/// What CI-Server acknowledged for one `POST /v1/events`. Never a partial
/// success: the server's `rejected[]` path always answers `ingested: 0` (see
/// [`post_capture_events`]), so a returned ack means every event in the
/// request is now either stored or a known duplicate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BatchAck {
    pub(crate) ingested: u32,
    pub(crate) duplicate: u32,
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

/// An event-ingest failure, classified for the retry-vs-dead-letter decision.
///
/// `permanent = true` means the server rejected *this request's payload*
/// (validation 4xx) — retrying the same bytes can never succeed, so the
/// caller should park it. Everything that can heal without changing the
/// event is transient: transport errors, timeouts, 5xx, missing routes
/// (server too old), auth failures (401/403 — the token refresh loop fixes
/// those; the event is not at fault), and an HTTP 200 whose body lists
/// `rejected[]` entries — CI-Server reports a failed *materialize* step that
/// way, having already compensated its capture log so the same events can be
/// re-sent verbatim (`BulkIngestController.ts` `envelopeResponse`). That
/// last one is transient by *classification*, not by nature: the body does
/// not say whether the materialize step tripped over the server's own state
/// (a database blip) or over one row of this payload (a column constraint),
/// so the client cannot tell a one-off from a poison. A repeating
/// `rejected[]` is bounded by the knowledge lane's head-of-line valve, which
/// bisects the chunk after `STALL_AFTER` and parks the one event that still
/// fails on its own.
#[derive(Debug)]
pub struct IngestError {
    pub permanent: bool,
    /// True when the server answered (any HTTP status, or a 200 whose body
    /// lists `rejected[]`); false when the request never got a reply —
    /// transport failure, timeout, DNS, a client that could not even be
    /// built. The knowledge lane's head-of-line valve keys on this: a
    /// transient verdict the *server* gave is evidence about the event at
    /// the head, an outage is not, and the two must not be confused or a
    /// day-long outage would mature into a park of a healthy event.
    pub reached_server: bool,
    message: String,
}

impl IngestError {
    /// A verdict on the payload itself — nothing to reach, so it counts as
    /// answered (the local check stands in for the server's).
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            permanent: true,
            reached_server: true,
            message: message.into(),
        }
    }

    /// A failure with no reply from the server at all.
    fn transient(message: impl Into<String>) -> Self {
        Self {
            permanent: false,
            reached_server: false,
            message: message.into(),
        }
    }

    /// Classify an HTTP error status. Permanent is an **allowlist** of
    /// statuses that unambiguously indict the payload itself — every other
    /// status (auth, routing, proxy/env 4xx like 407/421/425, rate limits,
    /// 5xx) is transient, because retrying can't lose data (the 24 h parking
    /// valve still bounds head-of-queue liveness) but a wrong "permanent"
    /// verdict parks an event that would have gone through.
    ///
    /// The response body is reduced to an error code (see [`error_body_suffix`])
    /// before it enters the message. This message is what `file_tracker` writes
    /// into the queue DB, and the request it describes carries the changed
    /// file's absolute path and up to [`DESCRIPTION_CONTENT_BYTES`] of its
    /// text — a validation 4xx that quotes the offending field would otherwise
    /// persist the document.
    fn from_status(status: StatusCode, body: &str, url: &reqwest::Url) -> Self {
        let message = format!(
            "event ingest to {url} failed with HTTP {status}{}",
            error_body_suffix(body)
        );
        let permanent = matches!(
            status,
            StatusCode::BAD_REQUEST                     // validation rejection
                | StatusCode::PAYLOAD_TOO_LARGE         // this payload will never fit
                | StatusCode::UNSUPPORTED_MEDIA_TYPE
                | StatusCode::UNPROCESSABLE_ENTITY // schema rejection
        );
        Self {
            permanent,
            reached_server: true,
            message,
        }
    }
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({})",
            self.message,
            if self.permanent {
                "permanent"
            } else {
                "transient"
            }
        )
    }
}

impl std::error::Error for IngestError {}

// ── Public API ─────────────────────────────────────────────────────────────────

/// Format a Unix-millisecond instant the one way CI-Server's `/v1/events`
/// accepts. The schema validates every timestamp with `z.string().datetime()`
/// and no `{ offset: true }`, which admits only the literal `Z` suffix —
/// `+00:00` and `-07:00` both fail validation and, because the request is
/// all-or-nothing, 400 the entire batch. Hence the parameter is a UTC
/// instant and the formatter is `use_z = true`; chrono only substitutes `Z`
/// when the offset is exactly zero, so a `DateTime<FixedOffset>` must never
/// reach this function without `.to_utc()` first.
///
/// `None` when the value is outside chrono's representable range — callers
/// treat that as an unreadable row (log and skip), never as "now".
pub(crate) fn wire_ts(unix_ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(unix_ms).map(wire_ts_utc)
}

/// [`wire_ts`] for an instant that is already a `DateTime<Utc>`.
pub(crate) fn wire_ts_utc(instant: chrono::DateTime<chrono::Utc>) -> String {
    instant.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn is_configured(config: &AppConfig) -> bool {
    config
        .server
        .applet_auth_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
}

/// Upload a captured screenshot to the server via `POST /files/upload`.
///
/// `inventory` toggles CI-Server's inventory pipeline (`inventoryCapture`).
/// `skip_description=true` sends `skipDescription=true` so CI-Server skips its
/// own Ollama image description — used when the applet analysed the image
/// locally and will write the title/description itself via [`set_file_metadata`].
/// (Since CI-Server #1615, `inventoryCapture=false` no longer suppresses the
/// description, so it must be requested explicitly.) Returns the server-assigned
/// file ID on success.
pub async fn ingest_capture(
    config: &AppConfig,
    filename: &str,
    png_bytes: &[u8],
    inventory: bool,
    skip_description: bool,
    model: Option<&str>,
) -> Result<String> {
    let credential = AuthCredential::from_config(config)?;

    // Never inherit an ambient proxy. reqwest honours HTTP_PROXY / HTTPS_PROXY /
    // ALL_PROXY from the environment by default, which would silently route screen
    // captures and window titles through a third party — and terminate TLS there
    // for a plain-http server. A capture client whose whole claim is "your data
    // reaches only your own machines" cannot let an inherited shell export
    // override that. Every client in this crate sets `.no_proxy()` for the same
    // reason; if proxy support is ever wanted it belongs in config as an explicit,
    // visible field.
    let client = Client::builder()
        .no_proxy()
        .use_rustls_tls()
        .build()
        .context("building capture upload client")?;

    let mut last_error = None;
    for upload_url in upload_urls(&config.server.portal_url)? {
        let form = capture_upload_form(filename, png_bytes, inventory, skip_description, model)?;
        let response = credential
            .apply(client.post(upload_url.clone()))
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("sending capture upload request to {upload_url}"))?;

        let status = response.status();
        if should_try_next_endpoint(status) {
            let body = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!(
                "capture upload failed with HTTP {status}{}",
                error_body_suffix(&body)
            ));
            continue;
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "capture upload failed with HTTP {status}{}",
                error_body_suffix(&body)
            ));
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
) -> Result<(), IngestError> {
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
            occurred_at: wire_ts_utc(event.occurred_at),
            ended_at: None,
            observed_at: Some(wire_ts_utc(event.observed_at)),
            title: path,
            description: Some(file_event_description(event.event_type, content)),
            payload,
            capture_tick_id: None,
        },
    )
    .await
}

/// Cap on extracted content carried in the event `description`. The indexer
/// embeds the first chunk of the field; the full markdown still rides in the
/// payload for storage.
const DESCRIPTION_CONTENT_BYTES: usize = 4 * 1024;

/// Build the event `description`: the event type, then the extracted Markdown.
///
/// CI-Server's search indexer embeds/tsvectors only top-level fields
/// (`title`, `description`, …) — anything in `payload`/`metadata` is stored
/// but invisible to recall. Carrying the content here is what makes the file
/// *body* retrievable by the companion, not just the fact that a path changed.
fn file_event_description(
    event_type: crate::watcher::types::EventType,
    content: &FileEventContent,
) -> String {
    let mut description = format!("{event_type:?}");
    if let Some(markdown) = content.markdown.as_deref() {
        let trimmed = markdown.trim();
        if !trimmed.is_empty() {
            let mut end = trimmed.len().min(DESCRIPTION_CONTENT_BYTES);
            while !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            description.push_str("\n\n");
            description.push_str(&trimmed[..end]);
            if end < trimmed.len() {
                description.push_str("\n…[truncated]");
            }
        }
    }
    description
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
            occurred_at: wire_ts_utc(event.observed_at),
            ended_at: None,
            observed_at: Some(wire_ts_utc(event.observed_at)),
            title: event
                .focus_title
                .clone()
                .or_else(|| event.focus_app.clone()),
            description: event.focus_app.clone(),
            payload,
            capture_tick_id: event.capture_tick_id.map(|id| id.to_string()),
        },
    )
    .await
    // The worker parks a failed send on disk and retries from the store
    // sweep, so flatten to anyhow the same way screenshot ingest does.
    .map_err(anyhow::Error::from)
}

/// Ship one BlueBubbles message to CI-Server via the generic capture-event
/// pipeline (see `crate::bluebubbles`'s module doc for the known gap this
/// leaves — it lands as an event blob, not a `ci.communications` row).
///
/// `title`/`description` (the two fields CI-Server's search indexer actually
/// embeds — see `file_event_description`'s doc comment above for the same
/// reasoning applied to file events) carry the chat name and message text.
/// Message *content* therefore reaches the server and, from there, disk and
/// logs on that machine — same trust boundary as every other capture lane,
/// but worth being explicit about for a lane that carries someone else's
/// words, not just this device's own activity.
pub async fn ingest_bluebubbles_message(
    config: &AppConfig,
    device_id: Uuid,
    message: &crate::bluebubbles::client::BlueBubblesMessage,
) -> Result<(), IngestError> {
    let chat = message.chats.first();
    // Never `unwrap_or_else(Utc::now)`: stamping an unreadable timestamp with
    // the current time gets the event acknowledged, which advances the caller's
    // cursor past it — a garbage value would then silently redate the message
    // and, in the imessage lane, strand every real row behind it.
    let occurred_at = message.date_created.and_then(wire_ts).ok_or_else(|| {
        IngestError::permanent(format!(
            "BlueBubbles message {} has no usable dateCreated ({:?})",
            message.guid, message.date_created
        ))
    })?;

    let title = chat
        .and_then(|c| c.display_name.clone().or_else(|| c.chat_identifier.clone()))
        .or_else(|| message.handle.as_ref().map(|h| h.address.clone()));

    let payload = serde_json::json!({
        "device_id": device_id,
        "guid": message.guid,
        "chat_guid": chat.map(|c| c.guid.clone()),
        "is_from_me": message.is_from_me,
        "handle": message.handle,
        "attachments": message.attachments,
    });

    post_capture_event(
        config,
        CaptureEvent {
            source_system: "ci-applet",
            source_id: format!("bluebubbles:{}", message.guid),
            kind: "bluebubbles_message".to_string(),
            occurred_at: occurred_at.clone(),
            ended_at: None,
            observed_at: Some(occurred_at),
            title,
            description: message.text.clone(),
            payload,
            capture_tick_id: None,
        },
    )
    .await
}

/// Ship one locally-read iMessage/SMS row to CI-Server via the generic
/// capture-event pipeline. Structurally close to `ingest_bluebubbles_message`
/// (same underlying kind of data, same transport) but kept separate and
/// tagged with a distinct `kind`/`source_id` prefix — this row came from
/// reading a file on disk, not from a network API, and that provenance is
/// worth keeping distinguishable server-side. See `crate::imessage`'s module
/// doc for the same message-content-reaches-the-server caveat
/// `ingest_bluebubbles_message` documents above.
pub async fn ingest_imessage_message(
    config: &AppConfig,
    device_id: Uuid,
    row: &crate::imessage::reader::ImessageRow,
) -> Result<(), IngestError> {
    // `reader::read_messages` already refuses to emit a row whose timestamp
    // cannot be represented, so this is a belt-and-braces guard — but an
    // explicit error rather than `Utc::now()` for the reason spelled out on
    // `ingest_bluebubbles_message` above.
    let occurred_at = wire_ts(row.date_created_unix_ms).ok_or_else(|| {
        IngestError::permanent(format!(
            "iMessage rowid={} has an unrepresentable timestamp ({}ms)",
            row.rowid, row.date_created_unix_ms
        ))
    })?;

    let title = row
        .chat_display_name
        .clone()
        .or_else(|| row.chat_identifier.clone())
        .or_else(|| row.handle.clone());

    let payload = serde_json::json!({
        "device_id": device_id,
        "rowid": row.rowid,
        "chat_identifier": row.chat_identifier,
        "is_from_me": row.is_from_me,
        "handle": row.handle,
        "service": row.service,
        "attachment_count": row.attachment_count,
        // Chat membership, so a group thread arrives with the people in it
        // rather than just a name. Outbound messages have no `handle`, so
        // without this they carried no recipient at all.
        "participants": row.participants,
        // Threading and edit state. A reply that flattens into the main thread
        // and an edit presented as the original are both silent corruptions of
        // what was actually said.
        "thread_originator_guid": row.thread_originator_guid,
        "associated_message_guid": row.associated_message_guid,
        "date_edited": row.date_edited_unix_ms,
        "date_retracted": row.date_retracted_unix_ms,
    });

    post_capture_event(
        config,
        CaptureEvent {
            source_system: "ci-applet",
            source_id: format!(
                "imessage:{}",
                row.guid.clone().unwrap_or_else(|| row.rowid.to_string())
            ),
            kind: "imessage_message".to_string(),
            occurred_at: occurred_at.clone(),
            ended_at: None,
            observed_at: Some(occurred_at),
            title,
            description: row.text.clone(),
            payload,
            capture_tick_id: None,
        },
    )
    .await
}

// ── Internal ──────────────────────────────────────────────────────────────────

async fn post_capture_event(config: &AppConfig, event: CaptureEvent) -> Result<(), IngestError> {
    post_capture_events(config, std::slice::from_ref(&event))
        .await
        .map(|_| ())
}

/// Ship a batch of events in one `POST /v1/events`. The server's contract
/// makes this all-or-nothing:
///
/// - a validation failure on *any* event is an HTTP 400 for the whole
///   request and nothing is stored (`permanent` — the caller bisects to find
///   the offending event);
/// - an HTTP 200 whose `rejected[]` is non-empty means the materialize step
///   failed after the capture log was written: `ingested` is always 0, every
///   non-duplicate index is listed with the same reason, and the server has
///   compensated its log so re-sending the identical batch is safe
///   (`transient` — the caller retries next tick);
/// - otherwise every event is stored or a known duplicate, and the returned
///   [`BatchAck`] says which.
///
/// An empty slice is answered locally with an empty ack: the schema requires
/// `events.min(1)`, so posting it would be a 400 the caller would misread as
/// a payload rejection. At most 1000 events per request (`events.max(1000)`).
pub(crate) async fn post_capture_events(
    config: &AppConfig,
    events: &[CaptureEvent],
) -> Result<BatchAck, IngestError> {
    if events.is_empty() {
        return Ok(BatchAck::default());
    }
    debug_assert!(events.len() <= 1000, "CI-Server caps /v1/events at 1000");

    // Setup failures (no credential, bad portal URL) are config problems, not
    // event problems — always transient.
    let credential =
        AuthCredential::from_config(config).map_err(|e| IngestError::transient(e.to_string()))?;
    let client = Client::builder()
        .no_proxy()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| IngestError::transient(format!("building event ingest client: {e}")))?;
    let request = IngestEventsRequest { events };

    let mut last_error = None;
    for url in
        events_urls(&config.server.portal_url).map_err(|e| IngestError::transient(e.to_string()))?
    {
        let response = match credential
            .apply(client.post(url.clone()))
            .json(&request)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Transport error — try the next candidate; whole call is
                // transient if none answers.
                last_error = Some(IngestError::transient(format!(
                    "sending event ingest request to {url}: {e}"
                )));
                continue;
            }
        };

        let status = response.status();
        if should_try_next_endpoint(status) {
            let body = response.text().await.unwrap_or_default();
            last_error = Some(IngestError::from_status(status, &body, &url));
            continue;
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(IngestError::from_status(status, &body, &url));
        }

        let body: IngestEventsResponse = response
            .json()
            .await
            .map_err(|e| IngestError::transient(format!("decoding event ingest response: {e}")))?;
        if !body.rejected.is_empty() {
            return Err(rejected_batch_error(&body, events.len()));
        }

        log::debug!(
            "event ingest accepted: ingested={} duplicate={}",
            body.ingested,
            body.duplicate
        );

        return Ok(BatchAck {
            ingested: body.ingested,
            duplicate: body.duplicate,
        });
    }

    Err(last_error.unwrap_or_else(|| IngestError::transient("no event ingest endpoint candidates")))
}

/// The error for an HTTP 200 that carries `rejected[]`. Transient, not
/// permanent: CI-Server's `envelopeResponse` (`BulkIngestController.ts`)
/// only produces this list from a `BulkMaterializeError`, which wraps *any*
/// throw from the materialize step, lists every fresh index with one shared
/// reason, and has already deleted the matching capture-log rows — so the
/// same bytes are always safe to re-send. Whether they will *succeed* the
/// body cannot say: the throw may be the server's own (a database blip
/// that clears) or a row-level rejection of one event in this batch (a
/// column constraint the schema validator does not pre-check), and the two
/// arrive identical. Calling every one permanent would park a whole batch
/// of good events on the first blip; calling every one transient leaves a
/// poison row to the knowledge lane's head-of-line valve, which bisects the
/// stalled chunk after `STALL_AFTER` and parks only the event that fails
/// alone — the same bound a server that 500s on one event gets. `reason`
/// is the server's free text about *our* event, so it only survives when it
/// is code-shaped; the counts always do.
fn rejected_batch_error(body: &IngestEventsResponse, sent: usize) -> IngestError {
    let first = &body.rejected[0];
    IngestError {
        permanent: false,
        // The server did answer — with a 200 — so this is a verdict the
        // head-of-line valve may count, unlike a transport failure.
        reached_server: true,
        message: format!(
            "event ingest rejected {} of {} event(s) for re-send (first index {}){}",
            body.rejected.len(),
            sent,
            first.index,
            error_code_suffix(&first.reason)
        ),
    }
}

fn capture_upload_form(
    filename: &str,
    png_bytes: &[u8],
    inventory: bool,
    skip_description: bool,
    model: Option<&str>,
) -> Result<reqwest::multipart::Form> {
    let part = reqwest::multipart::Part::bytes(png_bytes.to_vec())
        .file_name(filename.to_string())
        .mime_str("image/png")
        .context("setting MIME type on capture upload part")?;

    let mut form = reqwest::multipart::Form::new()
        .part("files", part)
        .text("inventoryCapture", if inventory { "true" } else { "false" });
    // Explicitly suppress CI-Server's own Ollama image description when the
    // applet already produced its own title/description locally (written via
    // `set_file_metadata`). Required since CI-Server #1615, where
    // `inventoryCapture=false` no longer implies "skip description".
    if skip_description {
        form = form.text("skipDescription", "true");
    }
    // Only forward a model when we want CI-Server to run its own (Ollama)
    // analysis — sending the applet's own vision-model name would make the
    // server reject the job. When the applet analyses locally, omit it.
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        form = form.text("model", model.to_string());
    }
    // The per-tick correlator rides as the UUID segment of `filename` (see
    // `capture_once` in worker.rs) rather than as its own parameter threaded
    // through every call site — that's what lets it survive the disk-backed
    // retry sweep, which only ever recovers a filename and bytes. Shared with
    // the vizier (desktop-activity) event sent via the same worker tick (see
    // `ingest_vizier_event`'s `capture_tick_id`), so CI-Server can link this
    // `ci.files` row back to the `ci.events` row it was captured alongside
    // (CI-Server migration 20260830042159_add_capture_correlation_to_files_and_events
    // / FilesController's `captureTickId` body field). Best effort: a
    // filename that doesn't parse (e.g. one already on disk from before this
    // field existed) just omits the correlator.
    if let Some(tick_id) = capture_tick_id_from_filename(filename) {
        form = form.text("captureTickId", tick_id.to_string());
    }
    Ok(form)
}

/// Recovers the per-tick correlator UUID that `WorkerState::capture_once`
/// (worker.rs) encodes as the trailing segment of `capture_{timestamp}_{uuid}.png`.
fn capture_tick_id_from_filename(filename: &str) -> Option<Uuid> {
    let stem = filename.strip_prefix("capture_")?.strip_suffix(".png")?;
    let (_, uuid_part) = stem.rsplit_once('_')?;
    Uuid::parse_str(uuid_part).ok()
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
        .no_proxy()
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
            last_error = Some(anyhow!(
                "file metadata update failed with HTTP {status}{}",
                error_body_suffix(&text)
            ));
            continue;
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "file metadata update failed with HTTP {status}{}",
                error_body_suffix(&text)
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::types::EventType;

    fn content(markdown: Option<&str>) -> FileEventContent {
        FileEventContent {
            markdown: markdown.map(|s| s.to_string()),
            ..FileEventContent::default()
        }
    }

    /// The exact filename shape `WorkerState::capture_once` (worker.rs)
    /// writes: `capture_{timestamp}_{capture_tick_id}.png`. Round-tripping
    /// this is what lets the correlator survive the disk-backed retry sweep,
    /// which only ever recovers a filename and bytes.
    #[test]
    fn capture_tick_id_from_filename_recovers_the_trailing_uuid() {
        let tick_id = Uuid::new_v4();
        let filename = format!("capture_20260830T120000_{tick_id}.png");

        assert_eq!(capture_tick_id_from_filename(&filename), Some(tick_id));
    }

    #[test]
    fn capture_tick_id_from_filename_is_best_effort_on_malformed_input() {
        // Missing prefix/suffix, or a non-UUID trailing segment — none of
        // these should panic; the correlator is simply omitted.
        assert_eq!(capture_tick_id_from_filename("not-a-capture.png"), None);
        assert_eq!(
            capture_tick_id_from_filename("capture_20260830T120000.png"),
            None
        );
        assert_eq!(
            capture_tick_id_from_filename("capture_20260830T120000_not-a-uuid.png"),
            None
        );
        assert_eq!(
            capture_tick_id_from_filename("capture_only_one_segment"),
            None
        );
    }

    #[test]
    fn description_is_event_type_when_no_content() {
        let d = file_event_description(EventType::FileModified, &content(None));
        assert_eq!(d, "FileModified");
        let d = file_event_description(EventType::FileDeleted, &content(Some("   ")));
        assert_eq!(d, "FileDeleted");
    }

    #[test]
    fn description_carries_extracted_markdown() {
        let d = file_event_description(
            EventType::FileModified,
            &content(Some("# Notes\n\nremember the quarterly plan")),
        );
        assert!(d.starts_with("FileModified\n\n# Notes"));
        assert!(d.contains("remember the quarterly plan"));
        assert!(!d.contains("[truncated]"));
    }

    #[test]
    fn classify_payload_rejections_as_permanent() {
        let url = reqwest::Url::parse("http://core/api/v1/events").unwrap();
        for status in [400u16, 413, 422] {
            let e = IngestError::from_status(StatusCode::from_u16(status).unwrap(), "bad", &url);
            assert!(e.permanent, "HTTP {status} should be permanent");
        }
    }

    #[test]
    fn classify_outages_auth_and_env_4xx_as_transient() {
        let url = reqwest::Url::parse("http://core/api/v1/events").unwrap();
        // Auth failures heal via token refresh; 404/405 mean the server is
        // too old; 408/429/5xx are the server's problem; 407/410/421/423/425
        // are environment/proxy states, not payload verdicts. None indict
        // the event payload — permanent is an allowlist, not a default.
        for status in [
            401u16, 403, 404, 405, 407, 408, 410, 421, 423, 425, 429, 500, 502, 503,
        ] {
            let e = IngestError::from_status(StatusCode::from_u16(status).unwrap(), "x", &url);
            assert!(!e.permanent, "HTTP {status} should be transient");
            assert!(e.reached_server, "HTTP {status} is an answer");
        }
        assert!(!IngestError::transient("net down").permanent);
        assert!(IngestError::permanent("rejected").permanent);
        // Only a reply from the server counts as reaching it: a transport
        // failure must not feed the knowledge lane's head-of-line clock.
        assert!(!IngestError::transient("net down").reached_server);
        assert!(IngestError::permanent("rejected").reached_server);
    }

    /// The exact shape of a CI-Server validation rejection that quotes the
    /// fields `ingest_file_event` sends: `title` is the changed file's absolute
    /// path, `description` is its extracted Markdown.
    fn rejection_quoting_the_document() -> String {
        serde_json::json!({
            "statusCode": 422,
            "error": "Unprocessable Entity",
            "message": [
                "title: /Users/liam/notes/q3-comp-review.md exceeds 255 characters",
                "description: # Q3 review\n\nsalary band 4 — liam.broza@gmail.com",
            ],
        })
        .to_string()
    }

    #[test]
    fn a_parked_events_persisted_reason_carries_no_document_text() {
        // This string is what `file_tracker` hands to `queue.dead_letter(…)`,
        // which writes it to `dead_letters.reason` — on disk, past process exit.
        let url = reqwest::Url::parse("http://core/api/v1/events").unwrap();
        let e = IngestError::from_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            &rejection_quoting_the_document(),
            &url,
        );
        let persisted = e.to_string();

        for secret in [
            "/Users/liam/notes",
            "q3-comp-review",
            "Q3 review",
            "salary band",
            "liam.broza@gmail.com",
        ] {
            assert!(
                !persisted.contains(secret),
                "leaked {secret:?}: {persisted}"
            );
        }
        // The diagnosis survives: status, endpoint, and the retry verdict.
        assert!(persisted.contains("422"), "{persisted}");
        assert!(
            persisted.contains("http://core/api/v1/events"),
            "{persisted}"
        );
        assert!(persisted.contains("(permanent)"), "{persisted}");
    }

    #[test]
    fn a_machine_readable_code_still_reaches_the_queue() {
        // Redaction must not cost the one field worth keeping.
        let url = reqwest::Url::parse("http://core/api/v1/events").unwrap();
        let e = IngestError::from_status(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"duplicate_source_id"}}"#,
            &url,
        );
        assert!(e.to_string().contains("duplicate_source_id"), "{e}");
    }

    #[test]
    fn a_rejected_batch_is_transient_and_drops_its_prose() {
        // CI-Server answers a failed materialize step with HTTP 200 and every
        // fresh index in `rejected[]`, having compensated its capture log —
        // so the verdict is "re-send", never "park", and the server's free
        // text about our event must not survive into the message.
        let body = IngestEventsResponse {
            ingested: 0,
            duplicate: 0,
            rejected: vec![RejectedEvent {
                index: 0,
                reason: "description quoted /Users/liam/notes/q3-comp-review.md".into(),
            }],
        };
        let err = rejected_batch_error(&body, 1);
        assert!(!err.permanent, "{err}");
        assert!(
            err.reached_server,
            "a 200 with rejected[] is the server's own verdict, not an outage"
        );
        let msg = err.to_string();
        assert!(!msg.contains("q3-comp-review"), "{msg}");
        assert!(msg.contains("rejected 1 of 1"), "{msg}");

        let coded = IngestEventsResponse {
            ingested: 0,
            duplicate: 2,
            rejected: vec![
                RejectedEvent {
                    index: 3,
                    reason: "duplicate_source_id".into(),
                },
                RejectedEvent {
                    index: 4,
                    reason: "duplicate_source_id".into(),
                },
            ],
        };
        let msg = rejected_batch_error(&coded, 7).to_string();
        assert_eq!(
            msg,
            "event ingest rejected 2 of 7 event(s) for re-send (first index 3) — duplicate_source_id (transient)"
        );
    }

    /// The schema requires `events.min(1)`; an empty POST would be a 400 the
    /// caller could only read as a payload rejection. Answered locally, and
    /// before any credential/URL setup so an unconfigured config is fine.
    #[tokio::test]
    async fn an_empty_batch_is_acked_locally_without_a_request() {
        let ack = post_capture_events(&AppConfig::default(), &[])
            .await
            .expect("empty batch");
        assert_eq!(ack, BatchAck::default());
    }

    /// `ended_at` rides the same zod-null trap as `description`: absent is
    /// fine, `null` fails the whole batch. `None` must therefore vanish from
    /// the wire, and `Some` must appear under the exact server key.
    #[test]
    fn ended_at_is_omitted_when_none_and_named_when_some() {
        let mut event = CaptureEvent {
            source_system: "ci-applet",
            source_id: "knowledge:test".into(),
            kind: "knowledge_app_usage".into(),
            occurred_at: "2026-09-14T07:00:00.000Z".into(),
            ended_at: None,
            observed_at: None,
            title: None,
            description: None,
            payload: serde_json::json!({}),
            capture_tick_id: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert!(wire.get("ended_at").is_none(), "{wire}");
        assert!(wire.get("description").is_none(), "{wire}");

        event.ended_at = Some("2026-09-14T07:12:00.000Z".into());
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["ended_at"], "2026-09-14T07:12:00.000Z");
        // The request envelope borrows a slice and serializes as `events: [...]`.
        let req = serde_json::to_value(IngestEventsRequest {
            events: std::slice::from_ref(&event),
        })
        .unwrap();
        assert_eq!(req["events"].as_array().map(Vec::len), Some(1));
    }

    /// CI-Server validates timestamps with `z.string().datetime()` and no
    /// `offset` option: only a trailing `Z` passes. This is the one formatter
    /// every lane routes through, so it is pinned here.
    #[test]
    fn wire_ts_is_utc_with_a_trailing_z() {
        let ts = wire_ts(1_789_456_800_000).expect("in range");
        assert_eq!(ts, "2026-09-15T07:20:00.000Z");
        assert!(ts.ends_with('Z'));
        assert!(
            wire_ts(i64::MAX).is_none(),
            "out-of-range must be None, not now()"
        );
    }

    #[test]
    fn a_rejected_file_metadata_update_leaks_no_screenshot_description() {
        // `set_file_metadata` sends the vision model's description of the
        // user's screen; a 400 that quotes it must not survive into the error.
        let body = serde_json::json!({
            "statusCode": 400,
            "message": "description invalid: 'debugging a payroll import; salary band 4 visible'",
        })
        .to_string();
        let out = error_body_suffix(&body);
        for secret in ["payroll", "salary band", "debugging"] {
            assert!(!out.contains(secret), "leaked {secret:?}: {out}");
        }
    }

    #[test]
    fn description_truncates_at_cap_on_char_boundary() {
        // Multibyte char straddling the cap must not split a UTF-8 sequence.
        let body = "é".repeat(DESCRIPTION_CONTENT_BYTES); // 2 bytes each
        let d = file_event_description(EventType::FileCreated, &content(Some(&body)));
        assert!(d.ends_with("…[truncated]"));
        assert!(
            d.len() <= "FileCreated\n\n".len() + DESCRIPTION_CONTENT_BYTES + "\n…[truncated]".len()
        );
        // Must still be valid UTF-8 end to end (String guarantees it; also
        // check we kept at least most of the budget rather than over-trimming).
        assert!(d.len() > DESCRIPTION_CONTENT_BYTES / 2);
    }
}
