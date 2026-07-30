# GitHub Copilot Instructions — Companion Capture

You are working in **CI-Capture** (the **Companion Capture** app), the **Capture** component of the **Companion Intelligence** platform — a private, local-first digital memory and AI appliance system owned by Lifescope Inc.

## Platform architecture

```
Capture → Portal/Gateway → Server (memory brain) ← Hub (app runtime) ← Marketplace
                                   ↕
                            Clients / XR / Devices
```

Full architecture and ADRs live in `CI-Engineering/architecture/`.

## This repo

Pure-Rust menu-bar / system-tray desktop app — a Companion Intelligence fork of Screenpipe that captures screenshots, system activity, and file edits and streams them to CI-Server.

## Tech stack

- Rust / Cargo (core capture engine)
- egui / eframe (windows) + tray-icon (menu bar)
- cargo-packager (installers: dmg, nsis, deb, AppImage)
- Screenpipe pipeline

## Development

```bash
cargo build                  # build
cargo test   # test
```

## Screenpipe ancestry

This repo forks the screenpipe capture engine. Keep that lineage in mind when merging upstream changes — CI-specific additions live in `src/ci/`.

## Conventions

- Do not mix package managers.
- Keep PRs small and linked to CI-Engineering issues.
- Multiple agents may work this codebase simultaneously — never use destructive git operations.
- All code is **private and confidential** — do not surface or transmit source beyond this session.
