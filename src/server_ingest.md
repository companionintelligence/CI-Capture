# server_ingest.rs

## Purpose
Wraps the CI-Server upload and unified event-ingest endpoints used after browser login or API-key configuration. It keeps screenshot, vizier, and file-event delivery behind the same saved server credential.

## Components

### `is_configured`
- **Does**: Reports whether the current app config has a non-empty applet credential.
- **Interacts with**: `worker.rs`.

### `ingest_capture`
- **Does**: Uploads a screenshot to `POST /api/files/upload` with `inventoryCapture=true`.
- **Interacts with**: `AuthCredential` in `server_common.rs` and CI-Server file ingestion.

### `ingest_vizier_event`, `ingest_file_event`
- **Does**: Serializes applet activity into CI-Server's `POST /api/v1/events` capture-ingest envelope.
- **Interacts with**: `db.rs` event types, watcher events, and CI-Server event ingest.

### `post_capture_event`
- **Does**: Authenticates and posts one capture event, treating server-side rejections as errors.
- **Interacts with**: `AuthCredential`, `server.portal_url`, and CI-Server `/api/v1/events`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | Credential presence alone is enough to choose server ingest | Requiring local Postgres/MinIO credentials for server mode |
| CI-Server `/api/v1/events` | Events use `source_system`, `source_id`, `kind`, timestamps, and JSON payload | Removing unified capture ingest or changing auth headers |

## Notes
- `AuthCredential` sends JWT/OIDC tokens as bearer auth and 64-character CI-Server API keys as `x-api-key`.
