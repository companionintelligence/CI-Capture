# server_upload.rs

## Purpose
Implements the user-driven document upload flow that the `--upload-ui` window orchestrates: multipart upload of selected files to CI-Server, followed by polling for the server's parse status. Distinct from `server_ingest.rs` (which is for background capture ingest, not user-picked files).

## Components

### `upload_files`
- **Does**: Builds a multipart form containing every selected file, sends a bearer-authed `POST` to `<portal_origin>/api/files/upload`, and decodes the returned array of file IDs.
- **Interacts with**: `upload_ui.rs::UploadApp::start_upload`.
- **Rationale**: CI-Server already deduplicates uploads by hash. When the response contains fewer IDs than the request had paths, the count delta is the only client-side signal that a duplicate occurred — `already_existed` is inferred from that delta.

### `poll_parse_status`
- **Does**: Polls the `file(id)` GraphQL query every `POLL_INTERVAL` (3 s) until the file reaches a terminal `parseStatus` (`completed`, `failed`, `skipped`), times out at `POLL_TIMEOUT` (5 min), or stays in a null state for `NULL_STATUS_TIMEOUT` (30 s) and is treated as skipped.
- **Interacts with**: `upload_ui.rs::start_upload` (spawns one poll task per uploaded file).
- **Rationale**: The "null status for 30 s" rule exists because CI-Server may return null while the file is still being claimed by the parser worker; if it never transitions out of null, the file is almost certainly not going to be parsed (unsupported type, etc.).

### `sanitize_parse_error` (private)
- **Does**: If the server's parse-error string looks like it contains a file path (`/`, `\`) or is suspiciously long (>200 chars), replaces it with a generic user-facing message. Otherwise passes it through.
- **Interacts with**: `poll_parse_status`.
- **Rationale**: Defense-in-depth against leaking server-internal paths or parser stack traces into the user UI.

### `supported_extensions`
- **Does**: Returns the hard-coded list of file extensions the server will parse (`pdf docx xlsx xls doc csv txt md markdown`).
- **Interacts with**: `upload_ui.rs::pick_files` (used to build the native file-picker filter).

### `mime_for_filename` (private)
- **Does**: Maps a filename's extension to a MIME string for the multipart part. Defaults to `text/plain`.
- **Interacts with**: `upload_files`.

## Types

| Type | Role |
|---|---|
| `UploadedFile { filename, id, already_existed }` | Per-file result returned from `upload_files`. |
| `ParseStatusValue { Completed, Failed, Skipped }` | The terminal status enum. |
| `TerminalParseStatus { value, error, parsed_at }` | The end state of polling, with sanitized error text. |

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `upload_ui.rs` | `upload_files` returns one `UploadedFile` per input path, in order; `poll_parse_status` returns a terminal `TerminalParseStatus` in bounded time | Changing return-order semantics or removing the timeout fallback |
| CI-Server REST | `POST /api/files/upload` accepts a multipart form with field name `files` and bearer-token auth, and returns a JSON array of `{id}` | Renaming the route, changing the field name, or returning a non-array shape |
| CI-Server GraphQL | The `file(id)` query exposes `parseStatus`, `parseError`, `parsedAt` | Removing or renaming any of these fields |

## Notes
- The `already_existed` heuristic is best-effort: if the server returns one ID for two duplicate files, only the second is flagged as already-existing. This matches the UI's "Already uploaded" hover string ("This file already exists on the server").
- Bearer-auth is required; the function errors immediately when `applet_auth_token` is missing or empty, instead of attempting an anonymous upload.
- `supported_extensions` is consumed both by the file picker filter and by anyone who wants to gate upload UX. Keep the list aligned with CI-Server's parser registry.
