# settings_ui.rs

## Purpose
Provides the standalone egui settings window for CI-Applet configuration, including portal URL, manual identity overrides, and infrastructure settings.

## Components

### `SettingsApp::save`
- **Does**: Validates and persists the edited config, treating `user_id` as an optional manual override now that portal pairing is the default path.
- **Interacts with**: `ConfigManager`.
- **Rationale**: Clears the applet bearer token when the manual identity changes so the worker never uploads under a mismatched server session.

### `SettingsApp::validate`
- **Does**: Enforces portal URL and numeric constraints while only validating `user_id` when a manual UUID is supplied.
- **Interacts with**: `save`.

### `render_identity_tab`
- **Does**: Explains the tray-based portal pairing flow and exposes the legacy manual UUID override.
- **Interacts with**: `SettingsFormState`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| Users relying on the settings window | Leaving the manual user field blank no longer blocks saving unrelated settings | Requiring `user_id` again or hiding portal URL |
| `main.rs` | Persisted portal URL remains editable here and manual identity edits invalidate stale applet auth | Keeping a mismatched token after manual user changes |

## Notes
- The portal URL hint now points at `https://memory.ci.localhost`, which matches the CI-Server repo's local hostname configuration.
