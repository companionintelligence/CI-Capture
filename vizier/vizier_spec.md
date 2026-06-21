# Vizier (`vz`) — System Perception Utility
## Specification v0.2

---

## Overview

`vizier` is a small, fast, cross-platform system observation utility written in Rust. Its purpose is to produce structured, machine-readable snapshots of a user's desktop environment — capturing what the user is doing, what is focused, what is visible, and what the system is doing — with minimal overhead. The primary consumer is an AI agent (Ponderer), but the output is useful for security monitoring, behavioral logging, and forensics.

The CLI command is `vz`.

---

## Goals

- Single, small, statically-linked binary per platform
- No runtime dependencies (no Python, no Node, no Electron)
- Produce clean, versioned JSON output suitable for LLM consumption
- Support both one-shot snapshots and streaming observation modes
- Minimize token cost: semantic data only, no raw pixel data
- Diff-friendly output structure to support delta streaming
- Designed for repeated polling to build temporal history
- Support a one-shot `wake` command for cold-start machine identity and context

---

## Non-Goals

- GUI
- Screen capture or pixel-level vision
- Kernel-level instrumentation (no eBPF, no kexts)
- Authentication or access control (caller is responsible)
- Aggregation or storage (output to stdout only; caller decides persistence)

---

## Platforms

| Platform | Window API | Priority |
|---|---|---|
| macOS | Core Graphics (`CGWindowListCopyWindowInfo`) + `libproc` | P0 — primary dev environment |
| Linux / Wayland (Hyprland) | Hyprland IPC socket + `/proc` | P1 — deployment target |
| Linux / X11 | `xcb` + EWMH | P2 — fallback |
| Windows | `windows-rs` (`EnumWindows`, `GetForegroundWindow`) | P3 — future |

Platform is selected at compile time via `cfg(target_os)` and Hyprland vs X11 via a runtime check (presence of `$HYPRLAND_INSTANCE_SIGNATURE`).

---

## CLI Interface

```
vz wake                            # one-shot cold-start orientation snapshot (WakeObservation)
vz snapshot                        # single JSON live-state snapshot to stdout (Observation)
vz watch                           # stream newline-delimited JSON snapshots
vz watch --interval <ms>           # set polling interval (default: 1000ms)
vz watch --diff                    # stream JSON Patch (RFC 6902) deltas only
vz --pretty                        # pretty-print JSON (any mode)
vz --version
vz --help
```

All output goes to stdout. Errors go to stderr. Exit codes: 0 = success, 1 = error.

### Two-Mode Perception Model

`vizier` exposes two distinct observation types that together form a complete perceptual system for an agent:

- **`WakeObservation`** (`vz wake`) — called once on agent cold-start, or after a long absence. Answers: *where am I, whose machine is this, what is the shape of this environment?* Higher token cost but paid once; cacheable with a TTL of hours.
- **`Observation`** (`vz snapshot` / `vz watch`) — called repeatedly to track live state. Answers: *what is happening right now?* Low token cost, high frequency.

An agent loads the `WakeObservation` to build its world model, then uses the stream of `Observation`s to track what is changing. Together they are episodic memory plus working memory.

---

## Output Schema

All output is JSON. There are two root object types: `WakeObservation` (from `vz wake`) and `Observation` (from `vz snapshot` / `vz watch`).

---

### `WakeObservation`

The cold-start orientation snapshot. Answers every question an agent needs to build a world model of the machine it has just arrived on.

```json
{
  "schema_version": 1,
  "ts": 1708732800.412,
  "machine": { ... },
  "user": { ... },
  "datetime": { ... },
  "filesystem": { ... },
  "installed_apps": [ ... ],
  "network_identity": { ... },
  "listening_ports": [ ... ],
  "resources": { ... },
  "recent_activity": { ... },
  "other_sessions": [ ... ]
}
```

#### `machine`

