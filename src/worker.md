# worker.rs

## Purpose
Runs Companion Capture background capture loops. It manages screenshot capture/analysis and records embedded `vizier` observations in lockstep with saved screenshots, preferring authenticated CI-Server GraphQL ingest when the applet has been paired.

## Components

### `WorkerHandle`
- **Does**: Starts the worker thread and exposes pause/resume/config update commands.
- **Interacts with**: `main.rs` tray controller.

### `run_worker`
- **Does**: Drives the screenshot interval loop and triggers `vizier` capture after each successful screenshot capture.
- **Interacts with**: `WorkerState`, `tokio::time::Interval`, and the analysis queue.
- **Rationale**: Uses a small bounded analysis queue so capture throughput cannot grow memory unbounded if upload/analysis lags.

### `WorkerState::capture_vizier`
- **Does**: Captures a `vizier` snapshot, turns later snapshots into diff envelopes, and records them through CI-Server when paired, otherwise through the legacy Postgres writer.
- **Interacts with**: `vizier::observer`, `vizier::diff`, `server_ingest.rs`, and `db.rs`.
- **Rationale**: Embeds the observation pipeline directly instead of shelling out to `vz`, which avoids subprocess/PATH issues.
- **Resilience**: Skips payloads larger than `MAX_VIZIER_PAYLOAD_BYTES`. Failed or disconnected sends are parked as `vizier_*.json` next to screenshots and drained on reconnect, same delete-on-ack rule as the screenshot store.

### `WorkerState::capture_once`
- **Does**: Preserves the existing screenshot path and analysis queue behavior while snapshotting the current config into each queued capture so later config changes apply without a restart.
- **Interacts with**: `capture.rs` and `run_analysis_loop`.
- **Rationale**: Skips capture ticks while the macOS screensaver is active so lock-screen frames are not uploaded/analyzed, permits bearer-auth CI-Server uploads without local identity IDs, and drops captures when analysis is backlogged to keep memory bounded.

### `run_analysis_loop` / `ingest_capture_to_server`
- **Does**: Routes queued screenshots. In server mode (`ingest_capture_to_server`): if a vision endpoint is configured, the applet analyses locally and writes the result onto the file as title/description (`set_file_metadata`), uploading with `inventoryCapture=false` so CI-Server skips its own Ollama analysis; with no endpoint it uploads `inventoryCapture=true` (no model) and lets CI-Server's default model analyse (the fallback). Without a server credential it falls back to the legacy local MinIO/Postgres path (which still runs the local analyzer).
- **Interacts with**: `VisionAnalyzer`, `server_ingest.rs`, `MinioUploader`, and `PostgresRecorder`.
- **Rationale**: The applet never forwards its own (non-Ollama) vision-model name to the server, which previously made CI-Server reject the image-description job.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | Pause/resume halts both screenshot capture and screenshot-aligned `vizier` writes | Splitting worker state into separate handles |
| `db.rs` | Legacy `VizierEventRecord` writes still use the same identity/device attribution as screenshots | Removing shared identity checks from the fallback path |
| Operators | Config updates can change `vizier` watcher settings and pairing-backed ingest without restart | Freezing worker ingest config at startup |
