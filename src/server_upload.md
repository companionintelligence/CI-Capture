# server_upload.rs

## Purpose
Implements the manual document-upload window's CI-Server transport. It uploads selected files, then polls parse status for the returned file IDs.

## Components

### `upload_files`
- **Does**: Sends multipart files to the CI-Server file upload endpoint.
- **Interacts with**: `AuthCredential` in `server_common.rs` and `upload_ui.rs`.

### `poll_parse_status`
- **Does**: Polls GraphQL `file(id)` until parsing completes, fails, skips, or times out.
- **Interacts with**: `graphql_url` in `server_common.rs` and `upload_ui.rs`.
- **Rationale**: The endpoint helper supports both `/api/...` and bare API route shapes.

### `supported_extensions`
- **Does**: Lists file extensions the upload UI should allow.
- **Interacts with**: `upload_ui.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `upload_ui.rs` | Upload returns a file ID per accepted unique file | Returning raw server entities |
| CI-Server auth | API keys use `x-api-key`; JWTs use bearer auth | Hard-coding bearer auth |
| Users | Parse errors are sanitized before display | Surfacing server paths or internals |
