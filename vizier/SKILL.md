---
name: vizier
description: Structured local system perception via the `vz` CLI or the embedded `vizier` Rust library. Use when an agent needs machine orientation, a one-shot local snapshot, wake context, or a stream of filesystem/window/network deltas.
---

# Vizier

`vizier` exposes a CLI binary named `vz` and a Rust library crate named `vizier`.

Use it when you need:
- a one-shot local machine snapshot
- cold-start environment orientation
- a live stream of local deltas
- structured machine-state telemetry for another Rust app

## Fast Rules

1. Use `vz` for a quick one-shot compact snapshot.
2. Use `vz --verbose` when you need the full raw snapshot.
3. Use `vz wake` for cold-start orientation; add `--verbose` for the full wake payload.
4. Use `vz watch --diff` for continuous deltas.
5. If you are writing Rust in another local app, prefer the `vizier` crate over shelling out to `vz`.

## CLI Modes

### Bare `vz`

Default:

```bash
vz
```

This emits a one-shot compact snapshot and exits.

Compact behavior:
- duplicate active network connections from the same app/process are grouped
- grouped rows expose:
  - `connection_count`
  - `remote_host_count`
  - `remote_addr: "(multiple)"`
  - `local_port: 0`
  - `remote_port: 0`

Use this when you want a readable summary for an agent prompt.

### Full snapshot

```bash
vz --verbose
vz snapshot
vz --all-connections snapshot
```

Notes:
- `vz --verbose` restores the full raw snapshot for the default path
- `vz snapshot` is explicit full snapshot mode
- `--all-connections` includes loopback/local connections that are otherwise filtered out

Use this when you need precise socket-level detail.

### Wake

```bash
vz wake
vz --verbose wake
vz --no-public-ip wake
```

Notes:
- default `wake` is compact
- `--verbose` returns the full wake payload
- `--no-public-ip` skips the external IP lookup

Use this when the agent needs startup orientation rather than live foreground state.

### Watch

```bash
vz watch
vz watch --diff
vz watch --interval 250
vz --watch-path /tmp watch --diff
```

Notes:
- `watch` streams snapshots continuously
- `watch --diff` emits:
  1. one full `Observation`
  2. then RFC 6902 patch envelopes
- `--watch-path` changes the filesystem watch root (default is home dir)

Use this for continuous telemetry or ingestion pipelines.

## Output Expectations

The primary payloads are:
- `WakeObservation`
- `Observation`
- `DiffEnvelope`

Important live fields in `Observation`:
- `focus`
- `windows`
- `cursor`
- `displays`
- `terminal_ctx`
- `net_connections`
- `fs_events`

Important note:
- `fs_events` are best-effort watcher events (`Create`, `Modify`, `Delete`, `Rename`)
- “open” activity is only inferred indirectly via file access timestamps in wake/file metadata, not kernel audit events

## Install

Preferred end-user install:

```bash
curl -fsSL https://raw.githubusercontent.com/MLTQ/vizier/master/scripts/install.sh | sh
```

Rolling build:

```bash
curl -fsSL https://raw.githubusercontent.com/MLTQ/vizier/master/scripts/install.sh | VZ_VERSION=rolling sh
```

If the command is not found after install, ensure `~/.local/bin` is on `PATH`:

```bash
grep -q 'HOME/.local/bin' ~/.zshrc || echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
exec zsh
```

To replace an older locally installed binary with a new local build:

```bash
cargo build --release
cp target/release/vz ~/.local/bin/vz
chmod +x ~/.local/bin/vz
rehash
```

To remove an installed binary:

```bash
rm -f ~/.local/bin/vz
rehash
```

## Rust Embedding

Prefer embedding for Rust integrations.

Example dependency:

```toml
vizier = { path = "../../vizier" }
```

Core API:

```rust
use vizier::diff::create_diff_envelope;
use vizier::observer::{create_observer, ObserverConfig};

let mut observer = create_observer(ObserverConfig {
    watch_path: None,
    all_connections: false,
});

let first = observer.snapshot()?;
let second = observer.snapshot()?;
let diff = create_diff_envelope(&first, &second)?;
```

Use embedding when:
- the host app is Rust
- you want no subprocess management
- you want direct access to `Observation` structs

## CI-Applet Integration

`CI-Applet` already embeds `vizier` directly rather than spawning `vz`.

Current pattern:
- create a `vizier` observer in the worker loop
- first tick stores a full snapshot
- subsequent ticks store diff envelopes
- rows are written to `ci.vizier_events`

If you are extending that integration:
- preserve raw JSON payloads
- use extracted columns only for indexing/query convenience
- keep compacting as a presentation concern, not a storage concern

## When Not To Use It

Do not use `vz` as the primary source when:
- you need authoritative browser history
- you need process-level attribution of who opened a file
- you need audit-grade security telemetry

It is a best-effort local perception tool, not a kernel audit system.
