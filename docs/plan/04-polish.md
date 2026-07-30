# Phase 4 — Polish, privacy, security

**Tracks:** post-MVP polish for [#3](https://github.com/companionintelligence/CI-Capture/issues/3) and [#4](https://github.com/companionintelligence/CI-Capture/issues/4).
**Branch:** `phase-4-polish`
**Coordinates:**
- **CI-Server:** short-lived signed upload URLs, device-revocation endpoint, dead-letter visibility API.
- **CI-Portal:** revoke device button per session.

---

## 1. Goal

Take the Phase-1+2+3 stack and make it actually shippable as a tray utility. Issue #4 §§12–14 are the spec; this phase implements them.

Four buckets:

1. **Privacy controls** — what the user can pause, exclude, restrict.
2. **Performance budget** — keep idle CPU low, throttle on battery, bound memory.
3. **Security hardening** — short-lived signed upload URLs, encrypted local cache, revocable devices.
4. **Observability** — logs, dead-letter UX, sync-health surfacing.

## 2. Privacy controls (issue #4 §12)

| Control | Where it lives | Default |
|---|---|---|
| Pause sync (per-lane: screenshot, vizier, file) | Tray menu + Settings | Running |
| Watched-folder selection | Settings | Empty list |
| Per-folder ignore list (glob) | Settings | `node_modules/`, `.git/`, OS caches (Phase 2 §7) |
| Per-folder policy | Settings | `metadata-only` for `Downloads`, `full` for `Documents`, `ignore` for browser profiles |
| Sensitive-path block list | Hard-coded + user-editable | Keychains, `.ssh`, `.aws/credentials`, password vaults |
| File-type exclusions | Settings | Off |
| Max file size | Settings | 100 MB |
| No-binary-upload mode (global) | Settings | Off |
| LAN-only mode (only sync when on same subnet as CI-Server) | Settings | Off |
| Bandwidth cap | Settings | None |
| Battery throttling | Settings | "Reduce on battery" on |

UX rule of thumb: every privacy default that protects the user should be **on**, every default that surveils should be **off** or scoped. The list above follows that.

## 3. Performance budget (issue #4 §13)

Targets (measure these in a follow-up PR):

| Metric | Target |
|---|---|
| Idle CPU (no file activity, no screenshot tick) | < 1% on a recent laptop |
| Peak RAM (steady state) | < 200 MB |
| 1-min CPU during heavy extraction | < 30% (capped by worker pool size) |
| Battery impact (Apple's Activity Monitor "energy") | "Low" while idle |
| Disk I/O (queue writes) | < 1 MB / hour at idle |

Mechanisms:

| Knob | Where |
|---|---|
| Extraction worker-pool size | `extractors/mod.rs` — derived from `num_cpus`. |
| Per-format concurrency (e.g. `pdf=1` while OCR runs) | `tokio::sync::Semaphore` map. |
| On-battery downscale | Halve pool size when `battery::on_battery()` true. |
| Adaptive batch size in sync engine | `sync/batching.rs` — start at 16, grow up to 256 on success. |
| Watcher storm summary mode (Phase 2 §9) | `watcher/normalizer.rs`. |
| Skip-on-screensaver (already done for screenshots) | extend to extraction. |

## 4. Security hardening (issue #4 §14)

| Item | Today | After this phase |
|---|---|---|
| Device-scoped token | Yes (pairing) | Unchanged. |
| Token rotation | No | `POST /devices/rotate-token`, automatic on heartbeat after N days. |
| Upload auth | Bearer on every request | Short-lived signed URL per blob upload; metadata still uses Bearer. |
| TLS | Yes (rustls) | Pin server cert if user opts in. |
| Local cache encryption | No | `~/.config/Companion Capture/cache/` encrypted with key in OS keychain (`security-framework` / `winapi credentials` / `secret-service`). |
| Audit logs (client-side) | No | Append-only `~/.config/Companion Capture/audit.log` of every send + every config change. |
| Device revocation | No | Settings shows other paired devices; revoke button calls `POST /devices/{id}/revoke` (server-owned). |
| Per-device signing keys | No | Out of scope here — flag for a later phase if needed. |

## 5. Observability

| Surface | What | How |
|---|---|---|
| Tray status string | "Synced ✓" / "12 pending" / "Paused" / "Sync error" | Worker emits status events; Tauri tray label binds to them. |
| Settings → Sync health panel | events in queue, oldest pending age, last successful sync, last error | New webview tab. |
| Settings → Dead-letter panel | files that have failed extraction or upload 3+ times | Reads from SQLite queue's `extraction_dlq` and `events` (where `attempts >= 5`). User can retry or discard. |
| Logs | Structured JSON to `~/.config/Companion Capture/logs/applet.YYYY-MM-DD.log` + rotated weekly | `tracing` + `tracing-appender`. |
| Open-logs button | Settings → "Open log folder" | `tauri-plugin-opener`. |
| Crash reports | Optional opt-in | `panic_hook` writes a redacted crash file; user can attach to a bug report. |

## 6. UX details

| Surface | Today | After |
|---|---|---|
| First-run | Tray icon appears; user has to know to click Sign in. | Webview pops once on first launch with "Sign in via Portal" → folder picker → done. |
| Tray menu | Pause/Resume, Sign in, Quit, Settings | + Sync status string, + per-lane Pause submenu, + Open recent activity (deep link to CI-Server timeline). |
| Settings | Single window | Tabs: General, Identity, Watched Folders, Privacy, Sync Health, Advanced. |
| Toast on error | None | When sync has been failing > 10 min, a Tauri notification fires. Once per session, not spammy. |
| Quit dialog | None | If quit pressed while sync queue is non-empty, ask "Quit anyway? N events will be sent next launch." |

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Encrypted local cache breaks if OS keychain is unavailable (some Linux setups). | Fall back to in-memory only with a visible warning; never silently store unencrypted. |
| Pinned cert breaks when CI-Server rotates certs. | Pin via SPKI of the issuing CA, not the leaf cert; user can disable pinning. |
| Battery-throttle masks real bugs ("why is sync so slow?"). | Surface "Throttled because on battery" in the sync-health panel. |
| Dead-letter panel grows unbounded over years. | Auto-prune entries older than 30 days unless the user explicitly retried them. |

## 8. Exit criteria

- [ ] First-run flow: install → tray icon → click → pair → pick folders → sync works in < 2 minutes.
- [ ] Settings has all six tabs above with the privacy controls listed in §2.
- [ ] Idle CPU < 1% steady-state (measured with `tlrr` or Activity Monitor for 10 min).
- [ ] Plug in / unplug AC: extraction pool size changes; visible in Sync Health panel.
- [ ] Settings → Revoke device → next sync attempt fails with "token revoked"; user is prompted to re-pair.
- [ ] Logs rotate weekly; old logs > 30 days auto-deleted.
- [ ] Dead-letter panel lists at least one synthetic failure and lets the user retry it.
- [ ] Toast appears once when sync has been failing > 10 min; not before.