```json
{
  "hostname": "maxbook-pro",
  "os": "macOS",
  "os_version": "14.3.1",
  "kernel": "Darwin 23.3.0",
  "arch": "aarch64",
  "is_vm": false,
  "is_container": false,
  "hypervisor": null,
  "chassis": "Laptop"
}
```

`is_vm` detected via DMI chassis type, hypervisor CPUID flag, or `/proc/cpuinfo` on Linux. `is_container` via cgroup namespace detection or `/.dockerenv` presence.

#### `user`

```json
{
  "username": "max",
  "full_name": "Max Tegmark",
  "home_dir": "/home/max",
  "shell": "/bin/zsh",
  "uid": 1000,
  "groups": ["wheel", "docker", "audio"]
}
```

#### `datetime`

```json
{
  "ts": 1708732800.412,
  "iso": "2024-02-24T10:33:20-05:00",
  "timezone": "America/New_York",
  "utc_offset_seconds": -18000,
  "uptime_seconds": 432910,
  "login_ts": 1708700000.0
}
```

`uptime_seconds` from `/proc/uptime` (Linux) or `sysctl kern.boottime` (macOS). `login_ts` from `utmpx` / `last` records.

#### `filesystem`

```json
{
  "home_tree": [
    { "path": "~/code", "kind": "dir", "children": ["vizier", "ponderer", "acoustic"] },
    { "path": "~/Desktop", "kind": "dir", "children": [] },
    { "path": "~/Downloads", "kind": "dir", "entry_count": 42 }
  ],
  "recent_files": [
    { "path": "/home/max/code/acoustic/model.py", "modified_ago_s": 312 },
    { "path": "/home/max/code/vizier/src/main.rs", "modified_ago_s": 891 }
  ],
  "mounts": [
    { "path": "/", "fs_type": "ext4", "total_gb": 500, "free_gb": 312 },
    { "path": "/home", "fs_type": "btrfs", "total_gb": 1000, "free_gb": 740 }
  ]
}
```

`home_tree` is limited to 2 levels deep. Directories with more than ~20 children show `entry_count` only. `recent_files` shows the 10 most recently modified files under `$HOME`, sorted by mtime descending.

#### `installed_apps`

Meaningful installed applications — not a full binary listing. Detected by scanning well-known locations (`/Applications` on macOS, common `$PATH` bins on Linux) and filtering for a curated set of significant app names (IDEs, browsers, terminals, servers, ML tools, creative apps).

```json
[
  { "name": "Visual Studio Code", "id": "code", "kind": "ide" },
  { "name": "Firefox", "id": "firefox", "kind": "browser" },
  { "name": "Alacritty", "id": "alacritty", "kind": "terminal" },
  { "name": "Docker", "id": "docker", "kind": "infra" },
  { "name": "python3", "id": "python3", "kind": "runtime", "version": "3.12.2" }
]
```

Kinds: `ide`, `browser`, `terminal`, `editor`, `runtime`, `infra`, `ml`, `media`, `communication`, `game`, `other`.

#### `network_identity`

```json
{
  "local_ips": ["192.168.1.42", "10.0.0.5"],
  "public_ip": "98.234.12.55",
  "vpn_active": true,
  "vpn_interface": "utun3",
  "default_gateway": "192.168.1.1",
  "dns_servers": ["8.8.8.8", "1.1.1.1"],
  "hostname_fqdn": "maxbook-pro.local"
}
```

`public_ip` is omitted if `--no-public-ip` flag is passed (avoids outbound request). VPN detection via presence of `utun`/`tun`/`wg` interfaces with routes.

#### `listening_ports`

```json
[
  { "port": 8080, "proto": "tcp", "pid": 12345, "app": "node", "addr": "127.0.0.1" },
  { "port": 5432, "proto": "tcp", "pid": 9871, "app": "postgres", "addr": "127.0.0.1" }
]
```

Listening ports are a high-signal fingerprint of what the machine is actively running. A machine with port 5432 open is running Postgres; port 11434 is running Ollama.

#### `resources`

