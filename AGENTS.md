# AGENTS.md — Companion Capture

## Platform context

Companion Capture (repo `CI-Capture`) is the **Capture** layer component of the Companion Intelligence platform — a private, local-first digital memory system.

Full architecture: `CI-Engineering/architecture/architecture.md`

## Repo summary

Pure-Rust menu-bar / system-tray desktop app — a Companion Intelligence fork of Screenpipe that captures screenshots, system activity, and file edits and streams them to CI-Server.

## Stack

- Rust / Cargo (core capture engine)
- egui / eframe (windows) + tray-icon (menu bar)
- cargo-packager (installers: dmg, nsis, deb, AppImage)
- Screenpipe pipeline

## Setup & commands

```bash
cargo build                  # build
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
