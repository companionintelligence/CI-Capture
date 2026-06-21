# config.rs

## Purpose
Defines persisted application configuration for CI-Applet. It is the schema boundary for the tray app, worker loops, and database integrations.

## Components

### `AppConfig`
- **Does**: Aggregates all persisted config sections used by the app.
- **Interacts with**: `ConfigManager`, `worker.rs`, and the settings UI.

### `ServerConfig`
- **Does**: Stores the CI-Server portal URL and the applet-scoped bearer token issued during browser pairing.
- **Interacts with**: `main.rs`, `oidc.rs`, and `server_ingest.rs`.
- **Rationale**: Keeps human login/bootstrap separate from infrastructure credentials while still letting the worker reuse the paired applet session.

### `VizierConfig`
- **Does**: Controls whether embedded `vizier` capture is enabled and what it watches.
- **Interacts with**: `WorkerState` in `worker.rs`.
- **Rationale**: Keeps machine-state capture configurable without hard-coding watch scope.

### `ConfigManager`
- **Does**: Loads and saves `config.json` under the app config directory.
- **Interacts with**: `main.rs` and `settings_ui.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | `load_or_default()` returns a valid `AppConfig` even when new fields are added | Removing defaults for persisted fields |
| `worker.rs` | `vizier` settings are present and default-safe | Renaming/removing `VizierConfig` fields |
| Existing installs | Older `config.json` files still deserialize | Adding non-defaulted fields |

## Notes
- `server.applet_auth_token` is intentionally optional so pre-pairing installs and manual UUID fallback still load cleanly.
- The default portal origin now targets the repo's local CI-Server hostname (`https://memory.ci.localhost`) rather than the external dev site.
