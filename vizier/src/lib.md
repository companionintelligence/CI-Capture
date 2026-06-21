# lib.rs

## Purpose
Library entry point exposing reusable modules for schema, collectors, and diffing. Enables integration tests and external embedding without invoking the CLI binary.

## Components

### Module exports
- **Does**: Re-exports `diff`, `observation`, `observer`, and `util` modules.
- **Interacts with**: `main.rs` and integration tests.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | Module paths resolve through crate root (`vizier::...`) | Removing or renaming module exports |
| Integration tests | Public APIs importable from `vizier` crate | Switching back to binary-only crate |
