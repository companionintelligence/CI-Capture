# Phase 4 — Polish, privacy, security — working checklist

This branch implements [docs/plan/04-polish.md](docs/plan/04-polish.md). It bundles in the Phase 5 plan ([docs/plan/05-intelligence.md](docs/plan/05-intelligence.md)) since Phase 5 is primarily cross-repo and only depends on Phase 4 polish for its applet-side surfaces.

Read both docs first. This branch starts with **docs only**: every item below lands as its own sub-PR after Phases 1–3 are merged.

---

## Prerequisites

- [ ] Phase 1 (Tauri shell) merged — the Settings tabs and tray menu items below live in webview / Tauri tray, not egui / tao.
- [ ] Phase 2 (file watcher) merged — most privacy controls gate Phase 2 + 3 surfaces.
- [ ] Phase 3 (conversion) merged — the conversion pool and worker config are what battery + concurrency knobs apply to.

## Sub-PR list

### A. Privacy controls (plan § 2)

- [ ] Per-lane pause (screenshot / vizier / file) — three independent flags in `AppConfig::worker`.
- [ ] Watched-folder picker (already starts in Phase 2; this PR adds drag-to-reorder and per-folder policy dropdown).
- [ ] Ignore-list editor: defaults pre-populated, each individually toggleable.
- [ ] Sensitive-path block list — hard-coded entries are read-only; user can append.
- [ ] File-type exclusion editor.
- [ ] Max file size slider (default 100 MB, hard cap 2 GB).
- [ ] No-binary-upload global toggle.
- [ ] LAN-only mode (skip sync unless on same /24 as the configured CI-Server host).
- [ ] Bandwidth cap (KB/s).

### B. Performance budget (plan § 3)

- [ ] Battery probe via `battery` crate; emit `on_battery=true|false` on every status tick.
- [ ] Extraction pool downscaled to `pool_size / 2` while `on_battery`.
- [ ] Adaptive sync batching (already drafted in Phase 2 `src/sync/batching.rs`).
- [ ] Storm summary mode for the watcher (already drafted in Phase 2 `src/watcher/normalizer.rs`).
- [ ] Performance test harness: idle CPU/RAM over 10 min, baseline + after.

### C. Security hardening (plan § 4)

- [ ] Token rotation: `POST /devices/rotate-token` called on heartbeat after N days. **Coordinate with CI-Server.**
- [ ] Short-lived signed URLs for blob uploads. **Coordinate with CI-Server.**
- [ ] Optional cert pinning toggle (pin SPKI of the issuing CA, not leaf).
- [ ] Local cache encryption via OS keychain:
  - macOS: `security-framework`
  - Windows: `windows` crate `CredWriteW`
  - Linux: `secret-service` D-Bus
- [ ] Append-only client audit log: every send + every config change.
- [ ] Device revocation UI — Settings → Identity → "Paired devices" list with revoke button. **Coordinate with CI-Server + CI-Portal.**

### D. Observability (plan § 5)

- [ ] Tray status string driven by worker events.
- [ ] Settings → Sync Health tab: queue depth, oldest pending age, last sync, last error.
- [ ] Settings → Dead Letter tab: failed events + extractions with retry/discard buttons.
- [ ] Structured JSON logs via `tracing` + `tracing-appender`; weekly rotation; auto-prune > 30 days.
- [ ] "Open log folder" button.
- [ ] Opt-in crash reports: `panic_hook` writes redacted crash file.

### E. UX details (plan § 6)

- [ ] First-run flow: install → tray icon click → webview pops with Sign-in → folder picker → done.
- [ ] Tray submenu for per-lane Pause.
- [ ] "Open recent activity" tray item — deep-links into CI-Server timeline UI.
- [ ] Sustained sync-failure toast (one per 10 min, not spammy).
- [ ] Quit-while-pending warning dialog.

### F. Phase 5 applet-side surfaces (plan 05-intelligence.md § 5)

- [ ] Tray items: Today's activity / Recent files / Search.
- [ ] Global hotkey search palette via `tauri-plugin-global-shortcut`. Default unset.
- [ ] Daily-digest opt-in: schedule + notification. Default off.
- [ ] Focused-app + cwd annotation on `file_events` (reuse `vizier::observer` output).
- [ ] **Skip "Recall on file open"** unless explicit user demand justifies the per-OS shell hook complexity.

## Server-side coordinates

| Item | Repo | Notes |
|---|---|---|
| `POST /devices/rotate-token` | CI-Server | Token rotation. |
| `POST /history/blobs/upload?signed=true` returning signed URL | CI-Server | Blob upload hardening. |
| `POST /devices/{id}/revoke` | CI-Server | Device revocation. |
| "Paired devices" UI surface | CI-Portal | The revoke action originates from either CI-Portal or applet Settings. |
| Daily-digest API | CI-Server | `GET /history/digest?date=YYYY-MM-DD&folder=…`. |
| Search palette API | CI-Server | `POST /history/search` already on the Phase 5 plan. |

## Verification (matches plan §8)

- [ ] First-run flow < 2 minutes end-to-end.
- [ ] Idle CPU < 1% steady-state.
- [ ] Plug-in vs unplug AC changes extraction pool size, visible in Sync Health.
- [ ] Revoke a device → next sync 401s → user is prompted to re-pair.
- [ ] Logs rotate weekly, prune > 30 days.
- [ ] Dead-letter retry roundtrip works.
- [ ] Sustained-failure toast fires only after the 10-min threshold.

---

## Branch hygiene

- Each sub-section (A–F) is its own sub-PR. F can land before E if it's gated on a server endpoint that ships first.
- The cert-pinning and audit-log items have higher risk-of-regression — land those last and behind feature flags.
