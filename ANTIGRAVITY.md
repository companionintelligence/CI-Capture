# ANTIGRAVITY.md — CI-Applet

## Context

**CI-Applet** — **Capture** layer of the Companion Intelligence platform (Lifescope Inc).

Architecture: `CI-Engineering/architecture/architecture.md`
Roadmap: `CI-Engineering/architecture/roadmap.md`

## Purpose

Rust/Tauri desktop applet — a Companion Intelligence fork of Screenpipe that captures screen, audio, and activity data and streams it to CI-Server.

## Stack

- Rust / Cargo (core capture engine)
- Tauri (desktop shell)
- Bun (JS tooling)
- Screenpipe pipeline

## Commands

```bash
bun install && cargo build
cargo run
cargo test
```

## Rules

- Package manager: do not switch or mix.
- Destructive git operations (`reset --hard`, `clean -f`) require explicit user confirmation.
- Commits reference CI-Engineering issues.
- All content is **private and confidential** — Property of Lifescope Inc.
