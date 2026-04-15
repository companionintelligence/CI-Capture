# worker.rs

## Purpose
Runs CI-Applet background capture loops. It manages screenshot capture/analysis and records embedded `vizier` observations in lockstep with saved screenshots, preferring authenticated CI-Server GraphQL ingest when the applet has been paired.

## Components

### `WorkerHandle`
- **Does**: Starts the worker thread and exposes pause/resume/config update commands.
- **Interacts with**: `main.rs` tray controller.

### `run_worker`
- **Does**: Drives the screenshot interval loop and triggers `vizier` capture after each successful screenshot capture.
- **Interacts with**: `WorkerState`, `tokio::time::Interval`, and the analysis queue.
- **Rationale**: Uses a small bounded analysis queue so capture throughput cannot grow memory unbounded if upload/analysis lags.

### `WorkerState::capture_vizier`
- **Does**: Captures a `vizier` snapshot, turns later snapshots into diff envelopes, and records them through CI-Server GraphQL when paired, otherwise through the legacy Postgres writer.
- **Interacts with**: `vizier::observer`, `vizier::diff`, `server_ingest.rs`, and `db.rs`.
- **Rationale**: Embeds the observation pipeline directly instead of shelling out to `vz`, which avoids subprocess/PATH issues.

### `WorkerState::capture_once`
- **Does**: Preserves the existing screenshot path and analysis queue behavior while snapshotting the current config into each queued capture so later config changes apply without a restart.
- **Interacts with**: `capture.rs` and `run_analysis_loop`.
- **Rationale**: Skips capture ticks while the macOS screensaver is active so lock-screen frames are not uploaded/analyzed, and drops captures when analysis is backlogged to keep memory bounded.

### `run_analysis_loop`
- **Does**: Analyzes queued screenshots, then routes them to CI-Server GraphQL when the applet has a bearer token or to the legacy MinIO/Postgres path otherwise.
- **Interacts with**: `VisionAnalyzer`, `server_ingest.rs`, `MinioUploader`, and `PostgresRecorder`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | Pause/resume halts both screenshot capture and screenshot-aligned `vizier` writes | Splitting worker state into separate handles |
| `db.rs` | Legacy `VizierEventRecord` writes still use the same identity/device attribution as screenshots | Removing shared identity checks from the fallback path |
| Operators | Config updates can change `vizier` watcher settings and pairing-backed ingest without restart | Freezing worker ingest config at startup |