```json
{
  "cpu_cores": 12,
  "cpu_model": "Apple M2 Pro",
  "ram_total_gb": 32,
  "ram_free_gb": 18,
  "gpus": [
    { "name": "Apple M2 Pro GPU", "vram_gb": null, "driver": "metal" }
  ]
}
```

`vram_gb` null for unified memory architectures. GPU detection via `system_profiler` (macOS), `/proc/driver/nvidia/gpus` or `lspci` (Linux).

#### `recent_activity`

```json
{
  "shell_history": [
    "cargo build --release",
    "cd ~/code/acoustic",
    "python train.py --epochs 50",
    "git status",
    "vz snapshot"
  ],
  "running_since_boot": [
    { "pid": 9871, "app": "postgres", "started_ago_s": 432800 },
    { "pid": 1204, "app": "tailscaled", "started_ago_s": 432750 }
  ]
}
```

`shell_history` is the last 20 commands from the user's history file (`~/.zsh_history`, `~/.bash_history`, etc.), most recent last. `running_since_boot` shows processes that have been alive since near system start — these are effectively services.

#### `other_sessions`

```json
[
  { "username": "max", "tty": "pts/1", "from": "192.168.1.10", "login_ts": 1708730000.0 }
]
```

Other active login sessions from `utmpx`. Empty array is the common case.

---

### `Observation` (live state)

```json
{
  "schema_version": 1,
  "ts": 1708732800.412,
  "monotonic_ms": 48291033,
  "idle_ms": 4200,
  "focus": { ... },
  "windows": [ ... ],
  "cursor": { "x": 712, "y": 340 },
  "displays": [ ... ],
  "terminal_ctx": { ... },
  "net_connections": [ ... ],
  "fs_events": [ ... ]
}
```

| Field | Type | Description |
|---|---|---|
| `schema_version` | u32 | Incremented on breaking schema changes |
| `ts` | f64 | Unix timestamp (seconds, fractional) |
| `monotonic_ms` | u64 | Monotonic clock in ms (for delta calculation) |
| `idle_ms` | u64 | Milliseconds since last user input event |
| `focus` | `WindowInfo?` | Currently focused/active window |
| `windows` | `[WindowInfo]` | All visible windows, z-order descending (front first) |
| `cursor` | `Point` | Current mouse cursor position in screen coordinates |
| `displays` | `[DisplayInfo]` | Connected monitors and their geometry |
| `terminal_ctx` | `TerminalCtx?` | Context if focused window is a terminal emulator |
| `net_connections` | `[ConnInfo]` | Active network connections (non-loopback) |
| `fs_events` | `[FSEvent]` | Filesystem events since last observation (inotify/FSEvents delta) |

### `WindowInfo`

```json
{
  "id": "0x00003e00",
  "title": "vessel_classifier.py — VS Code",
  "app": "Code",
  "pid": 8821,
  "bounds": { "x": 0, "y": 25, "w": 1440, "h": 877 },
  "workspace": 1,
  "is_minimized": false,
  "is_fullscreen": false
}
```

### `DisplayInfo`

```json
{
  "id": 0,
  "bounds": { "x": 0, "y": 0, "w": 2560, "h": 1440 },
  "is_primary": true,
  "scale_factor": 2.0
}
```

### `TerminalCtx`

Populated only when the focused window is identified as a terminal emulator (by app name heuristic).

```json
{
  "cwd": "/home/max/projects/acoustic",
  "shell": "zsh"
}
```

`cwd` is read from `/proc/<pid>/cwd` (Linux) or `proc_pidinfo` (macOS). `shell` from process name of foreground child.

### `ConnInfo`

```json
{
  "proto": "tcp",
  "local_port": 52341,
  "remote_addr": "142.250.80.46",
  "remote_port": 443,
  "pid": 8821,
  "app": "Code",
  "state": "ESTABLISHED"
}
```

Loopback connections (`127.x`, `::1`) are excluded by default. Only ESTABLISHED connections are included unless `--all-connections` flag is passed.

### `FSEvent`

