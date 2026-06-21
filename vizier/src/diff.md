# diff.rs

## Purpose
Generates RFC 6902 JSON Patch envelopes between consecutive live observations. Keeps watch streams compact for long-running agent sessions.

## Components

### `DiffEnvelope`
- **Does**: Wraps patch operations with timestamp and monotonic clock metadata.
- **Interacts with**: Emitted by `watch --diff` in `main.rs`.

### `create_diff_envelope`
- **Does**: Serializes observations to JSON values and computes a patch.
- **Interacts with**: `json_patch::diff` and `Observation` in `observation.rs`.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | Returns valid serializable patch envelopes | Changing return type or envelope fields |
| Downstream stream consumers | `patch` follows JSON Patch operation format | Replacing RFC 6902 representation |

## Notes
Patch generation is purely data-oriented and side-effect free.
