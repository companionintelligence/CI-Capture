# linux.rs

## Purpose
Linux-specific observer and wake collectors with Hyprland IPC enrichment and Linux system metadata collection. This module keeps Linux integration isolated from CLI and schema code while retaining baseline fallback behavior.

## Components

### `create_observer`
- **Does**: Returns `LinuxObserver` implementation.
- **Interacts with**: `BaselineObserver` in `common.rs`.

### `create_waker`
- **Does**: Returns `LinuxWaker` implementation.
- **Interacts with**: `BaselineWaker` in `common.rs`.

### `LinuxObserver::snapshot`
- **Does**: Starts from baseline snapshot and enriches data via Hyprland IPC when available (`clients`, `activewindow`, `monitors`).
- **Interacts with**: Unix socket IPC, `Observation` schema, and terminal cwd probes in `/proc`.

### `LinuxWaker::wake`
- **Does**: Starts from baseline wake payload and overrides Linux-specific values from `/etc/os-release`, DMI, `ip route`, `/proc/uptime`, `who`, and `lspci`.
- **Interacts with**: `sysinfo`, filesystem and command probes, `WakeObservation` schema.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `observer/mod.rs` | Exposes factory fns with stable signatures | Signature changes |
| `main.rs` | Linux collector probes fail open and return baseline-compatible payloads | Hard failing when Hyprland/system commands are missing |

## Notes
Hyprland IPC is opportunistic. If the Hyprland runtime socket is unavailable, the collector returns baseline snapshot data instead of failing.
