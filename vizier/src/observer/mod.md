# mod.rs

## Purpose
Defines observation interfaces and platform factory entry points. This module isolates CLI code from OS-specific implementation details.

## Components

### `ObserverConfig`, `WakeConfig`
- **Does**: Carries runtime options into collector implementations.
- **Interacts with**: Constructed in `main.rs`, consumed by backends.

### `Observer`, `Waker`
- **Does**: Trait boundaries for live and wake collectors.
- **Interacts with**: Implemented in `observer/common.rs` and wrapped by per-OS modules.

### `create_observer`, `create_waker`
- **Does**: Selects platform implementation via `cfg`.
- **Interacts with**: `linux.rs`, `macos.rs`, `windows.rs`, and fallback to `common.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | Factory returns trait objects for target platform | Signature changes |
| OS backend modules | Config structs contain required runtime flags | Removing/renaming config fields |

## Notes
Current platform modules wrap baseline collectors and are ready for platform-native replacements.
