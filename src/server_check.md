# server_check.rs

## Purpose
Provides the `--check-server` connectivity diagnostic. It reports configured endpoints, local auth state, and live health/GraphQL reachability.

## Components

### `run`, `report`
- **Does**: Loads config, optionally applies a `--portal` override, and prints the diagnostic report.
- **Interacts with**: `ConfigManager`, `server_common.rs`, and CI-Server health/GraphQL endpoints.

### `short_err`
- **Does**: Converts common HTTP client errors into concise terminal text.
- **Interacts with**: `report`.
- **Rationale**: TLS/certificate failures can be reported by `reqwest` as connect failures, so the full error chain is inspected before falling back to host/port wording.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| CLI users | `--check-server --portal <url>` probes without mutating config | Saving the override |
| CI-Server auth | Probe uses the same `AuthCredential` header choice as uploads/ingest | Always sending bearer auth |
