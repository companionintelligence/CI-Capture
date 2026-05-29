# CLAUDE.md — CI-Applet

> **Private & Confidential — Property of Lifescope Inc. Do not distribute.**

## Platform role

**CI-Applet** sits in the **Capture** layer of the Companion Intelligence platform.

Architecture: `CI-Engineering/architecture/architecture.md`
Roadmap: `CI-Engineering/architecture/roadmap.md`
Issues: <https://github.com/companionintelligence/CI-Engineering/issues>

## What this repo does

Rust/Tauri desktop applet — a Companion Intelligence fork of Screenpipe that captures screen, audio, and activity data and streams it to CI-Server.

## Stack

- Rust / Cargo (core capture engine)
- Tauri (desktop shell)
- Bun (JS tooling)
- Screenpipe pipeline

## Key commands

```bash
# Install dependencies
bun install && cargo build

# Start development server / service
cargo run

# Run tests
cargo test

# Build for production
cargo build --release
```

## Package manager

**cargo** — use exclusively; do not mix with npm/yarn/pnpm/bun unless the repo explicitly requires it.

## Cross-repo connections

- Streams captured data to CI-Server (screen activity, audio, accessibility text)
- Shares screenpipe pipeline with Companion-Pipe
- macOS TCC: signing identity required for consistent permissions

## Screenpipe ancestry

This repo forks the screenpipe capture engine. Keep that lineage in mind when merging upstream changes — CI-specific additions live in `src/ci/`.

## Multi-agent safety

Multiple AI agents work these repos in parallel. Never use `git reset --hard`, `git clean -f`, or any destructive git command without explicit confirmation from the user.

## Confidentiality

Private & Confidential — Property of Lifescope Inc. Do not distribute.
