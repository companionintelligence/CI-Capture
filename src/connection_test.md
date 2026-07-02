# connection_test.rs

## Purpose
Read-only "Test connection" probes invoked from the settings window. Each builds
a check from the current (possibly unsaved) form config and returns
`Ok(detail)` / `Err(friendly message)` for inline pass/fail feedback. No probe
mutates remote state.

## Components

### `test_ci_server(config)`
- **Does**: `{ __typename }` GraphQL probe (credential attached when present) with a `GET /health` fallback. Treats 401/403 as a rejected-credential failure.
- **Interacts with**: `server_common` (`server_origin`, `graphql_urls`, `AuthCredential`, `friendly_error`).

### `test_vision(config)`
- **Does**: HTTP reachability probe of `analysis.endpoint` (any HTTP reply = reachable; a POST-only path returning 405 still counts).

### `test_postgres(config)`
- **Does**: Opens a real `tokio_postgres` connection (NoTls) and runs `SELECT 1`.
- **Note**: Connects without TLS like the recorder pool, so the "Require SSL" toggle is not exercised.

### `test_minio(config)`
- **Does**: With `legacy-minio`, full S3 client + bucket check via `minio::MinioUploader::new`; otherwise an HTTP reachability probe of the endpoint (the S3 SDK is an optional dep, off by default).

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `settings_ui.rs` | These async fns take `&AppConfig` and return `Result<String, String>` | Changing the signatures or making them mutate state |

## Notes
- All probes share a 10s timeout and the friendly `reqwest` error mapping in
  `server_common::friendly_error`.
- Run off the UI thread (settings window owns a tokio runtime); results return
  over a channel keyed by tab.
