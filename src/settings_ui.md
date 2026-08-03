# settings_ui.rs

## Purpose
Provides the standalone egui settings window for Companion Capture configuration. **Connection** is Portal-first (sign in → discover/pick Memory → Check connection). Manual Memory URL, API key, and OIDC client wiring live under **Advanced**. Styled with the Companion Intelligence design system via [`theme`](theme.md): left sidebar nav, scrollable content, footer with Close/Save.

## Components

### `SettingsApp::render_sidebar` / `render_footer`
- **Does**: Draw the left nav (`theme::nav_item` per tab, logo header, bottom connection card) and the bottom footer (config path or current error, plus `theme::ghost_button`/`primary_button`). `connection_status` derives a coarse Configured/Incomplete/Not-configured hint from config presence (no network probe).
- **Interacts with**: `theme`, `ConfigManager::path`.
- **Advanced toggle**: Storage (MinIO) and Database (Postgres) are legacy direct-write connections (`SettingsTab::is_advanced`) — the supported path streams everything through CI-Server, so they're hidden behind a sidebar "Advanced" toggle (`show_advanced`, default off). Toggling off while viewing one falls back to Identity. See [connection_test.md](connection_test.md) and the worker's `server_ingest` vs `build_legacy_recorder` branch.

### Connection tests (`run_test` / `drain_tests` / `render_test_row` / `build_config`)
- **Does**: Each connection tab gets a "Test connection" button that probes the *current* form values off-thread (settings owns a tokio runtime), reporting an inline Connected/Failed badge + detail. `build_config` layers the form onto the saved config and is shared with `save` so a test probes exactly what's shown.
- **Interacts with**: `connection_test`.

### `field` / `page_header` / `stepper_button` (helpers)
- **Does**: Shared themed building blocks for the tab bodies — a labelled `theme::text_field`, a title+subtitle header, and the Capture interval ± buttons. Keep the tab renderers DRY.

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
