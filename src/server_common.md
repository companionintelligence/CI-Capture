# server_common.rs

## Purpose
Shared CI-Server URL and authentication helpers. This file centralizes endpoint derivation and decides whether a saved credential should be sent as bearer auth or as a CI-Server API key.

## Components

### `server_origin`, `graphql_urls`, `upload_urls`, `events_urls`, `devices_urls`, `files_urls`
- **Does**: Normalize configured CI-Server URLs to an origin and derive API endpoint candidates. `files_urls(portal, id)` builds `/api/files/{id}` (+ bare fallback) for file metadata updates.
- **Interacts with**: `server_check.rs`, `server_ingest.rs`, and `server_upload.rs`.
- **Rationale**: Reverse-proxied deployments often expose endpoints below `/api`, while direct API deployments often expose the same routes without that prefix.

### `friendly_error`
- **Does**: Maps a `reqwest::Error` to a short user-facing string (TLS/cert, connection refused, timeout, …). Shared by the CLI `--check-server` report and the settings-window connection tests.
- **Interacts with**: `server_check.rs`, `connection_test.rs`.

### `AuthCredential`
- **Does**: Represents either a bearer token or a CI-Server API key and applies the correct request header.
- **Interacts with**: `server_ingest.rs`, `server_upload.rs`, and `server_check.rs`.
- **Rationale**: CI-Server accepts JWT/OIDC credentials via `Authorization: Bearer` but API keys via `x-api-key`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `server_ingest.rs` | `AuthCredential::from_config` errors when no credential is configured | Returning an unauthenticated request |
| CI-Server API-key auth | 64-character lower-hex keys are sent in `x-api-key` | Sending API keys as bearer tokens |
| CI-Server JWT auth | Non-API-key tokens are sent as bearer tokens | Treating JWTs as API keys |
