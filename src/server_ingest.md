# server_ingest.rs

## Purpose
Wraps the CI-Server GraphQL mutations used after browser pairing so the worker can ingest screenshots and vizier events without embedding raw Postgres or MinIO credentials.

## Components

### `is_configured`
- **Does**: Reports whether the current app config has a non-empty applet bearer token.
- **Interacts with**: `worker.rs`.

### `ingest_capture`
- **Does**: Uploads a screenshot to `POST /api/files/upload` with `inventoryCapture=true`.
- **Interacts with**: `AnalysisResult` from `analysis.rs` and CI-Server file ingestion.

### `ingest_vizier_event`
- **Does**: Serializes a `VizierEventRecord` into the generic `ingestUserSnapshot` mutation.
- **Interacts with**: `db.rs` event types and CI-Server GraphQL.

### `build_gql_client`, `graphql_url`, `graphql`
- **Does**: Build the authenticated GraphQL client and normalize transport errors.
- **Interacts with**: `reqwest`, `server.portal_url`, and `server.applet_auth_token`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | Token presence alone is enough to choose server ingest and mutation names match CI-Server schema | Renaming ingest mutations or changing auth to cookies only |
| CI-Server GraphQL | Inputs stay camelCase and accept RFC3339 timestamps/base64 PNGs | Renaming input fields or changing payload encoding |

## Notes
- This module intentionally keeps GraphQL transport separate from the pairing client in `server_auth.rs` because the auth model is different after pairing.
