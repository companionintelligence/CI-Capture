# cli_behavior.rs

## Purpose
Integration tests for CLI contract behavior and flag semantics. These tests guard invocation details (`vz` usage text, watch stream shape, and flag effects) independent of platform-specific collector internals.

## Components

### `help_uses_vz_command_name`
- **Does**: Verifies help text exposes the intended `vz` command name and optional command position.
- **Interacts with**: Clap parser wiring in `main.rs`.

### `wake_no_public_ip_omits_public_ip_field`
- **Does**: Verifies wake output omits `network_identity.public_ip` when `--no-public-ip` is set.
- **Interacts with**: Wake config handling in `main.rs` and observer backends.

### `wake_defaults_to_compact_profile`
- **Does**: Verifies default wake output uses compacted content (bounded groups/history and omitted `filesystem.home_tree`) while still exposing recent-file activity metadata.
- **Interacts with**: `WakeObservation::compact` in `observation.rs`.

### `wake_verbose_returns_larger_payload`
- **Does**: Verifies `--verbose` bypasses compacting and returns a larger wake payload.
- **Interacts with**: `--verbose` flag handling in `main.rs`.

### `no_subcommand_defaults_to_snapshot`
- **Does**: Verifies bare `vz` emits a one-shot compact, pretty-printed snapshot payload instead of entering streaming mode, and that grouped connection rows are clearly marked as aggregates.
- **Interacts with**: Default command fallback in `main.rs`.

### `no_subcommand_verbose_returns_full_snapshot`
- **Does**: Verifies `vz --verbose` bypasses default snapshot compaction while keeping the default pretty formatting.
- **Interacts with**: `--verbose` handling in `main.rs`.

### `explicit_snapshot_remains_compact_without_pretty`
- **Does**: Verifies `vz snapshot` stays single-line JSON unless `--pretty` is requested, so script-oriented callers keep a compact one-shot path.
- **Interacts with**: Snapshot formatting selection in `main.rs`.

### `snapshot_all_connections_is_superset`
- **Does**: Verifies `--all-connections` does not reduce observed network connection rows.
- **Interacts with**: Active connection collector in `util/net.rs`.

### `watch_diff_emits_full_snapshot_then_patch`
- **Does**: Verifies watch diff mode outputs an initial full snapshot followed by patch envelopes.
- **Interacts with**: Stream loop in `main.rs` and patch builder in `diff.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| CLI users | Stable behavior for documented flags, stream format, and `vz` binary name | Flag semantic changes without test updates |
| CI | Fast, deterministic CLI-level contract checks | Tests depending on non-deterministic timing or global machine state |
