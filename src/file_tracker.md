# file_tracker.rs

## Purpose
Runs the file-activity capture lane. It watches configured folders, stores normalized filesystem events durably, converts changed files to Markdown, and syncs them to CI-Server.

## Components

### `is_runnable`
- **Does**: Determines whether the watcher should run and queue events. Enabled plus at least one watched folder. A missing Memory credential does not idle the lane.
- **Interacts with**: `worker.rs`. Sync still checks `server_ingest::is_configured` before POSTing.
- **Rationale**: Outages must still record edits; the SQLite queue is the backfill.

### `run`
- **Does**: Owns the watcher, SQLite queue, conversion, and sync loop for file events.
- **Interacts with**: `watcher`, `queue`, `extractors`, and `server_ingest.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | Runnable means enabled and has watched folders | Requiring a live Memory credential to watch |
| `server_ingest.rs` | File events include stable event IDs and payload metadata | Dropping `event_id` or content payloads |
