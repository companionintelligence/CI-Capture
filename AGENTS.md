# AGENTS.md — CI-Applet

## Platform context

CI-Applet is the **Capture** layer component of the Companion Intelligence platform — a private, local-first digital memory system.

Full architecture: `CI-Engineering/architecture/architecture.md`

## Repo summary

Rust/Tauri desktop applet — a Companion Intelligence fork of Screenpipe that captures screen, audio, and activity data and streams it to CI-Server.

## Stack

- Rust / Cargo (core capture engine)
- Tauri (desktop shell)
- Bun (JS tooling)
- Screenpipe pipeline

## Setup & commands

```bash
bun install && cargo build   # install
cargo run   # dev
cargo test   # test
cargo build --release   # build
```

## Screenpipe ancestry

This repo forks the screenpipe capture engine. Keep that lineage in mind when merging upstream changes — CI-specific additions live in `src/ci/`.

## Git discipline

- Never `git reset --hard` or `git clean -f` without user confirmation.
- Commit messages: imperative mood, ≤ 72 chars.
- Reference CI-Engineering issues in PRs (e.g. `Closes companionintelligence/CI-Engineering#32`).

## Confidentiality

Private & Confidential — Property of Lifescope Inc. Do not distribute.
