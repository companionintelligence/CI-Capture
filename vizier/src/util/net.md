# net.rs

## Purpose
Encapsulates network connection and listening-port collection. Keeps network parsing isolated from observation assembly code.

## Components

### `collect_active_connections`
- **Does**: Returns active connection records for `Observation`.
- **Interacts with**: `ConnInfo` schema type.
- **Rationale**: Uses `lsof` parsing on macOS to avoid privileged kernel table access in constrained environments.

### `collect_listening_ports`
- **Does**: Returns open listening ports for `WakeObservation`.
- **Interacts with**: `ListeningPort` schema type.
- **Rationale**: Uses `lsof` LISTEN rows on macOS and degrades to empty output when unavailable.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `observer/common.rs` | Functions exist and return schema vectors | Renaming functions or return types |

## Notes
Current implementation now includes macOS parsers backed by `lsof` and Linux parsers backed by `ss`; duplicate rows are deduplicated and loopback traffic is excluded unless explicitly requested. Non-macOS/non-Linux targets remain placeholder until their platform-specific collectors are implemented.
