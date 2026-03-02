# db.rs

## Purpose
Owns Postgres writes for CI-Applet. It records both screenshot-derived history and embedded `vizier` event streams through a shared connection pool.

## Components

### `PostgresRecorder`
- **Does**: Holds the shared Postgres pool and write helpers.
- **Interacts with**: `worker.rs` async loops.

### `record_capture`
- **Does**: Persists file metadata plus screen-capture rows for screenshot analysis.
- **Interacts with**: `ci.files` and `ci.screen_captures`.

### `record_vizier_event`
- **Does**: Persists `vizier` snapshots and diff envelopes into `ci.vizier_events`.
- **Interacts with**: `worker.rs`, JSONB payload storage, and the auto-created `ci.vizier_events` table.
- **Rationale**: Keeps raw telemetry intact while exposing query-friendly columns like focus app and cwd.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | Recorder methods are async and best-effort per event | Changing method names/signatures |
| Analytics queries | `ci.vizier_events.payload` contains raw JSON for each event | Renaming the table or narrowing payload |
| Fresh installs | `ci.vizier_events` is created automatically before first write | Removing schema bootstrap |