```json
{
  "path": "/home/max/projects/acoustic/model.py",
  "kind": "Modify",
  "ts": 1708732799.1
}
```

Events are collected since the previous observation. On first run, this array is empty. Watched path defaults to `$HOME`; configurable via `--watch-path`. Event kinds: `Create`, `Modify`, `Delete`, `Rename`.

### `Point`

```json
{ "x": 712, "y": 340 }
```

### `Bounds`

```json
{ "x": 0, "y": 25, "w": 1440, "h": 877 }
```

---

## Delta / Diff Mode

In `vz watch --diff`, after the initial full snapshot, subsequent outputs are JSON Patch documents (RFC 6902 `application/json-patch+json`) wrapped in an envelope:

```json
{
  "ts": 1708732801.422,
  "monotonic_ms": 48292044,
  "patch": [
    { "op": "replace", "path": "/focus/title", "value": "main.rs — VS Code" },
    { "op": "replace", "path": "/idle_ms", "value": 0 }
  ]
}
```

This keeps the stream very lean for the agent use case — only changed fields are transmitted.

---

## Crate Dependencies

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
json-patch = "1"                    # RFC 6902 patch generation
notify = "6"                        # Cross-platform FS events (inotify/FSEvents/ReadDirectoryChanges)

[target.'cfg(target_os = "macos")'.dependencies]
core-graphics = "0.23"
core-foundation = "0.9"
libc = "0.2"

[target.'cfg(target_os = "linux")'.dependencies]
xcb = { version = "1", features = ["randr", "xinerama"], optional = true }
# Hyprland IPC: stdlib UnixStream only, no extra crate needed

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.52", features = [
  "Win32_UI_WindowsAndMessaging",
  "Win32_System_Threading",
  "Win32_NetworkManagement_IpHelper",
]}
```

No async runtime. No heavy dependencies. `clap` is the largest dependency; consider `pico-args` if binary size becomes a concern.

---

## Project Structure

```
vizier/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── src/
│   ├── main.rs               # CLI entry, arg parsing, mode dispatch
│   ├── observation.rs        # Observation + WakeObservation structs, serde derives
│   ├── diff.rs               # JSON Patch generation (wraps json-patch crate)
│   ├── observer/
│   │   ├── mod.rs            # Observer + Waker trait definitions
│   │   ├── macos.rs          # Core Graphics + libproc implementation
│   │   ├── linux.rs          # Hyprland IPC + X11 fallback
│   │   └── windows.rs        # Win32 implementation
│   └── util/
│       └── net.rs            # Shared network connection parsing
└── tests/
    └── snapshot_schema.rs    # Validate snapshot and wake output against schema
```

---

## Traits

```rust
pub trait Observer {
    /// Collect a full live-state snapshot.
    fn snapshot(&mut self) -> anyhow::Result<Observation>;
}

