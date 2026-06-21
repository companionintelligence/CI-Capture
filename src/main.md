# main.rs

## Purpose
Owns the tray application lifecycle, menu actions, configuration reloads, worker control, and the portal pairing entrypoint.

## Components

### `AppController`
- **Does**: Handles tray actions, settings reloads, worker state, and portal pairing completion events.
- **Interacts with**: `WorkerHandle`, `ConfigManager`, `oidc.rs`, and `settings_ui.rs`.

### `launch_portal_login`
- **Does**: Starts a GraphQL pairing session, opens the browser to the pairing URL, and waits for the authenticated browser to claim the applet.
- **Interacts with**: `pair_with_portal`.

### `pair_with_portal`
- **Does**: Runs the end-to-end pairing workflow and persists the resulting `user_id` / `device_id` plus the applet bearer token.
- **Interacts with**: `oidc.rs` and `ConfigManager`.

### `ensure_device_registration`
- **Does**: Preserves the legacy manual-UUID fallback by direct-registering a device in Postgres when a user ID exists but no device ID is set.
- **Interacts with**: `register_or_create_device`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| Users clicking tray login | Pairing is single-flight and ends by persisting identity plus applet auth to config | Removing browser launch or not saving the returned token |
| `worker.rs` | Identity and `server.applet_auth_token` remain available in config after successful pairing | Renaming/removing paired auth fields |
