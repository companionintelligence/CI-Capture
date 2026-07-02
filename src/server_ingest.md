# server_ingest.rs

## Purpose
Wraps the CI-Server upload and unified event-ingest endpoints used after browser login or API-key configuration. It keeps screenshot, vizier, and file-event delivery behind the same saved server credential.

## Components

### `is_configured`
- **Does**: Reports whether the current app config has a non-empty applet credential.
- **Interacts with**: `worker.rs`.

### `ingest_capture`
- **Does**: Uploads a screenshot to CI-Server file upload. Takes `inventory: bool` (sent as `inventoryCapture`) and `model: Option<&str>`. The applet sends `inventory=false`/no model when it analysed locally (its own vision endpoint) so the server skips its Ollama pass; `inventory=true`/no model asks the server to analyse with its default model. Returns the server file id.
- **Interacts with**: `AuthCredential` in `server_common.rs` and CI-Server file ingestion.

### `set_file_metadata`
- **Does**: `PATCH /files/{id}` to write a `title`/`description` the applet derived from its local analysis (parity with the server's Ollama image description). No-op when both are `None`.
- **Interacts with**: `files_urls` in `server_common.rs`; called by `worker.rs` only when a vision endpoint is configured.

### `ingest_vizier_event`, `ingest_file_event`
- **Does**: Serializes applet activity into CI-Server's capture-ingest envelope.
- **Interacts with**: `db.rs` event types, watcher events, and CI-Server event ingest.

### `post_capture_event`
- **Does**: Authenticates and posts one capture event, treating server-side rejections as errors.
- **Interacts with**: `AuthCredential`, `server.portal_url`, and CI-Server `/v1/events` (canonical; `/api/v1/events` fallback).

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | Credential presence alone is enough to choose server ingest | Requiring local Postgres/MinIO credentials for server mode |
| CI-Server `/v1/events` | Events use `source_system`, `source_id`, `kind`, timestamps, and JSON payload | Removing unified capture ingest or changing auth headers |

## Notes
- `AuthCredential` sends JWT/OIDC tokens as bearer auth and 64-character CI-Server API keys as `x-api-key`.
- Endpoint candidates from `server_common` allow either `/api/...` or bare API routes. The events lane probes the canonical bare `/v1/events` first (matching CI-Server's route and the browser extension) and keeps `/api/v1/events` only as a compatibility fallback; file upload stays at `/api/files/upload`.
