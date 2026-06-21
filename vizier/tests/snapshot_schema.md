# snapshot_schema.rs

## Purpose
Integration tests for baseline schema and diff behavior. Guards the initial command/data contract while deeper platform-specific collectors are still in progress.

## Components

### `snapshot_shape_has_required_fields`
- **Does**: Verifies baseline snapshot includes required root fields.
- **Interacts with**: `BaselineObserver` and `Observation` schema.

### `wake_respects_no_public_ip_flag`
- **Does**: Verifies wake collection omits public IP when requested.
- **Interacts with**: `BaselineWaker` and `WakeConfig`.

### `diff_envelope_contains_patch_operations`
- **Does**: Verifies diff mode emits operations between successive snapshots.
- **Interacts with**: `create_diff_envelope` in `diff.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| CI/test workflows | Baseline collector APIs remain callable from library crate | Making collectors private, changing core signatures |
| Developers | Failures identify schema contract regressions early | Removing tests without equivalent coverage |
