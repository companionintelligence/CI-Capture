# db.rs

## Purpose
Owns Postgres writes for CI-Applet. It records both screenshot-derived history and screenshot-aligned embedded `vizier` events through a shared connection pool.

## Components

### `PostgresRecorder`
- **Does**: Holds the shared Postgres pool and write helpers.
- **Interacts with**: `worker.rs` async loops.

### `record_capture`
- **Does**: Persists file metadata plus screen-capture rows for screenshot analysis.
- **Interacts with**: `ci.files` and `ci.screen_captures`.
- **Rationale**: Maps applet metadata onto the server's current `ci.files` storage schema and recreates `ci.screen_captures` on demand because the server no longer migrates that table.

### `record_vizier_event`
- **Does**: Persists `vizier` snapshots and diff envelopes into the generic `ci.user_snapshots` hypertable.
- **Interacts with**: `worker.rs`, JSONB payload storage, and the server-owned `ci.user_snapshots` table.
- **Rationale**: Keeps raw telemetry intact while storing it in an applet-agnostic time-series table whose schema is owned by CI-Server.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | Recorder methods are async and best-effort per event | Changing method names/signatures |
| Analytics queries | `ci.user_snapshots.payload` contains raw JSON for each event | Renaming the table or narrowing payload |
| Fresh installs | `ci.screen_captures` is created automatically before first write and `ci.user_snapshots` comes from CI-Server migrations | Removing schema bootstrap or migration ownership |
