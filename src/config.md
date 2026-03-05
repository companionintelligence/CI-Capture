# config.rs

## Purpose
Defines persisted application configuration for CI-Applet. It is the schema boundary for the tray app, worker loops, and database integrations.

## Components

### `AppConfig`
- **Does**: Aggregates all persisted config sections used by the app.
- **Interacts with**: `ConfigManager`, `worker.rs`, and the settings UI.

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
