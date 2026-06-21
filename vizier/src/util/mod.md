# mod.rs

## Purpose
Utility module root for shared helpers that do not belong to observer orchestration.

## Components

### `net`
- **Does**: Houses network and socket-oriented helper functions.
- **Interacts with**: `observer/common.rs` for observation fields.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `observer/common.rs` | `util::net` module is present and importable | Removing module export |
