# settings_ui.rs

## Purpose
Provides the standalone egui settings window for CI-Applet configuration, including CI-Server URL, OIDC login settings, bearer/API-key override, manual identity overrides, and infrastructure settings.

## Components

### `SettingsApp::save`
- **Does**: Validates and persists the edited config, treating `user_id` and `server.applet_auth_token` as independent optional overrides.
- **Interacts with**: `ConfigManager`.
- **Rationale**: Allows users to point captures at one CI-Server while choosing either OIDC browser login or a pasted CI-Server bearer/API key.

### `SettingsApp::validate`
- **Does**: Enforces CI-Server/OIDC URL constraints and numeric constraints while only validating `user_id` when a manual UUID is supplied.
- **Interacts with**: `save`.

### `render_identity_tab`
- **Does**: Exposes CI-Server URL, OIDC issuer/client/redirect settings, bearer/API-key paste, and legacy manual UUID override.
- **Interacts with**: `SettingsFormState`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| Users relying on the settings window | Leaving the manual user field blank no longer blocks saving unrelated settings or pasted tokens | Requiring `user_id` again or hiding auth fields |
| `main.rs` | Persisted CI-Server URL and OIDC issuer remain editable here | Merging server URL and OIDC issuer into one field |

## Notes
- `server.portal_url` is where captures are sent; `oidc.issuer` is where the browser login starts.
- Pasted bearer/API keys are stored in `server.applet_auth_token`; changing that field clears the stored OIDC session metadata.
