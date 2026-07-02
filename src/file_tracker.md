# file_tracker.rs

## Purpose
Runs the file-activity capture lane. It watches configured folders, stores normalized filesystem events durably, converts changed files to Markdown, and syncs them to CI-Server.

## Components

### `is_runnable`
- **Does**: Determines whether file tracking can run from current config.
- **Interacts with**: `worker.rs` and `server_ingest::is_configured`.
- **Rationale**: API-key auth lets CI-Server derive the user, so local identity IDs are no longer required for server ingest mode.

### `run`
- **Does**: Owns the watcher, SQLite queue, conversion, and sync loop for file events.
- **Interacts with**: `watcher`, `queue`, `extractors`, and `server_ingest.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | Runnable means enabled, has watched folders, and has a server credential | Requiring local user/device IDs for API-key mode |
| `server_ingest.rs` | File events include stable event IDs and payload metadata | Dropping `event_id` or content payloads |