pub trait Waker {
    /// Collect a cold-start orientation snapshot.
    fn wake(&self) -> anyhow::Result<WakeObservation>;
}
```

`Observer` takes `&mut self` because it maintains internal state (FS event cursor, previous snapshot for delta tracking). `Waker` takes `&self` — it is stateless and side-effect free.

Both are instantiated once at startup via factory functions:

```rust
pub fn create_observer() -> Box<dyn Observer> { ... }
pub fn create_waker() -> Box<dyn Waker> { ... }
```

---

## Platform Implementation Notes

### macOS

**Live (`Observer`):**
- **Window list**: `CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID)` — returns all on-screen windows with title, PID, bounds, layer. No accessibility permission required for titles of most apps.
- **Focus**: `CGWindowListCopyWindowInfo` with `kCGWindowListOptionOnScreenAboveWindow` trick, or use `NSWorkspace.sharedWorkspace.frontmostApplication` via `objc2` if simpler.
- **Idle time**: `IOHIDGetParameter` or read from `IORegistry` — `HIDIdleTime` key.
- **Network**: `proc_pidinfo` with `PROC_PID_FD_SOCKETINFO`, or parse `netstat -n` as fallback.
- **Terminal CWD**: `proc_pidinfo(pid, PROC_PIDVNODEPATHINFO)` for cwd of child shell process.
- **FS events**: `notify` crate wraps FSEvents API.

**Wake (`Waker`):**
- **Machine identity**: `sysctl hw.model`, `uname`, `sysctl kern.boottime`, DMI via IORegistry.
- **User info**: `getpwuid` / `NSFullUserName`.
- **Datetime + uptime**: `clock_gettime`, `sysctl kern.boottime`, `utmpx` for login time.
- **Filesystem**: `readdir` on `$HOME` to depth 2; `mdfind` or `find` sorted by mtime for recent files; `getmntinfo` for mounts.
- **Installed apps**: Scan `/Applications` for `.app` bundles; scan `$PATH` for known binaries.
- **Network identity**: `getifaddrs` for local IPs; `SCDynamicStore` for gateway/DNS; detect `utun` interfaces for VPN.
- **Public IP**: HTTP GET to `https://api.ipify.org` (skipped with `--no-public-ip`).
- **Listening ports**: `proc_pidinfo` with `PROC_PIDLISTFDS` + `PROC_PID_FD_SOCKETINFO`.
- **Resources**: `sysctl hw.physicalcpu`, `sysctl hw.memsize`; GPU via `system_profiler SPDisplaysDataType`.
- **Shell history**: Read tail of `~/.zsh_history` or `~/.bash_history`.
- **Other sessions**: `getutxent` for active sessions.

### Linux / Hyprland

**Live (`Observer`):**
- **Window list + focus**: Write `j/clients` to `$HYPRLAND_INSTANCE_SIGNATURE` socket, parse JSON response. Returns all windows with address, title, class, workspace, geometry, focusHistory.
- **Active window**: Write `j/activewindow` to socket.
- **Idle time**: `$XDG_RUNTIME_DIR/wayland-0` via `ext-idle-notify-v1` protocol, or read `/proc/interrupts` delta as approximation.
- **Network**: Parse `/proc/net/tcp` and `/proc/net/tcp6`. Resolve inode → PID via `/proc/<pid>/fd/` symlinks.
- **Terminal CWD**: `readlink /proc/<pid>/cwd`.
- **FS events**: `notify` crate wraps inotify.
- **X11 fallback**: If `$HYPRLAND_INSTANCE_SIGNATURE` absent, use `xcb` with EWMH `_NET_CLIENT_LIST_STACKING`, `_NET_ACTIVE_WINDOW`, `_NET_WM_NAME`.

**Wake (`Waker`):**
- **Machine identity**: `/etc/os-release`, `uname -r`, `/sys/class/dmi/id/chassis_type`, check `/.dockerenv` or cgroup v2 namespace for container detection.
- **User info**: `getpwuid`, `/etc/passwd`.
- **Datetime + uptime**: `clock_gettime`, `/proc/uptime`, `utmpx`/`last` for login time.
- **Filesystem**: `readdir` on `$HOME` to depth 2; `find $HOME -type f -printf '%T@ %p\n' | sort -rn | head -10` for recent files; `/proc/mounts` for mounts with `statvfs` for usage.
- **Installed apps**: Scan `$PATH` dirs for known binary names; scan `~/.local/share/applications` and `/usr/share/applications` for `.desktop` files.
- **Network identity**: `getifaddrs`; parse `/etc/resolv.conf` for DNS; detect `tun`/`wg` interfaces for VPN.
- **Public IP**: HTTP GET to `https://api.ipify.org` (skipped with `--no-public-ip`).
- **Listening ports**: Parse `/proc/net/tcp` + `/proc/net/tcp6` for state `0A` (LISTEN); resolve PID via inode.
- **Resources**: `/proc/cpuinfo`, `/proc/meminfo`; GPU via `/proc/driver/nvidia/gpus` or `lspci` output.
- **Shell history**: Read tail of `~/.zsh_history` or `~/.bash_history`.
- **Other sessions**: `getutxent` / parse `/var/run/utmp`.

