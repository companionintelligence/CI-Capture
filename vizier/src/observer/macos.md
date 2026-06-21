# macos.rs

## Purpose
macOS-specific observer and wake collectors that enrich baseline data with macOS-native sources. This file now handles display/window extraction, cursor and idle probes, and wake-time machine/network/session enrichment.

## Components

### `create_observer`
- **Does**: Returns `MacObserver` with baseline fallback behavior.
- **Interacts with**: `BaselineObserver` in `common.rs`.

### `create_waker`
- **Does**: Returns `MacWaker` with baseline fallback behavior.
- **Interacts with**: `BaselineWaker` in `common.rs`.

### `MacObserver::snapshot`
- **Does**: Starts from baseline snapshot and enriches displays/windows/cursor/idle values from CoreGraphics and IORegistry probes.
- **Interacts with**: `core_graphics`, `Observation` schema, network helpers in `util/net.rs`.

### `MacWaker::wake`
- **Does**: Starts from baseline wake payload and overrides macOS-specific fields (OS identity, gateway, groups, sessions, GPU metadata, uptime fixes).
- **Interacts with**: `system_profiler`, `netstat`, `who`, `sysinfo`, and `WakeObservation` schema.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `observer/mod.rs` | Exposes factory fns with stable signatures | Signature changes |
| `main.rs` | Snapshot/wake calls remain resilient when macOS APIs are unavailable | Converting best-effort probes into hard failures |

## Notes
The implementation is best-effort by design: each probe fails independently and falls back to baseline values so `vz snapshot` and `vz wake` remain reliable in restricted execution contexts.
