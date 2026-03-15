# CI-Applet

CI-Applet is a desktop capture agent for Companion Intelligence. The root Rust crate
captures screenshots, stores them locally, uploads new images to MinIO-compatible
object storage, runs vision analysis, and writes screenshot plus `vizier` activity
records to PostgreSQL.

This repository now also includes a **Tauri v2 desktop launcher** in
`desktop-launcher/` that provides a native launcher window and tray icon for
Windows, macOS, and Linux.

## Repository layout

- `src/main.rs` – legacy monitor entrypoint and tray app
- `src/worker.rs` – screenshot loop, upload queue, and analysis pipeline
- `src/config.rs` – persisted JSON config model and config paths
- `src/db.rs` – PostgreSQL writes for screenshots and vizier events
- `src/analysis.rs` – vision model integration
- `src/settings_ui.rs` – egui settings window
- `desktop-launcher/` – Tauri launcher shell, tray icon, and static frontend

The module contract notes in `src/config.md`, `src/worker.md`, `src/analysis.md`, and
`src/db.md` describe the boundaries for the core app.

## Architecture

### Root monitor crate

The root crate remains the background monitor:

1. Loads persisted configuration from the user config directory.
2. Captures primary-display screenshots on an interval.
3. Skips uploads when the image diff stays below the configured threshold.
4. Uploads accepted PNGs to MinIO / S3-compatible storage.
5. Sends images to the configured vision endpoint for structured analysis.
6. Records screenshots and `vizier` observations into PostgreSQL.

### Tauri launcher

The Tauri launcher is intentionally small:

- Presents a native desktop launcher window backed by static HTML/CSS.
- Creates a Tauri tray icon with **Show Launcher**, **Launch Monitor**,
  **Open Settings**, and **Quit Launcher** actions.
- Launches the existing root `CI-Applet` binary instead of duplicating monitor logic.
- Uses the existing `--settings-ui` flow for configuration editing.

This keeps the capture pipeline stable while giving the project a packaging-friendly
desktop shell that can be used across Windows, macOS, and Linux.

## Build and run

### Root monitor

```bash
cargo run
```

Open the egui settings UI directly:

```bash
cargo run -- --settings-ui
```

### Tauri launcher

Run the launcher in development mode:

```bash
cd desktop-launcher
cargo run
```

The launcher looks for the root monitor binary in:

1. `CI_APPLET_MONITOR_BIN`
2. `target/debug/CI-Applet`
3. `target/release/CI-Applet`
4. A copy of the monitor binary placed next to the launcher executable

If your build output lives elsewhere, set the environment variable explicitly:

```bash
export CI_APPLET_MONITOR_BIN=/absolute/path/to/CI-Applet
```

## Configuration

The app stores `config.json` in the operating system's standard config directory for
`com.compint/CI-Applet`.

Important sections:

- `capture` – screenshot interval, diff threshold, local storage path
- `minio` – endpoint, bucket, credentials, SSL toggle
- `postgres` – database host, port, credentials, database name
- `analysis` – vision endpoint URL, API key, model name
- `identity` – user/device IDs
- `server.portal_url` – Companion Intelligence portal login base URL
- `vizier` – lightweight observation capture settings

## Cross-platform notes

- The legacy monitor tray app remains in the root crate.
- The Tauri launcher adds a second, packaging-oriented tray shell for desktop builds.
- Portal login launching now uses the host operating system's default URL opener on
  Windows, macOS, and Linux.

## Validation

The most relevant validation commands for this repository are:

```bash
cargo test

cd desktop-launcher
cargo test
```

On Linux, the Tauri launcher may additionally require the system WebKit / GTK
development packages expected by Tauri v2.