### Windows (future)

**Live (`Observer`):**
- `GetForegroundWindow` + `GetWindowText` for focus.
- `EnumWindows` for all windows.
- `GetLastInputInfo` for idle time.
- `GetTcpTable2` / `GetExtendedTcpTable` for connections.
- `ReadDirectoryChangesW` (wrapped by `notify` crate) for FS events.

**Wake (`Waker`):**
- `GetComputerNameEx`, `RtlGetVersion` for machine identity.
- `GetUserNameEx` for user info.
- `GetTickCount64` for uptime; `WTSQuerySessionInformation` for login time.
- `SHGetKnownFolderPath` for home dir; enumerate well-known install locations for apps.
- `GetAdaptersInfo` for network identity.
- `GetTcpTable2` for listening ports.
- `GlobalMemoryStatusEx`, `GetSystemInfo` for resources.

---

## Security & Privacy Notes

- `vizier` reads system state but never writes to it.
- Window titles may contain sensitive data (passwords in terminal prompts, document names). The caller is responsible for deciding what to forward to an LLM.
- Network connections expose remote IPs. No DNS resolution is performed by default (avoids latency and leakage); add `--resolve-dns` flag to opt in.
- On macOS, Screen Recording permission is NOT required for window titles via `CGWindowListCopyWindowInfo`. Accessibility permission is NOT required unless reading window content (which `vizier` does not do).
- The binary does not phone home, write logs, or maintain state between invocations except transiently during `watch` mode.

---

## Build & Install

```bash
cargo build --release
# Binary: target/release/vz (symlink or rename from vizier)

# macOS universal binary
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
lipo -create -output vz target/x86_64-apple-darwin/release/vizier \
                          target/aarch64-apple-darwin/release/vizier
```

Target binary size: < 2MB stripped on all platforms.

---

## Integration with Ponderer

On cold start, Ponderer calls `vz wake` once to build its world model. It then calls `vz snapshot` or maintains a `vz watch --diff` subprocess for live state tracking.

The `WakeObservation` is cached with a TTL of several hours and re-fetched when the agent suspects significant environment change (new network, long idle gap, etc.).

Suggested context framing for `WakeObservation`:

```
[WAKE @ 2024-02-24T10:33:20-05:00 | uptime: 5d 0h | user: max on maxbook-pro (macOS 14.3.1, aarch64)]
Home: ~/code (vizier, ponderer, acoustic, ...), ~/Downloads (42 items)
Recent: acoustic/model.py (5m ago), vizier/src/main.rs (15m ago)
Apps: VS Code, Firefox, Alacritty, Docker, python3 3.12.2
Net: 192.168.1.42, VPN active (utun3), public: 98.234.12.55
Ports: 8080 (node), 5432 (postgres)
Resources: 12-core M2 Pro, 32GB RAM (18GB free)
Last commands: cargo build --release, cd ~/code/acoustic, python train.py --epochs 50
```

Suggested context framing for live `Observation`:

```
[OBSERVATION @ 2024-02-24T10:33:21Z | idle: 4.2s]
Focus: "vessel_classifier.py — VS Code" (Code, pid 8821)
Windows: VS Code, Terminal (zsh @ /home/max/acoustic), Firefox
Terminal CWD: /home/max/projects/acoustic
Net: 3 established connections (443×2, 8080×1)
FS: model.py modified 1.3s ago
```

Both summaries are generated by Ponderer from the JSON, not by `vizier` itself. `vizier` outputs raw data only.

---

## Future Considerations

- `--format summary` mode: emit a compact one-line natural language description (useful for lightweight agents)
- `--filter` flag: emit only specific fields to reduce token cost
- Named pipe / Unix socket server mode: avoid re-spawning binary on each call
- Clipboard hash (not content): signal copy/paste activity without privacy violation
- Active audio output: which app is producing sound
- GPU utilization per process: useful for ML workload awareness
