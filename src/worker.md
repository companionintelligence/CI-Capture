# worker.rs

## Purpose
Runs CI-Applet background capture loops. It manages screenshot capture/analysis and records embedded `vizier` observations in lockstep with saved screenshots.

## Components

### `WorkerHandle`
- **Does**: Starts the worker thread and exposes pause/resume/config update commands.
- **Interacts with**: `main.rs` tray controller.

### `run_worker`
- **Does**: Drives the screenshot interval loop and triggers `vizier` capture after each successful screenshot capture.
- **Interacts with**: `WorkerState`, `tokio::time::Interval`, and `PostgresRecorder`.

### `WorkerState::capture_vizier`
- **Does**: Captures a `vizier` snapshot, turns later snapshots into diff envelopes, and records them to Postgres.
- **Interacts with**: `vizier::observer`, `vizier::diff`, and `db.rs`.
- **Rationale**: Embeds the observation pipeline directly instead of shelling out to `vz`, which avoids subprocess/PATH issues.

### `WorkerState::capture_once`
- **Does**: Preserves the existing screenshot path and analysis queue behavior.
- **Interacts with**: `capture.rs`, MinIO, and `db.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | Pause/resume halts both screenshot capture and screenshot-aligned `vizier` writes | Splitting worker state into separate handles |
| `db.rs` | `VizierEventRecord` writes use the same identity/device attribution as screenshots | Removing shared identity checks |
| Operators | Config updates can change `vizier` watcher settings without restart | Ignoring `vizier` config updates |
