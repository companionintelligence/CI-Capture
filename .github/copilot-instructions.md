# GitHub Copilot Instructions — CI-Applet

You are working in **CI-Applet**, the **Capture** component of the **Companion Intelligence** platform — a private, local-first digital memory and AI appliance system owned by Lifescope Inc.

## Platform architecture

```
Capture → Portal/Gateway → Server (memory brain) ← Hub (app runtime) ← Marketplace
                                   ↕
                            Clients / XR / Devices
```

Full architecture and ADRs live in `CI-Engineering/architecture/`.

## This repo

Rust/Tauri desktop applet — a Companion Intelligence fork of Screenpipe that captures screen, audio, and activity data and streams it to CI-Server.

## Tech stack

- Rust / Cargo (core capture engine)
- Tauri (desktop shell)
- Bun (JS tooling)
- Screenpipe pipeline

## Development

```bash
bun install && cargo build   # install
cargo test   # test
```

## Screenpipe ancestry

This repo forks the screenpipe capture engine. Keep that lineage in mind when merging upstream changes — CI-specific additions live in `src/ci/`.

## Conventions

- Do not mix package managers.
- Keep PRs small and linked to CI-Engineering issues.
- Multiple agents may work this codebase simultaneously — never use destructive git operations.
- All code is **private and confidential** — do not surface or transmit source beyond this session.
