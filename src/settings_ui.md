# settings_ui.rs

## Purpose
Provides the standalone egui settings window for CI-Applet configuration, including CI-Server URL, API-key/OIDC auth mode, manual identity overrides, and infrastructure settings.

## Components

### `SettingsApp::save`
- **Does**: Validates and persists the edited config, treating `user_id` and `server.applet_auth_token` as independent optional overrides.
- **Interacts with**: `ConfigManager`.
- **Rationale**: Allows users to point captures at one CI-Server while choosing either OIDC browser login or a pasted CI-Server API key.

### `SettingsApp::validate`
- **Does**: Enforces CI-Server/OIDC URL constraints for the selected auth mode and numeric constraints while only validating `user_id` when a manual UUID is supplied.
- **Interacts with**: `save`.

### `render_identity_tab`
- **Does**: Exposes CI-Server URL plus either API-key paste or OIDC issuer/client/redirect settings, with legacy manual UUID override hidden behind an advanced section.
- **Interacts with**: `SettingsFormState`.

### `normalize_ci_server_url`
- **Does**: Removes browser page routes such as `/feed` from pasted CI-Server URLs while preserving explicit API base paths.
- **Interacts with**: `save`, `validate`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| Users relying on the settings window | Leaving the manual user field blank no longer blocks saving unrelated settings or pasted tokens | Requiring `user_id` again or hiding auth fields |
| `main.rs` | Persisted CI-Server URL and OIDC issuer remain editable here | Merging server URL and OIDC issuer into one field |

## Notes
- `server.portal_url` is where captures are sent; `oidc.issuer` is where the browser login starts.
- The applet does not assume a local or hosted CI-Server URL; users provide the URL that matches their deployment.
- OIDC settings may be left blank when a CI-Server API key is pasted.
- Pasted bearer/API keys are stored in `server.applet_auth_token`; changing that field clears the stored OIDC session metadata.
- Users may paste a page URL like `https://host/feed`; settings stores the server origin instead.
