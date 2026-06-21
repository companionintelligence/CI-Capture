# windows.rs

## Purpose
Windows platform shim for collector factories. Provides compile-time structure for future Win32 native collector work.

## Components

### `create_observer`
- **Does**: Returns Windows observer implementation.
- **Interacts with**: `BaselineObserver` in `common.rs`.

### `create_waker`
- **Does**: Returns Windows wake implementation.
- **Interacts with**: `BaselineWaker` in `common.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `observer/mod.rs` | Exposes factory fns with stable signatures | Signature changes |

## Notes
Current module is a placeholder boundary only.
