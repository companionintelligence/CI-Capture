# common.rs

## Purpose
Provides baseline cross-platform collectors used as initial implementation and fallback. Supplies working `wake`, `snapshot`, and filesystem-delta behavior before deep native integrations.

## Components

### `BaselineObserver`
- **Does**: Produces live observations and tracks filesystem event deltas.
- **Interacts with**: `notify` watcher, `Observation` schema types, net helpers in `util/net.rs`.

### `BaselineWaker`
- **Does**: Produces wake orientation payload from portable system probes.
- **Interacts with**: `sysinfo`, `if_addrs`, filesystem scans, and schema types.

### Helper functions (`build_home_tree`, `recent_files`, `installed_apps`, etc.)
- **Does**: Fill specific wake fields with deterministic best-effort data.
- **Interacts with**: Standard library IO, external crates, and schema DTOs.
- **Rationale**: File activity helpers rank recent files by the freshest available created/accessed/modified timestamp and attach the same metadata to live fs events when the path still exists.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `observer/mod.rs` | Implements `Observer` and `Waker` traits | Trait method signature changes |
| `main.rs` | `snapshot` and `wake` return serializable payloads | Returning partial invalid objects |
| Future OS collectors | Baseline semantics remain a fallback path | Removing fallback without replacement |

## Notes
Many fields are intentionally conservative placeholders in v0 baseline (for example deeper per-window semantics), to be incrementally replaced by native collectors. Public-IP lookup is best-effort with short timeouts to avoid blocking CLI responsiveness. Recent files are chosen objectively as the five freshest files within the scanned home-tree depth using created/accessed/modified timestamps when available. Notify-backed fs deltas surface create/modify/delete/rename and include file activity metadata only when the path can still be `stat`ed. Uptime uses boot-time derived logic with sanity caps to avoid host-specific `sysinfo` anomalies.
