# server_common.rs

## Purpose
Shared URL helpers used by both the pairing client (`server_auth.rs`) and the post-pairing ingest client (`server_ingest.rs`). Centralizes the rules for deriving a CI-Server origin and GraphQL endpoint from the user-configured portal URL string.

## Components

### `server_origin`
- **Does**: Parses the portal URL, validates it has a host, and returns the scheme + host + port as a `reqwest::Url`. Strips paths and query strings.
- **Interacts with**: `server_auth.rs`, `server_ingest.rs`, `server_upload.rs`.
- **Rationale**: Users sometimes paste the portal URL with a trailing path (`/pair`, `/dashboard`, …); endpoints must hang off the origin, not whatever path the URL happened to carry.

### `graphql_url`
- **Does**: Returns `server_origin(portal_url).join("/api/graphql")`.
- **Interacts with**: `server_auth.rs`, `server_ingest.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| Pairing + ingest callers | Both helpers return a fully-qualified `reqwest::Url` and error on malformed input | Returning `String` instead of `Url`, or accepting malformed input silently |
| CI-Server | The GraphQL endpoint is always served from `/api/graphql` on the portal origin | Moving GraphQL off `/api/graphql` |

## Notes
- This module is intentionally tiny. If a third URL-shaped concept appears, add it here rather than letting each caller duplicate the parse + validate + join logic.
- Both helpers preserve the port when present, which matters for local-dev hostnames like `https://memory.ci.localhost:3000`.
