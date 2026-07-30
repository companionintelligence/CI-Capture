# ANTIGRAVITY.md — Companion Capture

## Context

**Companion Capture** (repo `CI-Capture`) — **Capture** layer of the Companion Intelligence platform (Lifescope Inc).

Architecture: `CI-Engineering/architecture/architecture.md`
Roadmap: `CI-Engineering/architecture/roadmap.md`

## Purpose

Pure-Rust menu-bar / system-tray desktop app — a Companion Intelligence fork of Screenpipe that captures screenshots, system activity, and file edits and streams them to CI-Server.

## Stack

- Rust / Cargo (core capture engine)
- egui / eframe (windows) + tray-icon (menu bar)
- cargo-packager (installers: dmg, nsis, deb, AppImage)
- Screenpipe pipeline

## Commands

```bash
cargo build
cargo run
cargo test
```

## Rules

- Package manager: do not switch or mix.
- Destructive git operations (`reset --hard`, `clean -f`) require explicit user confirmation.
- Commits reference CI-Engineering issues.
- All content is **private and confidential** — Property of Lifescope Inc.
