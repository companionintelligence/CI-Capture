# minio.rs

> **Status: legacy / deprecated.** Used only when the applet has not been paired with a CI-Portal. After pairing, `server_ingest.rs` uploads screenshots through the CI-Server REST endpoint instead and the server owns blob storage. This module exists to keep pre-pairing installs working until the legacy code path is removed (see [docs/plan/00-foundation.md](../docs/plan/00-foundation.md)).

## Purpose
Direct S3/MinIO uploader for screenshot PNGs. Bypasses CI-Server entirely and writes to a bucket the user (or an operator) has configured by hand.

## Components

### `MinioUploader`
- **Does**: Holds an `aws_sdk_s3::Client`, the target bucket name, and the public endpoint string used to construct returned URLs.
- **Interacts with**: `worker.rs::run_analysis_loop` (legacy branch only).

### `MinioUploader::new`
- **Does**: Builds an AWS SDK client with explicit credentials, normalizes the endpoint to include a scheme, forces path-style addressing (required for MinIO), and calls `ensure_bucket`.
- **Interacts with**: `MinioConfig` in `config.rs`.
- **Rationale**: Path-style is required because MinIO does not implement virtual-hosted-style buckets out of the box. The forced `path-style` setting is the reason this module exists separately from a generic S3 uploader.

### `MinioUploader::upload_png`
- **Does**: PUTs the bytes under the given key, sets `image/png` content type, returns a public URL composed from the configured endpoint + bucket + key.
- **Interacts with**: `worker.rs` (legacy ingest path).

### `MinioUploader::ensure_bucket` (private)
- **Does**: HEADs the bucket; if absent, attempts to create it, swallowing `BucketAlreadyOwnedByYou` / `BucketAlreadyExists`.
- **Interacts with**: Called once by `new`.
- **Rationale**: The applet's "single-machine MinIO" deployments often start with no buckets at all; auto-creating reduces setup friction.

### `normalized_endpoint` (private)
- **Does**: Returns the endpoint with `http://` or `https://` prefixed if missing, picking the scheme from `MinioConfig::use_ssl`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` (legacy branch) | `upload_png(key, bytes) -> Result<public_url>` is idempotent on key | Returning a non-URL string or requiring pre-existing buckets |
| `config.rs` | `MinioConfig { endpoint, access_key, secret_key, region, bucket, use_ssl }` shape is stable | Renaming fields without a migration |
| Paired installs | This module is **never called** when `server.applet_auth_token` is set | Re-entering the legacy path while paired |

## Notes
- Deprecation plan: every call site logs a `WARN` line; the module is slated for removal after Phase 2 (file-watcher MVP) lands and verifies the paired ingest is the only path in use.
- Force-path-style is non-negotiable for MinIO interop. Do not "improve" it.
- The returned URL is *not* a signed URL. It assumes the bucket / endpoint is reachable from wherever the URL is later resolved. For production CI deployments, the paired path (via CI-Server) is the correct choice.
