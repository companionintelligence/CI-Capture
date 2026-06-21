# observation.rs

## Purpose
Defines all serialized JSON contracts for `WakeObservation` and live `Observation` payloads. This file is the schema boundary between `vz` and downstream consumers.

## Components

### `WakeObservation`
- **Does**: Represents cold-start orientation data.
- **Interacts with**: Populated by `BaselineWaker` in `observer/common.rs`.
- **Rationale**: Supports compacting via `WakeObservation::compact` for low-token default wake output.

### `Observation`
- **Does**: Represents live-state snapshots collected repeatedly.
- **Interacts with**: Produced by `Observer::snapshot`, diffed in `diff.rs`.
- **Rationale**: Supports `Observation::compact` so the bare `vz` command can stay readable without changing the full `vz snapshot` contract.

### Nested DTO structs
- **Does**: Model strongly typed payload sections (machine, windows, network, filesystem, etc.).
- **Interacts with**: CLI serialization in `main.rs` and tests.
- **Rationale**: `FileActivityInfo` captures best-effort created/accessed/modified ages so wake and fs deltas can describe recent file activity without platform-specific audit hooks.

### `WakeObservation::compact`
- **Does**: Prunes wake payload volume (groups, home tree section omission, port list size, shell wrappers, local sessions) while preserving schema shape. Recent files are retained as an objective top-5 by freshest available file activity time.
- **Interacts with**: Applied by default in `main.rs`; bypassed by `--verbose`.

### `Observation::compact`
- **Does**: Collapses duplicate active network connections by app/process into an explicit aggregate row with optional `connection_count` and `remote_host_count`.
- **Interacts with**: Applied only for bare `vz` in `main.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `observer/common.rs` | Field names/types are stable and serializable | Renaming fields, type changes |
| `diff.rs` | `Observation` is serializable and cloneable | Removing serde derive |
| External consumers | JSON shape is versioned with `schema_version` | Structural schema changes without bump |

## Notes
The schema is intentionally expansive; collectors may initially return empty vectors or null-able fields and gain fidelity over time. Compact wake mode reduces token cost by shrinking arrays and filtering noisy values without breaking field compatibility.
