# worker.rs

## Purpose
Runs CI-Applet background capture loops. It now manages both screenshot capture/analysis and embedded `vizier` observation streaming within the same worker runtime.

## Components

### `WorkerHandle`
- **Does**: Starts the worker thread and exposes pause/resume/config update commands.
- **Interacts with**: `main.rs` tray controller.

### `run_worker`
- **Does**: Drives the screenshot interval loop and the `vizier` interval loop from one command channel.
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
| `main.rs` | Pause/resume affects both screenshot and `vizier` capture loops | Splitting worker state into separate handles |
| `db.rs` | `VizierEventRecord` writes use the same identity/device attribution as screenshots | Removing shared identity checks |
| Operators | Config updates can change `vizier` cadence and watcher settings without restart | Ignoring `vizier` config updates |
