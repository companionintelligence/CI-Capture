# upload_ui.rs

## Purpose
Standalone egui window (launched via `CI-Applet --upload-ui`) for picking documents from disk, uploading them to CI-Server through `server_upload.rs`, and surfacing per-file progress through a small state machine.

## Components

### `run_upload_ui`
- **Does**: Loads `AppConfig`, builds an `eframe::NativeOptions` window, and runs the egui app loop.
- **Interacts with**: `ConfigManager`, `eframe`, `UploadApp`.

### `UploadApp`
- **Does**: Holds the `tokio` runtime that drives uploads, the list of `FileEntry` rows, a `UnboundedSender/Receiver` pair for status updates from background tasks, and a `uploading` flag.
- **Interacts with**: `server_upload::{upload_files, poll_parse_status}`, the user via egui.

### `UploadApp::pick_files`
- **Does**: Opens a native file dialog filtered by `supported_extensions`, appends new entries (skipping duplicates already in the list) with `Phase::Queued`.
- **Interacts with**: `tinyfiledialogs::open_file_dialog_multi`.

### `UploadApp::start_upload`
- **Does**: Marks every `Queued` row as `Uploading`, spawns one `upload_files` call for the whole batch, and on success spawns one `poll_parse_status` task per returned file ID. Sends results back through the `StatusUpdate` channel.
- **Interacts with**: `server_upload`, `runtime.spawn`, the UI repaint loop.
- **Rationale**: One batch upload + N parallel polls keeps server load low while letting per-file UI states update independently.

### `UploadApp::drain_updates`
- **Does**: Drains `StatusUpdate` messages from the channel and transitions each affected `FileEntry`'s `Phase`. Unlocks the upload button when nothing remains in-flight.
- **Interacts with**: `update` (called on every egui frame).

### `render_phase` (private)
- **Does**: Maps a `Phase` to a colored label, with hover text on failures.

## State machine

`Phase` is a per-file discriminated union:

```text
Queued
  └─ start_upload ─▶ Uploading
                        ├─ upload error ─▶ UploadFailed(error_string)
                        └─ upload ok    ─▶ AlreadyUploaded { file_id }  (was a dedupe)
                                            └─ poll done ─▶ Done(state, was_duplicate=true)
                                       ─▶ Polling { file_id }            (fresh upload)
                                            └─ poll done ─▶ Done(state, was_duplicate=false)
```

`DoneState` carries the terminal `ParseStatusValue`, optional sanitized error, optional `parsed_at` timestamp, and the dedupe flag.

## Channel protocol

`StatusUpdate`:

| Variant | Sent when |
|---|---|
| `UploadFailed { filename, error }` | Multipart POST failed for the whole batch (one per filename). |
| `Uploaded { filename, file }` | Upload succeeded; carries the `UploadedFile` for transition to `Polling` / `AlreadyUploaded`. |
| `PollDone { file_id, result, was_duplicate }` | Per-file poll reached a terminal status. |

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| Users running `--upload-ui` | Window opens with a "Sign in first" guard when no bearer token is configured, and the Upload button is disabled until at least one Queued file exists | Allowing uploads without a bearer token or hiding the auth state |
| `server_upload.rs` | Returns `Vec<UploadedFile>` ordered to match input paths; `poll_parse_status` always terminates | Changing ordering semantics or making `poll_parse_status` cancellable mid-flight |
| `main.rs` | This UI runs as a separate sub-process spawned with `--upload-ui` | Removing the CLI flag or requiring shared state with the tray process |

## Notes
- The repaint loop is driven by `ctx.request_repaint()` after each spawned task sends an update; egui itself is idle otherwise, keeping CPU low.
- File dedupe in `pick_files` is by `PathBuf` equality — picking the same file twice does not enqueue it twice.
- `already_existed` is shown as "Already uploaded" (gray) rather than green, so the user can tell their click did less than they expected.
- The Tauri migration in Phase 1 ([docs/plan/01-tauri-shell.md](../docs/plan/01-tauri-shell.md)) replaces this module with a Tauri webview; the `Phase` state machine should be ported as-is to a TypeScript discriminated union.
