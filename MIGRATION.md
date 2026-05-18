# Tauri v2 migration — working notes

This branch implements [docs/plan/01-tauri-shell.md](docs/plan/01-tauri-shell.md). It's intentionally separate from the architecture / roadmap PR so reviewers can focus on the shell move.

Read [01-tauri-shell.md](docs/plan/01-tauri-shell.md) first — the goals, target structure, command surface, and exit criteria all live there. This file is a working checklist.

---

## Working checklist

### Scaffold

- [ ] Generate a stock Tauri v2 project in a side directory (`npm create tauri-app@latest`, Rust + React/TS).
- [ ] Copy `src-tauri/` and the React `ui/` scaffold into this repo.
- [ ] Add `src-tauri/` to the workspace in the root `Cargo.toml`.
- [ ] Confirm `cargo build` and `npm run tauri dev` both work before touching any applet code.

### Library promotion

- [ ] Add `src/lib.rs` re-exporting every existing module (`analysis`, `capture`, `config`, `server_*`, `worker`, etc.).
- [ ] Update `Cargo.toml`: declare both `[lib]` and `[[bin]]` (keep the old binary temporarily for safety).
- [ ] Verify the existing `target/release/CI-Applet` still builds and runs unchanged.

### Tray

- [ ] In `src-tauri/src/main.rs`, build a tray icon (use `src-tauri/icons/` populated from `2024 CI Logo Mark.svg`).
- [ ] Wire menu items: Pause/Resume, Sign in via Portal, Settings, Upload, Quit.
- [ ] Set macOS `LSUIElement = true` in `tauri.conf.json` so the dock icon is suppressed.

### Worker hookup

- [ ] Spawn `WorkerHandle::spawn(config)` in Tauri's `setup` hook.
- [ ] `app.manage(WorkerHandle)` so commands can reach it.
- [ ] Implement `#[tauri::command] worker_pause / worker_resume / worker_status` delegating to the handle.
- [ ] Tray menu items bind to those commands.

### Settings webview

- [ ] React route `/settings` with tabs: General, Identity, Server, Capture, Vizier.
- [ ] Commands: `get_config`, `save_config(config)`, `start_pairing`, `pairing_status`.
- [ ] Achieve field-for-field parity with `src/settings_ui.rs` (current egui window).
- [ ] On save, fire a Tauri event so the worker reloads.

### Upload webview

- [ ] React route `/upload` with the file list + per-file status badges.
- [ ] Reimplement the `Phase` state machine from `src/upload_ui.rs:34` as a TypeScript discriminated union — same variants, same transitions.
- [ ] Commands: `pick_files`, `upload_files(paths)`, `poll_parse_status(file_id)`.
- [ ] Commands emit `StatusUpdate`-shaped events; webview subscribes.

### Pairing

- [ ] `start_pairing` command calls `server_auth::start_pairing` and `tauri-plugin-opener::open(url)`.
- [ ] Settings webview polls `pairing_status` until success or timeout.
- [ ] On success, persist token via `ConfigManager` and emit `pairing-completed` event.

### Dialogs + opener

- [ ] Replace every `tinyfiledialogs::message_box` with `tauri-plugin-dialog`.
- [ ] Replace every `Command::new("open"/"xdg-open"/"cmd")` with `tauri-plugin-opener::open`.

### Bundle

- [ ] Fill `tauri.conf.json`: bundle ids, version, app name, window defaults, updater endpoint.
- [ ] Linux: desktop file + `.deb` + `.AppImage` targets.
- [ ] macOS: `.dmg` target; flag for signing+notarization in a follow-up PR (entitlements file checked in but unsigned for now).
- [ ] Windows: `.msi` target.
- [ ] Verify `npm run tauri build` produces an installer for the current OS.

### Cleanup

- [ ] Delete `src/main.rs` once the Tauri binary covers every entry point.
- [ ] Delete `src/settings_ui.rs` and the `--settings-ui` CLI flag (keep flag as a no-op alias that opens the Settings webview if anyone has muscle memory).
- [ ] Delete `src/upload_ui.rs` and `--upload-ui` (same flag-aliasing rule).
- [ ] Drop unused deps from `Cargo.toml`: `tao`, `tray-icon`, `eframe`, `egui`, `tinyfiledialogs`. Keep `resvg`/`usvg` only if Tauri's icon pipeline can't consume the SVG directly; otherwise drop those too.
- [ ] `cargo deny check` clean.

### Auto-updater

- [ ] Configure `tauri-plugin-updater` against an update manifest URL hosted by CI-Marketplace.
- [ ] **Coordinate with CI-Marketplace** to publish the manifest endpoint before enabling.

### Verification

- [ ] Pairing roundtrip works end-to-end against a local CI-Portal.
- [ ] Capture loop still ingests to CI-Server in paired mode.
- [ ] Worker pause/resume works from both the tray and the Settings webview.
- [ ] Idle CPU did not regress (measured before and after).
- [ ] Bundle size: report new vs. old binary + installer size in the PR description.

---

## Risks tracked separately

See "Risks & mitigations" in [docs/plan/01-tauri-shell.md](docs/plan/01-tauri-shell.md). The highest one in practice is screen-capture permission interaction on macOS — verify on a clean machine, not a dev machine that's already granted permission.

## Branch hygiene

- This PR will be **large**. Land in stages by opening it as a draft and merging the scaffolding commit first, then sub-PRs for each section above into this branch.
- Do not include unrelated refactors. If something tempts you, open a separate issue.
