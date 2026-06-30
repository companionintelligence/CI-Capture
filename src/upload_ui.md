# upload_ui.rs

## Purpose
Standalone egui window for uploading documents to CI-Server: pick files, upload
them, and watch each file move through parse status. Styled with the Companion
Intelligence design system via [`theme`](theme.md).

## Components

### `UploadApp` / `update`
- **Does**: Renders the window — header (logo + title), auth status line with Add/Upload buttons, a dashed dropzone, and a list of file rows. Drains background status updates each frame.
- **Interacts with**: `server_upload` (`upload_files`, `poll_parse_status`, `supported_extensions`), `theme`.

### `Phase` / `drain_updates` / `start_upload`
- **Does**: Per-file state machine (Queued → Uploading → Polling/AlreadyUploaded → Done | UploadFailed) driven by async upload+poll tasks over an unbounded channel. **Unchanged by the redesign** — presentation only was restyled.

### `dropzone` / `file_row` / `file_badge` (theme) / `render_phase`
- **Does**: `dropzone` paints the dashed drop target; `file_row` shows a file-type chip + filename + trailing status pill; `render_phase` maps each `Phase` to a coloured `theme::status_badge` (Ready=green, Uploading/Processing=amber, Queued/Uploaded=muted, failures=red), preserving the original hover tooltips.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` (`--upload-ui`) | `run_upload_ui()` opens the window | Changing the entrypoint signature |
| `theme.rs` | `install_fonts`/`apply_visuals`/`load_logo` + widget helpers exist | See theme.md contracts |

## Notes
- Upload remains disabled until authenticated (tray "Login to …"); the button
  carries a disabled hover hint.
- The visual redesign changed only rendering; the upload/poll logic and channel
  protocol are untouched.
