# Phase 1 — Tauri v2 shell migration

**Tracks:** [#1](https://github.com/companionintelligence/CI-Applet/issues/1)
**Branch:** `phase-1-tauri-shell`
**Coordinates:** none (CI-Server / CI-Portal contracts untouched).

---

## 1. Goal

Replace the current `tao` + `tray-icon` + `eframe/egui` shell with **Tauri v2**, so:

- The tray + windows are managed by one cross-platform framework.
- Settings + upload UIs become webviews (TS/React), which is also where Phase 4 privacy controls and the Phase 2 watched-folder picker will live.
- Distribution is unified: signed `.dmg` / `.msi` / `.AppImage` / `.deb` artifacts come out of a single Tauri build pipeline.

The capture loop, pairing, and ingest code (`worker.rs`, `analysis.rs`, `server_auth.rs`, `server_ingest.rs`) stay; they get re-exposed to the new shell as Tauri commands.

## 2. Why Tauri v2

| Reason | Notes |
|---|---|
| Tray + multi-window in core (no plugin gymnastics in v2). | v2 stabilized the tray API. |
| Webview UI = same stack as the rest of the CI front-end (TS/React, matching CI-Portal). | Reduces context-switch cost across the org. |
| Smaller installer footprint than Electron. | Important because this is a background utility. |
| Native menu items + global shortcuts. | Replaces what `tray-icon::menu` and our ad-hoc menu code do today. |
| First-class auto-updater. | Today there's no update path. |

## 3. Target structure

```text
CI-Applet/
├── src-tauri/                   # NEW — Tauri host (Rust)
│   ├── Cargo.toml               # Tauri-flavored, depends on the workspace crate below
│   ├── tauri.conf.json          # tray, windows, updater, bundle ids
│   ├── build.rs
│   └── src/
│       └── main.rs              # Tauri::Builder, registers commands + tray
├── src/                         # KEPT — pure-logic library crate (no UI)
│   ├── lib.rs                   # NEW — re-exports the modules below as a library
│   ├── analysis.rs              # unchanged
│   ├── capture.rs               # unchanged
│   ├── config.rs                # unchanged
│   ├── server_auth.rs           # unchanged
│   ├── server_ingest.rs         # unchanged
│   ├── server_upload.rs         # unchanged
│   ├── server_common.rs         # unchanged
│   ├── worker.rs                # unchanged
│   ├── db.rs / minio.rs         # legacy, still present, see Phase 0
│   └── svg_icon.rs              # may delete — Tauri can rasterize SVG itself
├── ui/                          # NEW — webview front-end
│   ├── package.json
│   ├── vite.config.ts
│   └── src/
│       ├── routes/
│       │   ├── settings/
│       │   └── upload/
│       └── lib/tauri.ts         # typed wrappers for `invoke("command_name", …)`
└── Cargo.toml                   # KEPT — top-level workspace declaring src/ + src-tauri/
```

Key move: **`src/` becomes a library crate, not a binary.** The binary lives in `src-tauri/`. The current `src/main.rs` (tray + menu + sub-process spawning) is *replaced*, not migrated — the new `src-tauri/src/main.rs` is much shorter.

## 4. Migration mapping

| Today | Tomorrow |
|---|---|
| `src/main.rs::MenuState` (tray menu via `tray-icon::menu`) | Tauri `TrayIconBuilder` + `MenuBuilder` in `src-tauri/src/main.rs`. |
| `tao::event_loop::EventLoopBuilder` + custom `AppEvent` | Tauri's built-in event system + `AppHandle::listen` / `emit`. |
| `Command::new(std::env::current_exe()).arg("--settings-ui")` (spawning a child process for the egui window) | `WebviewWindowBuilder::new(app, "settings", ...)` opens a Tauri window in-process. |
| `Command::new(std::env::current_exe()).arg("--upload-ui")` | Same — webview window with `url = "/upload"`. |
| `settings_ui.rs` (egui) | `ui/src/routes/settings/` (React) + `#[tauri::command]` functions for `get_config` / `save_config` / `start_pairing` / `complete_pairing`. |
| `upload_ui.rs` (egui) | `ui/src/routes/upload/` + commands `pick_files`, `upload_files`, `poll_parse_status`, all delegating to `server_upload.rs`. |
| `svg_icon.rs` (resvg/usvg rasterize) | Tauri's icon pipeline; the SVG goes into `src-tauri/icons/`. Module probably deleted. |
| `launch_portal_login` opens browser via `Command::new("open"/"xdg-open"/"cmd")` | `tauri-plugin-opener::open(url)` (cross-platform). |
| `tinyfiledialogs::message_box` for errors | `tauri-plugin-dialog`. |
| `tinyfiledialogs::open_file_dialog_multi` for file picker | `tauri-plugin-dialog::open()`. |

## 5. Commands exposed to the webview

| Command | Calls into | Used by |
|---|---|---|
| `get_config` | `ConfigManager::load_or_default` | Settings window. |
| `save_config(config)` | `ConfigManager::save` + worker `UpdateConfig` event | Settings window. |
| `start_pairing` | `server_auth::start_pairing` + `tauri-plugin-opener` | Settings window "Sign in via Portal" button. |
| `pairing_status` | polls `server_auth::wait_for_pairing` | Settings window. |
| `pick_files` | `tauri-plugin-dialog` | Upload window. |
| `upload_files(paths)` | `server_upload::upload_files` | Upload window. |
| `poll_parse_status(file_id)` | `server_upload::poll_parse_status` | Upload window. |
| `worker_pause` / `worker_resume` | `WorkerHandle` | Tray menu *and* settings window. |
| `worker_status` | `WorkerHandle::is_running` | Tray menu label + settings window. |

## 6. Step-by-step migration

1. **Scaffold.** `npm create tauri-app@latest` in a side directory; copy `src-tauri/`, `ui/`, root `package.json` into this repo.
2. **Promote `src/` to a library.** Add `src/lib.rs` re-exporting modules. Keep `src/main.rs` temporarily so the old binary still builds for safety.
3. **Re-implement the tray** in `src-tauri/src/main.rs`. Wire up two menu items (Pause/Resume, Sign in via Portal, Quit) and verify with a hard-coded callback.
4. **Wire `WorkerHandle`.** Spawn it from the Tauri setup hook; stash in `App::manage(...)`. Hook tray Pause/Resume to it.
5. **Settings webview.** Stand up the React route at `/settings`; implement `get_config` / `save_config` commands; achieve parity with `settings_ui.rs` field by field.
6. **Upload webview.** Same as settings — webview at `/upload`, react to the existing `StatusUpdate`-like protocol but expressed as Tauri events emitted from the upload commands.
7. **Pairing.** Migrate `launch_portal_login` to `tauri-plugin-opener::open`; webview reflects pairing progress (today this is a `tao` custom event into the tray).
8. **Delete `src/main.rs`** (replaced by `src-tauri/src/main.rs`).
9. **Drop `tao`, `tray-icon`, `eframe`, `egui`, `tinyfiledialogs`, `resvg`, `usvg`, `screenshots` (Tauri has its own primary-display capture — verify before dropping), `image` (still needed for PNG encode of captures)** from `Cargo.toml` as each becomes unused.
10. **Bundle config.** Fill in `tauri.conf.json`: bundle identifiers, macOS LSUIElement (so the app is tray-only), Windows install scope, Linux desktop file, code-signing cert refs.
11. **Updater.** Configure `tauri-plugin-updater` against a CI-Marketplace-hosted manifest. (Coordinate with CI-Marketplace.)

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| `screenshots` crate vs Tauri's screen-capture story differ subtly on macOS permissions. | Keep `screenshots` for now; Tauri's primary value here is the shell, not the capture primitive. Drop `screenshots` only after verifying replacement on all three OSes. |
| macOS code-signing + notarization breaks if certs aren't ready. | Land the migration as a build-but-don't-sign milestone first; sign in a follow-up PR. |
| egui→React rewrite of `upload_ui.rs` loses subtle UX (the per-file Phase state machine). | The Phase state machine is the contract — re-implement it as a TS discriminated union literally matching the Rust `Phase` enum. |
| `--settings-ui` / `--upload-ui` CLI flags are documented somewhere. | Keep them as no-op aliases that open the corresponding webview (just in case a user has muscle memory or a launcher shortcut). |
| Tauri v2 plugin churn between minor versions. | Pin exact versions in `Cargo.toml` and `package.json`. |

## 8. Exit criteria

- [ ] One binary (`ci-applet`) replaces the multi-process tray + sub-window model.
- [ ] Tray works on macOS, Windows, Linux; Pause/Resume toggles worker.
- [ ] Settings webview reaches feature parity with `settings_ui.rs` (portal URL, manual UUID, interval, vizier toggles).
- [ ] Upload webview reaches feature parity with `upload_ui.rs`, including the Polling/AlreadyUploaded/Done state machine.
- [ ] `npm run tauri build` produces installers for all three OSes (signing can land separately).
- [ ] Pairing flow (start → browser → token persisted) still works end-to-end against the local CI-Portal.
- [ ] `cargo deny` shows no remaining direct deps on `tao`, `tray-icon`, `eframe`, `egui`, `tinyfiledialogs`.
