# server_auth.rs

## Purpose
Implements the CI-Applet side of the GraphQL pairing handshake with CI-Server.

## Components

### `start_pairing`
- **Does**: Calls `startAppletPairing`, returning the private poll key and browser URL the user must visit.
- **Interacts with**: CI-Server GraphQL and the tray controller in `main.rs`.

### `wait_for_pairing`
- **Does**: Polls `appletPairingStatus` until the user finishes browser login/confirmation or the request expires, then returns the issued applet bearer token.
- **Interacts with**: `main.rs` and the applet config update path.

### `PairingRequestMetadata`, `PairingLaunch`, `PairingCompletion`
- **Does**: Carry device metadata into the GraphQL launch call and paired user/device/auth data back out.
- **Interacts with**: `main.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | `PairingCompletion` contains raw UUIDs plus `applet_auth_token` suitable for persisted config | Removing the token from pairing status or changing UUID fields to global IDs |
| CI-Server pairing GraphQL | Mutation/query field names match the current schema exactly | Renaming `startAppletPairing` / `appletPairingStatus` fields |
