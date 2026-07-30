# config.rs

## Purpose
Defines persisted application configuration for Companion Capture. It is the schema boundary for the tray app, worker loops, and database integrations.

## Components

### `AppConfig`
- **Does**: Aggregates all persisted config sections used by the app.
- **Interacts with**: `ConfigManager`, `worker.rs`, and the settings UI.

### `ServerConfig`
- **Does**: Stores the CI-Server URL and the bearer credential used by upload/ingest calls.
- **Interacts with**: `main.rs`, `settings_ui.rs`, `oidc.rs`, and `server_ingest.rs`.
- **Rationale**: Lets the applet send captures to a chosen CI-Server while the bearer credential can come from OIDC browser login or a pasted API key.

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
- `server.portal_url` controls where data is sent. `oidc.issuer` controls where login happens; they are not automatically derived from each other.
- New configs do not embed a CI-Server URL; users provide the server origin/API URL that matches their deployment.
