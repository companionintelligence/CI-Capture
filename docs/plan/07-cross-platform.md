# Phase 7 — Cross-platform parity (Linux and Windows)

**Tracks:** the "support multi-device sync across Mac, Windows, and Linux" thread of [#4](https://github.com/companionintelligence/CI-Capture/issues/4) and the cross-platform applet spec in [#3](https://github.com/companionintelligence/CI-Capture/issues/3); tracked in CI-Engineering by the umbrella issue [#281](https://github.com/companionintelligence/CI-Engineering/issues/281) with one issue per priority (P1–P12, linked from each heading below) so the gap is tracked somewhere other than this document. No open issue in either repo tracks Windows vizier, Linux Wayland/X11, multi-monitor, or lock detection today (survey: `gh` issue search across CI-Capture and CI-Engineering, 2026-09-15).
**Branch:** `clau/cross-platform-quick-wins` (stacked on the knowledge lane, `bc8be01`)
**Coordinates:**
- **CI-Server:** no change for the quick wins. P12 (app-usage sessionizer) reuses the `knowledge_*` kinds already accepted on `POST /v1/events`; whether app-usage is derived client- or server-side is open question 4 in §8.
- **CI-Portal:** no change.
- **CI-Engineering:** `architecture/platform-review.md:26,43` already treats the browser extension as the web-domain capture surface (§6); `data-mappings/ci-capture.md` item 2 (structured activity fields opaque in jsonb) is still open and gates the P12 decision.
- **macOS:** behaviour unchanged by the quick wins except where a fix is platform-neutral (`is_primary` display pick, vizier tests in CI, empty-observation skip) and for the locale resolution order: `CFLocaleCopyPreferredLanguages` (the UI language list) now precedes `defaults read -g AppleLocale` (the Region-format locale), which was previously first — see §5 row 2.

---

## 1. Goal

CI-Capture builds, signs and ships installers for macOS, Windows and Linux
(`.github/workflows/release.yml:124`), and README.md:5,17 calls it
cross-platform. Five parallel surveys (packaging/CI, documented gaps, user-facing
quality, cfg-gate inventory, native data sources — 2026-09-15) found that the
claim is **install-only**: every non-macOS `cfg` arm in `src/` and `vizier/` is
a stub or a fallback (~45 OS gates, "no Windows- or Linux-specific positive
implementation anywhere except `open_url` and tray icon size"), and the stubs
fail *silently* — the tray says "Monitoring" while the Activity lane ships empty
observations on Windows, a fake terminal window on non-Hyprland Linux, and
Wayland screenshot errors go to `log::error!` alone.

This phase has two halves:

1. **Quick wins (this branch, §5):** a batch of S-sized fixes that make the app
   *honest* on Linux and Windows today — no console window, no fake data, no
   phantom UI, correct locale and display — without changing macOS behaviour.
2. **A ranked plan (§4) for the real ports:** Windows and Linux vizier
   observers, lock/idle detection, start-at-login and single-instance, Wayland
   screenshots, per-lane health, release-gate parity, auto-update, and a
   cross-platform app-usage sessionizer that gives the knowledge lane meaning
   off macOS.

Why now: the knowledge lane (`06-knowledge.md`) made the macOS experience
materially richer, which widens the gap; the comparison doc has asked since
July for "either implement the vizier Windows backend or state the limitation
in README" (`docs/screenpipe-comparison.md:232-233`) and neither happened.

## 2. Where each capability stands today

Compiled from the five surveys; every cell is verified in code unless marked
*inferred*. "Degraded" means it runs but returns wrong or partial data;
"absent" means the arm returns a constant.

| Lane / capability | macOS | Linux | Windows |
|---|---|---|---|
| Screenshot backend (`screenshots` 0.8.10, `src/capture.rs:32-44`) | works (deprecated CGDisplay API; Sequoia periodic re-consent) | works on X11; broken/degraded on GNOME/KDE Wayland (portal prompt or failure every tick, log-only) | works |
| Primary-display pick / downscale / multi-monitor | degraded (first screen, full-res, one display) | degraded | degraded |
| Lock-screen suppression (`src/worker.rs:1126-1146`) | degraded (`pgrep ScreenSaverEngine` only) | absent (lock screen captured) | absent (lock screen captured) |
| Idle detection (`idle_ms`) | sampled, never used | absent (always 0) | absent (always 0) |
| Activity lane: focus / windows / displays (vizier) | works (CGWindowList) | Hyprland-only; synthetic `local-shell` elsewhere | stub (empty observation every tick, lane on by default) |
| Activity lane: net connections / ports | works | works | absent (empty `Vec`) |
| File tracker (notify) | works | works (no Linux ignore defaults) | degraded (rename → delete+create; `/`-only globs; no Windows sensitive globs) |
| Knowledge lane (knowledgeC.db) | works | idle; toggle still armable | idle; toggle still armable |
| iMessage lane | works (config-file only, no UI) | silently idles (`HOME`/Library assumption) | silently idles |
| BlueBubbles lane | works (no UI) | works (no UI) | works (no UI) |
| Permission / readiness model | TCC mirror | stub `NotRequired` | stub `NotRequired` |
| Onboarding | works | phantom "Permission" chip; "menu bar" copy | same |
| Settings lane list / copy | works | misleading ("4 lanes", "this Mac", "Paused while locked") | same |
| Tray icon (tray-icon 0.11) | works | degraded (no click events, no tooltip, black glyph, needs AppIndicator host) | degraded (black glyph on dark taskbar; console window) |
| Helper-window focus / single-instance | focus works; no single-instance | focus no-op; no single-instance | focus no-op; `process_is_running`=true; no single-instance |
| Start at login | absent | absent | absent |
| Auto-update / package-manager channel | absent | absent | absent |
| Locale "Match my system" | works | works | broken (always English) |
| Error dialogs / file pickers | works | degraded (needs zenity/kdialog, undeclared) | works |
| Lane-failure surfacing in tray/Settings | partial (Screen Recording denied only) | log-only | log-only |
| `companioncapture://` scheme | registered | absent | absent |
| Installer & signing | signed + notarized dmg (arm64 + x64) | deb/AppImage/pacman, GPG `.asc` only; deb internals leak; pacman artifact unusable | signed NSIS x64 (no arm64, no msi) |
| Release-time tests / smoke launch | none | preflight tests only; no smoke | none |
| vizier tests in CI | never run | never run | never run |
| Fonts / theme / telemetry | works | works | works |

Three rows are fine everywhere and are listed so nobody re-audits them: fonts
are `include_bytes!` (`src/theme.rs:331-334`), the Sentry scrubber has explicit
Windows drive/UNC path tests (`src/telemetry.rs:1793,1860`), and config/data
dirs come from `directories::ProjectDirs` (`src/config.rs:562-625`).

## 3. Principles

**Honesty over silent no-ops.** A lane that cannot run on this OS must say so
— in Settings, in the tray status, and in the data it ships. It must never
fabricate a plausible-looking observation (the `local-shell` window,
`vizier/src/observer/common.rs:168-215`), never POST an event with nothing in
it (`src/worker.rs:837-930` on Windows), and never offer a toggle that arms a
lane which idles (`src/settings_ui.rs:2268-2273` off macOS). Where the fix for
the underlying gap is M or L, the S-sized honesty fix ships first.

**Per-lane readiness, not a TCC mirror.** `permissions.rs` is a macOS TCC
mirror (`src/permissions.rs:207-227,268-270`: every non-macOS probe is
`NotRequired`, `permissions_relevant()` is `cfg!(macos)`), and both
`needs_attention()` (`src/main.rs:740-744`) and onboarding key off it. The
generalisation is a *readiness* model per lane — screenshot backend health,
Wayland portal grant, FDA, last success / last error / rows shipped — that the
tray and Settings render on every OS (P2). "Not required" and "unavailable on
this OS" are different states and get different copy.

**macOS parity is a north star, not a gate.** The macOS `Observation`
(focus app + title, window list, displays, cursor, idle;
`vizier/src/observer/macos.rs:40-66`) is the contract other OSes fill in over
time. A Linux or Windows change does not wait for full parity to ship: a
Windows observer with focus + idle and no window list is better than the
baseline, and is labelled as such. Conversely no quick win may regress macOS —
every change here is `cfg`-gated or platform-neutral and has a test that runs
on all three CI runners.

**Native packaging stays primary on Linux.** Every Linux source in §6 needs the
session bus, the system bus, or the Wayland socket from an unsandboxed process;
a Flatpak would need holes that defeat the sandbox (open question 11). deb /
AppImage remain the channel.

## 4. Priorities

Ranked by user impact × breadth. Effort: S ≤ 1 day, M ≤ 1 week, L > 1 week.
Each priority has its own CI-Engineering issue, linked from its heading; [#281](https://github.com/companionintelligence/CI-Engineering/issues/281) tracks the set.

### P1 — Implement a real vizier Windows observer (M) — [CI-Engineering#282](https://github.com/companionintelligence/CI-Engineering/issues/282)

**Why.** The Activity lane is enabled by default (`src/config.rs:151`) and
Settings shows it "on", but on Windows the observer is a 10-line wrapper around
`BaselineObserver` (`vizier/src/observer/windows.rs:1-10`) which, with `$SHELL`
unset, returns `focus: None` / `windows: []` / `displays: []` / `idle_ms: 0`
every tick (`vizier/src/observer/common.rs:169-232`); net connections are also
an empty `Vec` (`vizier/src/util/net.rs:20-23,38-40`). The worker still POSTs
one event per tick (`src/worker.rs:837-930`) and server ingest derives
title/description from `focus_title`/`focus_app` (`src/server_ingest.rs:405-433`),
so Windows users pay server rows for an empty timeline. Largest desktop
platform, and the project's own comparison doc lists this as an unhonoured
parity claim (`docs/screenpipe-comparison.md:82,120,232-233`).

**What.** Add a `WindowsObserver`: `GetForegroundWindow` →
`GetWindowThreadProcessId` → `QueryFullProcessImageNameW` + `GetWindowTextW`
for focus; `EnumWindows` + `IsWindowVisible` for the window list;
`EnumDisplayMonitors` for displays; `GetLastInputInfo` for `idle_ms`;
`GetCursorPos`; `GetExtendedTcpTable` for net. The `windows` / `windows-sys`
crates are already in `Cargo.lock` transitively (`windows` 0.61.3). Optional
later upgrade to `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`
(<https://devblogs.microsoft.com/oldnewthing/20131202-00/?p=2503>). Add a
cfg-gated test pinning the `Observation` shape. Caveat (web): UIPI hides titles
of elevated windows from a non-elevated process
(<https://github.com/blep/win32_window_monitor>).

**Evidence.** `vizier/src/observer/windows.rs:1-10`;
`vizier/src/observer/common.rs:169-232`; `vizier/src/util/net.rs:20-23,38-40`;
`src/config.rs:151`; `src/worker.rs:837-930`; `src/server_ingest.rs:405-433`;
`docs/screenpipe-comparison.md:82,120,232-233`; `docs/test-plan.md:393`;
`Cargo.lock` (`windows` 0.61.3).

### P2 — Per-lane health surfacing and an honest baseline observer (M; S part in this branch) — [CI-Engineering#283](https://github.com/companionintelligence/CI-Engineering/issues/283)

**Why.** Off macOS the tray says "Monitoring" no matter what fails: screenshot
errors go only to `log::error!` (`src/worker.rs:160-162`), `needs_attention()`
is false off macOS because it only mirrors TCC (`src/main.rs:740-744`),
knowledge FDA-missing is a warn log (`src/knowledge/mod.rs:261-267`), and the
baseline observer fabricates a `local-shell` focus window from `$SHELL` with
zero bounds (`common.rs:168-215`) so Memory shows a fake terminal all day on
GNOME/KDE/X11. This is the umbrella under which every other silent no-op hides.

**What.** *S, this branch:* stop fabricating the `local-shell` window when no
real observer exists (emit `focus: None`); skip the vizier POST and log a
one-time warn when an observation has no focus / windows / fs_events; label the
Activity lane "limited on this OS" in Settings. *M:* add a per-lane health row
(last success / last error / rows shipped) to the Settings sidebar and derive
the tray status line from it ("Last capture failed"), independent of the macOS
permission model; generalise `Permissions` into a per-lane readiness model
(backend health, Wayland portal grant, FDA). Add `session_type`
(x11/wayland/win32/aqua) and desktop environment to device registration
(`src/main.rs:1391-1397`) and Sentry tags so you can see which stub a user is
hitting.

**Evidence.** `src/worker.rs:160-162`; `src/main.rs:740-744`;
`src/knowledge/mod.rs:261-267`; `vizier/src/observer/common.rs:168-232`;
`src/settings_ui.rs:2077-2083`; `src/main.rs:1391-1397`;
`src/permissions.rs:207-227,268-270`.

### P3 — Start at login and single-instance guard on all three OSes (M) — [CI-Engineering#284](https://github.com/companionintelligence/CI-Engineering/issues/284)

**Why.** No LaunchAgent / `SMAppService`, XDG autostart `.desktop`, or HKCU
`Run` key exists anywhere (grep over `src/`, `packaging/`, `Cargo.toml` is
empty; `packaging/` holds only icons and `Info.plist`), so a capture agent goes
silent after every reboot on every platform. Nothing prevents two tray
processes racing on `config.json` and the queue DBs (`src/main.rs:178-226`
builds the tray with no lock), the Upload window has no PID guard
(`src/main.rs:425-434`), and `bring_pid_to_front` is a no-op off macOS
(`src/main.rs:959-987`) so a second "Open Settings" click does nothing while
the window is buried.

**What.** Add an "Open at login" toggle in Settings › Capture backed by the
`auto-launch` crate (macOS LaunchAgent/SMAppService, Windows HKCU Run, Linux
`~/.config/autostart`), default it on when onboarding completes, and add an
NSIS option. Take a file lock (`fd-lock` / `single-instance`) in the tray
branch of `main()` before the event loop and show a message box + exit if held.
Add the PID guard to `open_upload_window`. Implement `bring_pid_to_front` on
Windows (`EnumWindows` by PID → `SetForegroundWindow`) and Linux
(`_NET_ACTIVE_WINDOW`, or helper self-raise on a signal file).

**Evidence.** grep `autostart|LaunchAgent|SMAppService|CurrentVersion\\Run`
over `src/`, `packaging/` → none; `packaging/` tree;
`docs/crash-reporting.md:55,437`; `src/main.rs:178-226,391-434,937-951,959-987`;
`docs/user-flows.md:553`; `docs/test-plan.md:360`.

### P4 — Lock-screen and idle detection everywhere, and fix the macOS probe (M; S per OS) — [CI-Engineering#285](https://github.com/companionintelligence/CI-Engineering/issues/285)

**Why.** `is_capture_blocked` returns `false` unconditionally off macOS
(`src/worker.rs:1132-1135`), so Windows and Linux lock screens are captured and
shipped; the macOS path shells out to `pgrep -x ScreenSaverEngine` per tick,
which misses a plain loginwindow lock. `idle_ms` is only real on macOS
(`vizier/src/observer/macos.rs:58-60,261`) and the worker never reads it, so
screenshots keep flowing on an idle machine on every OS. The test plan already
tells QA to "confirm and log" that locked Windows/Linux screens are captured
(`docs/test-plan.md:321,392`).

**What.** macOS: `CGSessionCopyCurrentDictionary()["CGSSessionScreenIsLocked"]`
(CoreGraphics already linked) instead of `pgrep`. Windows: `OpenInputDesktop`
failure / `WTSRegisterSessionNotification` for lock, `GetLastInputInfo` for
idle. Linux: `org.freedesktop.login1` `Session.LockedHint` or
`org.freedesktop.ScreenSaver.GetActive` over D-Bus, `XScreenSaverQueryInfo` /
`ext-idle-notify-v1` for idle. Add a "skip when idle" gate
(`idle_ms > interval`) that also pauses vizier, and make the "Paused while
locked" lane copy true.

**Evidence.** `src/worker.rs:983-995,1126-1147`;
`vizier/src/observer/macos.rs:58-60,261-268`;
`vizier/src/observer/common.rs:197,222`; `docs/test-plan.md:321,392`;
`docs/memory-integration.md:348-355`; `README.md:272`;
`locales/settings.yml:471-473`.

### P5 — Linux vizier beyond Hyprland: X11, wlroots, KDE, plus `idle_ms` (M) — [CI-Engineering#286](https://github.com/companionintelligence/CI-Engineering/issues/286)

**Why.** `linux.rs` enriches only when a Hyprland IPC socket exists
(`vizier/src/observer/linux.rs:35-67,139-160`); on GNOME, KDE, sway and X11 it
falls back to the synthetic baseline. README.md:96 says Linux needs an X11
session while README.md:162 says enrichment is "Hyprland IPC" (a Wayland
compositor) — the documented supported session and the only enriched session
are mutually exclusive, and `docs/test-plan.md:393` wrongly rates Linux focus
as "real". `idle_ms` is never populated on Linux. The waker shells out to `ip`,
`lspci`, `who`, `id`, `uname` (`linux.rs:392-460`), absent in the Docker image.

**What.** Layer collectors by session type: `x11rb` for X11
(`_NET_ACTIVE_WINDOW` / `_NET_CLIENT_LIST` / `_NET_WM_NAME` / `_NET_WM_PID` +
XScreenSaver idle); a `wlr-foreign-toplevel-management` client for
sway/Hyprland/niri/COSMIC/Wayfire (has `activated` state;
`ext-foreign-toplevel-list` does not —
<https://wayland.app/protocols/wlr-foreign-toplevel-management-unstable-v1>,
<https://wayland.app/protocols/ext-foreign-toplevel-list-v1>); KWin scripting
over D-Bus for Plasma (<https://develop.kde.org/docs/plasma/kwin/api/>); a
documented GNOME-extension D-Bus fallback (the ActivityWatch approach,
<https://github.com/ActivityWatch/activitywatch/issues/1218>). Idle via
`ext-idle-notify-v1`, else `org.gnome.Mutter.IdleMonitor` /
`org.freedesktop.ScreenSaver`. Replace `who` with `org.freedesktop.login1`.
Correct `docs/test-plan.md:393` to "Hyprland only".

**Evidence.** `vizier/src/observer/linux.rs:35-67,139-160,392-460`;
`vizier/src/observer/macos.rs:58-60,261`; `README.md:96,162`;
`docs/test-plan.md:393`; `vizier/src/observer/linux.md:4,18`.

### P6 — Make Settings, onboarding and copy honest off macOS; surface BlueBubbles (S; mostly this branch) — [CI-Engineering#287](https://github.com/companionintelligence/CI-Engineering/issues/287)

**Why.** Settings shows "4 lanes" and an armable "Record device activity"
toggle on every OS, with a card that says both "— Not required on this OS" and
"This lane only works on a Mac" (`src/settings_ui.rs:2064-2109,2255-2323`;
`locales/settings.yml:34-36,558-560`). "this Mac" ×4, "Paused while locked" and
"lives in your menu bar" render on Linux/Windows (`settings.yml:139-165,471-473`;
`onboarding.yml:291-293`). The onboarding chip row iterates `Step::ALL` so a
phantom "Permission" chip is drawn (`src/onboarding_ui.rs:466`; the comment at
`:42` cites a nonexistent `Step::visible`). The iMessage lane silently idles
off macOS with no UI anywhere (`src/imessage/mod.rs:135-142`), while
BlueBubbles — the one messaging lane that works on Windows/Linux — is
config-file-only and undiscoverable (no Settings card). README, architecture,
user-flows and test-plan never mention iMessage or BlueBubbles.

**What.** Add `Step::visible()` filtered by `permissions_relevant()` and use it
in the header. Off macOS hide the knowledge `lane_row` and Device-activity
section, set the sidebar count to 3, and add a distinct "Unavailable on this
OS" `PermissionStatus`. Add a small per-OS string helper and split the ~10
macOS strings ("menu bar" → "system tray"/"tray", "this Mac" → "this
computer"). Gate iMessage `is_runnable` with `cfg!(macos)` and use `BaseDirs`
instead of `HOME`. Add a BlueBubbles Settings card. Document
iMessage/BlueBubbles/knowledge as macOS-only (BlueBubbles as the cross-platform
iMessage path) and add the README "Known limitations" row the comparison doc
already asks for.

**Evidence.** `src/settings_ui.rs:2064-2109,2255-2323`;
`src/onboarding_ui.rs:41-42,73-96,466`; `src/permissions.rs:53-58,222-227`;
`locales/settings.yml:20-22,34-36,139-165,471-473,558-560`;
`locales/onboarding.yml:291-293,318-319`; `locales/upload.yml:41-51`;
`src/imessage/mod.rs:56,135-142`; `src/bluebubbles/mod.rs:1-6`;
`src/config.rs:225-247`; `docs/test-plan.md:286`; `docs/user-flows.md:753`;
`docs/screenpipe-comparison.md:232`.

### P7 — Wayland screenshots via a persistent ScreenCast portal session (L; S interim gate) — [CI-Engineering#288](https://github.com/companionintelligence/CI-Engineering/issues/288)

**Why.** `capture.rs` calls `screenshots::Screen::all()` / `capture()` with no
Wayland awareness (`src/capture.rs:32-44`). The vendored crate's Wayland chain
is `org.gnome.Shell.Screenshot` (restricted on modern GNOME) → portal
`Screenshot` with `interactive=false` (consent dialog or "Screenshot failed or
canceled", 60 s wait) → wlr-screencopy (only wlroots compositors)
(`screenshots-0.8.10/src/linux/wayland_screenshot.rs:118-200`). On GNOME/KDE
Wayland — the two most common desktops — the lane fails or prompts every 300 s
tick, and the error is log-only. README.md:96 says "Needs an X11 session". The
crate is deprecated upstream in favour of `xcap`
(<https://github.com/nashaofu/xcap>), and its macOS path uses the deprecated
CGDisplay API that Sequoia nags about. xdg-desktop-portal ≥ 1.14 gates
non-interactive `Screenshot` behind the Access portal
(<https://github.com/flatpak/xdg-desktop-portal/pull/851>,
<https://github.com/flatpak/xdg-desktop-portal/issues/649>).

**What.** *S interim:* detect `XDG_SESSION_TYPE=wayland` without a wlroots
compositor and disable the screenshot lane with a Settings/onboarding
"Environment check" notice instead of failing silently. *L:* open one
ScreenCast portal session via `ashpd` (`CreateSession` → `SelectSources`
`persist_mode=2` + `restore_token` → `Start` → PipeWire fd —
<https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html>),
persist the restore token in config next to FDA state, grab a frame per tick;
migrate `screenshots` → `xcap` for X11/Windows/macOS. Align `.deb` depends
with the Dockerfile runtime closure (`libxkbcommon`, `libgl1`/`libegl1`) and
add `zenity | kdialog` + `xdg-utils`.

**Evidence.** `src/capture.rs:32-44`; `src/worker.rs:160-162`;
`screenshots-0.8.10/src/linux/mod.rs:12-28`, `wayland_screenshot.rs:118-200`;
`README.md:96`; `Cargo.toml:24,40,129-135`; `Dockerfile:37-42`;
`docs/screenpipe-comparison.md` (portal design notes).

### P8 — Capture the actual primary display, downscale frames, then multi-monitor (S; L for multi-monitor) — [CI-Engineering#289](https://github.com/companionintelligence/CI-Engineering/issues/289)

**Why.** `capture_primary` takes `screens.into_iter().next()`, not the display
with `is_primary` (`src/capture.rs:32-34`), so on multi-monitor Windows/Linux it
may capture a secondary while Settings promises "Your main display"
(`locales/settings.yml:471-473`). Frames are full native pixels with no
downscale — a 5K frame is ~59 MB RGBA and is cloned three times per tick
(`src/worker.rs:1006-1048`) then PNG-encoded and uploaded at full size.
Multi-monitor is the comparison doc's own "Adopt" item #2, and `capture.rs` has
0 automated tests (`docs/test-plan.md:87`).

**What.** *S (this branch, first half):* pick
`screens.iter().find(|s| s.display_info.is_primary)` with fallback to first.
*S:* downscale to a bounded long edge (e.g. 1920) before diff/save/upload;
revisit the 2 GB cap afterwards. *L follow-on:* per-display capture with
per-display `last_capture` baselines, tag uploads with display index/bounds
from vizier's `DisplayInfo`, and a Settings picker. Consider the
focus-change/idle trigger from `docs/screenpipe-comparison.md` §4.1 once
`idle_ms` is real everywhere.

**Evidence.** `src/capture.rs:32-44`; `src/worker.rs:1006-1048`;
`locales/settings.yml:471-473`; `display-info-0.4.8` `is_primary`
(`macos.rs:34`, `windows.rs:64`); `docs/screenpipe-comparison.md:60,118,129-166,229-231`;
`docs/test-plan.md:87,121-126`.

### P9 — Windows hygiene batch: console window, locale, tray glyph, rename inference, globs, scheme (S; partly this branch) — [CI-Engineering#290](https://github.com/companionintelligence/CI-Engineering/issues/290)

**Why.** No `#![windows_subsystem = "windows"]` anywhere, so the tray, every
helper and every `cmd /C start` `open_url` spawn a console window
(`src/main.rs:75-77,1001-1017`). Locale reads only `LC_ALL`/`LC_MESSAGES`/`LANG`
off macOS, which Windows GUI processes lack, so "Match my system" is always
English (`src/locale.rs:121-149`). The tray glyph is a solid `#000000`
silhouette and `with_icon_as_template` is macOS-only, so it is a black blob on
the default dark Windows taskbar and dark GNOME/KDE panels
(`src/svg_icon.rs:37-50`; `assets/tray-mark.svg:15,19`;
`tray-icon-0.11.3/src/lib.rs:259-268`). `file_id` is `None` on Windows so
renames become delete+create (`src/watcher/fingerprint.rs:79-96`); ignore globs
are `/`-only with no backslash normalisation and no Windows credential-store
patterns (`src/watcher/ignore.rs:23-80`). `companioncapture://` is registered
only in `Info.plist` (`docs/test-plan.md:398`).

**What.** *This branch:* `#![cfg_attr(all(windows, not(debug_assertions)),
windows_subsystem = "windows")]`, `CREATE_NO_WINDOW` on the `cmd` spawn,
`sys-locale` first with the macOS `AppleLocale` and then the POSIX env as
fallbacks, theme-adaptive glyph off macOS. *Later:*
route `env_logger` to a file in the data dir on Windows; restore rename
inference with `GetFileInformationByHandleEx(FileIdInfo)` / `file_index()`;
normalise backslashes before globset matching, add a test for
`C:\Users\x\AppData\...`, add Windows sensitive globs plus Linux ignore
defaults (`~/.cache`, `~/.mozilla`, `~/.config/google-chrome`, Trash); register
the scheme in NSIS (`HKCU\Software\Classes`) and in a packaged `.desktop`; call
`SetWindowDisplayAffinity(WDA_MONITOR)` on helper windows so Recall cannot
snapshot tokens
(<https://learn.microsoft.com/en-us/windows/apps/develop/windows-integration/recall/>).

**Evidence.** `src/main.rs:75-77,1001-1017`; `src/locale.rs:121-149`;
`src/svg_icon.rs:37-50`; `assets/tray-mark.svg:15,19`;
`tray-icon-0.11.3/src/lib.rs:259-268`; `src/watcher/fingerprint.rs:79-96`;
`src/watcher/notify_glue.rs:100-108`; `src/watcher/ignore.rs:23-80`;
`packaging/macos/Info.plist:8-26`; `docs/test-plan.md:349,397,398`.

### P10 — Release gate parity, per-OS smoke, vizier tests in CI, artifact and docs hygiene (M; mostly S items) — [CI-Engineering#291](https://github.com/companionintelligence/CI-Engineering/issues/291)

**Why.** The release preflight `cargo test` runs on `ubuntu-latest` only
(`.github/workflows/release.yml:142-183`) and per-OS build jobs run no tests
(`:374-387`), so macOS-only and Windows-only branches are never tested at
release time and the x86_64 macOS binary is cross-compiled and never executed.
No step launches any produced artifact (only `codesign --verify`, `:389-429`)
and there is no `--version`. vizier's 13 tests never run anywhere (no
`[workspace]`; `ci.yml:42-51`). No clippy/fmt/cargo-deny/dependabot/MSRV. The
Release leaks deb internals (`control.tar.gz` / `data.tar.gz`) via a recursive
`find dist -name '*.tar.gz'` (`:498-500,545-547`); the pacman format yields a
PKGBUILD source tarball so README.md:96's `pacman -U` cannot work and R2 gets no
pacman entry. Docs promise build-provenance attestations that do not exist
(`RELEASING.md:129`; `README.md:209`), say no CDN feed exists while `upload-r2`
runs unconditionally (`:671-698`) and fails after publish if secrets are
missing; Dockerfile still says `ci-applet` / `LicenseRef-CI-Commercial`.
alpha.6 failed in Build installers on both macOS jobs (`gh run list`).

**What.** *This branch:* vizier tests on all three runners; release artifact
glob restricted so deb internals stop leaking. *Later:* reuse `ci.yml` via
`workflow_call` as a 3-OS preflight; add `--version` and a display-free
`--self-test`; per-OS smoke (dmg mount / `dpkg -x` / `--appimage-extract` /
NSIS `/S`) running `--version` + `--check-server` against a loopback mock, and
`docker run … --check-server` in `packages.yml`. The Windows leg must account
for the release exe being GUI-subsystem (`windows_subsystem = "windows"`):
`cmd` and PowerShell do not wait for it when it is invoked directly, so the
prompt returns before the report and `$LASTEXITCODE` is never set from it. Write
the step as

```pwsh
$out = & "$env:ProgramFiles\Companion Capture\companion-capture.exe" --check-server 2>&1 | Out-String
if ($out -notmatch 'Companion Capture → server connection check') { throw "no report:`n$out" }
if ($out -match '✗') { throw "probe failed:`n$out" }
```

(capturing the output makes PowerShell wait; `cmd /c start /wait "" …` or
`Start-Process … -Wait -NoNewWindow -PassThru` are equivalent) and assert on
the captured stdout, not the exit code: `server_check::report` returns `Ok(())`
even when every probe prints `✗`, so `--check-server` exits 0 unless config
load or the runtime build fails. While writing that step, verify on the runner
that stdout redirected to a pipe survives `AttachConsole` when the parent
shell does have a console; if not, only call `AttachConsole` when
`GetStdHandle(STD_OUTPUT_HANDLE)` is NULL/invalid (see `attach_parent_console`
in `src/main.rs`). Add `[workspace] members =
[".", "vizier"]`, clippy `-D warnings`, fmt, cargo-deny, dependabot,
`rust-toolchain.toml`. Drop pacman or `makepkg` to a real `.pkg.tar.zst` and
fix README/upload script. Add `actions/attest-build-provenance` or delete the
claim; guard `upload-r2` on secret presence; fix asset names, image name,
Dockerfile labels/license, dead links. Add `packaging/macos/entitlements.plist`
and sign dev builds with the Developer ID cert so TCC grants survive alpha
upgrades. Replace the private `__rustls-ring` reqwest feature with the public
`rustls` feature + ring provider.

**Evidence.** `.github/workflows/release.yml:142-183,214-228,318-320,374-451,498-500,545-557,559-602,671-698`;
`.github/workflows/ci.yml:22-51`; `.github/workflows/packages.yml:55-64`;
`Cargo.toml` (no `[workspace]`; `:21-23` `__rustls-ring`);
`docs/test-plan.md:98,107-113,400-401`; `docs/RELEASING.md:29-34,129`;
`docs/DESKTOP-RELEASE-SECRETS.md:66,90,126,134`; `README.md:82-105,209`;
`Dockerfile:6-11,49-52`; `gh release view v0.2.0-alpha.7` assets;
`gh run list` alpha.6.

### P11 — Auto-update and package-manager channels from the feed that already exists (M) — [CI-Engineering#292](https://github.com/companionintelligence/CI-Engineering/issues/292)

**Why.** There is no updater code in `src/`, yet `release.yml:559-602`
generates `latest.json` and `upload-desktop-r2.sh` publishes `manifest.json` +
`latest.json` to `dl.ci.computer/applet/` on every production release — nothing
consumes it. `homebrew-tap/Casks` and `scoop-bucket` contain only
`companion-hub`. CI-Hub already has `updater.rs` and
`publish-package-managers.yml` to copy. Without update or autostart, every user
on every OS runs a stale build after the first install.

**What.** Port CI-Hub's `updater.rs` polling pattern against
`https://dl.ci.computer/applet/latest.json` (frozen `ci-applet` wire values)
with a "check for updates" tray item and notification. Add a
`publish-package-managers` job that generates a `companion-capture` Cask and
Scoop manifest from `latest.json`; consider winget and AUR later. Pairs with
P3 (autostart) and P10 (smoke) so an auto-installed build is verified before it
ships.

**Evidence.** `.github/workflows/release.yml:559-602,671-698`;
`.github/scripts/upload-desktop-r2.sh`;
`/Users/liam/devel/ci/homebrew-tap/Casks/companion-hub.rb`;
`/Users/liam/devel/ci/scoop-bucket/companion-hub.json`;
`/Users/liam/devel/ci/CI-Hub/.github/workflows/publish-package-managers.yml`;
`/Users/liam/devel/ci/CI-Hub/packages/desktop/src-tauri/src/updater.rs`;
grep `updater|self-update` over `src/` → none.

### P12 — Cross-platform app-usage sessionizer plus native media/notification/Bluetooth/lock enrichments (M) — [CI-Engineering#293](https://github.com/companionintelligence/CI-Engineering/issues/293)

**Why.** The knowledge lane's wire kinds (`knowledge_app_usage`,
`_media_usage`, `_notification`, `_bluetooth`, `_display`, daily digests) are
platform-neutral strings (`src/knowledge/facts.rs:311-479,575-577`) but only
macOS has a source. No OS store to read exists elsewhere (§6): Windows Timeline
stopped recording app interactions on Win11, Recall is a sealed enclave with no
read API, Zeitgeist is dormant since 2021, Phone Link's `phone.db` is a private
30-day Android-only schema. vizier already samples focus/idle every tick
(`src/worker.rs:163-173`), so the missing piece is a sessionizer. Direct 1:1
analogues exist for the other kinds: GSMTC (`windows::Media::Control`) and
MPRIS for media; `UserNotificationListener` and D-Bus `BecomeMonitor` on
`org.freedesktop.Notifications` for notifications (app name only, matching the
macOS policy); `DeviceWatcher` and BlueZ `Device1.Connected` for Bluetooth;
logind `LockedHint` for display spans.

**What.** After P1/P5 land: build one in-process focus sessionizer fed by
`Observation.focus` + `idle_ms` that emits `knowledge_app_usage` spans and
`knowledge_daily_app_usage` digests on Windows/Linux, reusing `facts.rs` floors
and suppression (extend `SENSITIVE_BUNDLE_IDS` matching to exe names /
`app_id`s); keep knowledgeC as the macOS source since it covers time Capture was
not running. Add a shared `zbus` / `windows`-crate scaffold for media,
notification, Bluetooth (hash the device id as `src/knowledge/reader.rs:93`
does) and lock/unlock. Use the browser extension for web domains; an opt-in
Chrome/Firefox history importer (domains only, same suffix list) is optional.
Explicitly decline Timeline, Recall, Zeitgeist, Phone Link and Flatpak in docs.

**Evidence.** `src/knowledge/facts.rs:52-76,111-149,311-479,575-577`;
`src/knowledge/mod.rs:13-17,112-136`; `src/knowledge/reader.rs:93`;
`src/worker.rs:163-173`; `vizier/src/observer/common.rs:169-232`;
`CI-Engineering/architecture/platform-review.md:26,43`; external sources as
cited in §6.

## 5. Quick wins (this branch)

Exactly what `clau/cross-platform-quick-wins` does. Every item is S, `cfg`-gated
or platform-neutral, leaves macOS behaviour unchanged unless noted (row 2 notes
a macOS *and* a Linux locale-precedence change), and carries a test that runs
on macOS, Ubuntu and Windows (or is `cfg`-gated per OS).

| # | Change | Where | Feeds |
|---|---|---|---|
| 1 | **Windows console subsystem + `CREATE_NO_WINDOW`.** `#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]` at the top of `main.rs`; `.creation_flags(CREATE_NO_WINDOW)` on the `cmd /C start` spawn so `open_url` stops flashing a console. Debug builds keep the console so `env_logger` output stays visible. `attach_parent_console` re-attaches stdout for the `--check-*` probes, but a GUI-subsystem exe is not *waited on* by `cmd`/PowerShell when invoked directly — see §4 P10 for how the smoke step must call it. | `src/main.rs` (`#![cfg_attr(… windows_subsystem)]`, `attach_parent_console`, `CREATE_NO_WINDOW` in `open_url`) | P9 |
| 2 | **`sys-locale` as the first locale source.** `sys_locale::get_locales()` (`GetUserPreferredUILanguages` on Windows, `CFLocaleCopyPreferredLanguages` on macOS, `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG` on Linux, normalised to BCP-47) is consulted first, then the macOS `defaults read -g AppleLocale` subprocess is **kept as a second-tier fallback**, then the POSIX `LC_ALL`/`LC_MESSAGES`/`LANG` env, so "Match my system" resolves on Windows. `first_usable_locale` skips `C`/`POSIX` at every tier. **Behaviour changes:** on macOS the UI-language list now precedes the Region-format `AppleLocale` (Region = Spain + English UI used to give Spanish, now English); on Linux `LANGUAGE` (gettext's colon-separated list, never read before) now outranks `LC_ALL`/`LANG` (`LANGUAGE=en_US:en LANG=es_ES.UTF-8` used to give Spanish, now English). Both are pinned by tests. | `src/locale.rs` (`system_language`, `first_usable_locale`); `Cargo.toml` (`sys-locale`) | P9 |
| 3 | **Theme-adaptive tray glyph off macOS.** The rasterised `#000000` silhouette is recoloured (alpha preserved, RGB written as *straight* alpha because tray-icon PNG-encodes it on Linux and hands it to `CreateIcon` on Windows) to **white unless the system explicitly reports Light** — `Unspecified` and read errors also resolve to white, since GNOME's top bar is dark in its light theme. On Windows `HKCU\…\Themes\Personalize\SystemUsesLightTheme` (the taskbar's own switch) is read via `winreg` first and `dark-light` is consulted only if the `Personalize` key cannot be opened; on Linux `dark-light` asks the XDG portal. macOS keeps `with_icon_as_template(true)` and is untouched. | `src/svg_icon.rs` (`tray_glyph_colour`, `windows_taskbar_glyph_colour`, `recolour_rgba`, `GlyphColour`); `src/main.rs` (`with_icon_as_template`); `Cargo.toml` (`dark-light` under `cfg(not(macos))`, `winreg` under `cfg(windows)`) | P9 |
| 4 | **`is_primary` display pick.** `screens.iter().find(|s| s.display_info.is_primary)` with fallback to the first screen, replacing `into_iter().next()`. Platform-neutral; on a single-display Mac the result is identical. | `src/capture.rs:32-34` | P8 |
| 5 | **vizier tests in CI on all three runners.** `cargo test --locked --manifest-path vizier/Cargo.toml` added to `ci.yml` so the per-OS observer code compiles and its tests run on macOS, Ubuntu and Windows for the first time; `Swatinem/rust-cache` is given `workspaces: [., vizier]` so `vizier/target` is cached rather than rebuilt from scratch each run. | `.github/workflows/ci.yml` (`Cache cargo build`, `Test (vizier)`) | P10 |
| 6 | **Honest baseline observer + skip empty vizier posts.** `BaselineObserver::snapshot` stops fabricating the `local-shell` focus window from `$SHELL` (emits `focus: None`, `windows: []`). The worker skips the vizier POST when an observation has no focus, no windows, no fs_events **and** no net_connections (`vizier_observation_is_empty`) *and* the last state the server holds is also empty (`vizier_should_skip_send`) — so Windows and non-Hyprland Linux stop paying server rows for an empty timeline, while the first empty tick after a non-empty one is still shipped to clear the previous fs_events/windows and advance `ts`. A one-time `warn` then debug lines; a skipped-tick counter (`vizier_empty_ticks_skipped`) is exposed for the P2 health row. macOS observations carry focus/windows whenever at least one layer-0 window is on screen; with every window closed **and** no established TCP rows (offline / `lsof` empty) the skip can fire there too, after the all-closed transition has been sent. | `vizier/src/observer/common.rs` (`BaselineObserver::snapshot`); `src/worker.rs` (`vizier_observation_is_empty`, `vizier_should_skip_send`, `capture_vizier`) | P2 |
| 7 | **Phantom onboarding chip.** `Step::visible()` filters `Step::ALL` by `permissions_relevant()` and the header chip row uses it; the stale comment at `:42` that cited the nonexistent helper is deleted. On macOS all steps are visible, so nothing changes. | `src/onboarding_ui.rs:41-42,466` | P6 |
| 8 | **Per-OS copy + hidden knowledge lane off macOS.** Runtime-branched variants for "this Mac" → "this computer" (`settings.backlog.*`), "Paused while locked" dropped from the Screens lane detail off macOS, and "menu bar" → "system tray" (Windows) / "tray" (Linux) in `onboarding.finish.where_body`, `onboarding.nav.sign_in_later`, `settings.language.help`, `settings.connection.oidc.help`, `upload.auth.signed_out`, `upload.action.upload_needs_auth`. Off macOS the knowledge `lane_row` and the Device-activity section are hidden and the sidebar count reads 3 lanes. Locale catalogue test (`tests/locale_catalogue.rs`) covers the new keys in both languages. | `locales/settings.yml:20-22,34-36,139-165,389-391,471-473`; `locales/onboarding.yml:291-293,318-319`; `locales/upload.yml:41-51`; `src/settings_ui.rs:2064-2109,2255-2323` | P6 |
| 9 | **Release artifact glob.** The "Collect release artifacts" `find` is restricted to `find dist -maxdepth 1` (cargo-packager writes every installer straight into `dist/` and keeps its scratch tree under `dist/.cargo-packager/`) so `control.tar.gz` / `data.tar.gz` (the `.deb`'s internal members) no longer land in the GitHub Release. | `.github/workflows/release.yml:498-507` | P10 |

Plus this document, a one-line pointer in `docs/plan/00-overview.md` §7, and a
"Known limitations" pointer in README.md next to the platform install notes.

Explicitly **not** in this branch (they are M or need a design decision):
the Windows observer (P1), per-lane health rows (P2), autostart /
single-instance (P3), lock/idle detection (P4), Linux observers (P5), the
BlueBubbles Settings card and iMessage `cfg` gate (P6 remainder), anything
Wayland (P7), frame downscaling (P8 second half), rename inference / globs /
scheme registration (P9 remainder), the pacman decision (open question 6).

## 6. Native data sources per OS

What the native-sources survey found when looking for a Windows or Linux
equivalent of `knowledgeC.db` (`06-knowledge.md`). Web-sourced claims carry
their URL; everything else is verified in code.

### 6.1 What exists and is usable

| Kind (`facts.rs`) | Windows | Linux |
|---|---|---|
| `knowledge_app_usage` | **No OS store.** Must sessionize in-process from `GetForegroundWindow` / `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` + `GetLastInputInfo` — the ActivityWatch / AppUsageTracker pattern (<https://github.com/dmsm-ven/AppUsageTracker>). | **No OS store.** Same sessionizer, fed by the P5 focus collectors. |
| `knowledge_media_usage` | `Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager` — `SourceAppUserModelId`, `PlaybackStatus`, `SessionsChanged`; `windows` crate feature `Media_Control` (<https://microsoft.github.io/windows-docs-rs/doc/windows/Media/Control/struct.GlobalSystemMediaTransportControlsSessionManager.html>). No prompt, no package identity (*inferred*). | MPRIS `org.mpris.MediaPlayer2.*` on the session bus — `PlaybackStatus`, `Metadata`, `PropertiesChanged`; Firefox/Chromium expose it by default. `dbus` 0.9.9 is already in `Cargo.lock` via `screenshots`; prefer `zbus`. |
| `knowledge_notification` | `Windows.UI.Notifications.Management.UserNotificationListener` — app name + toast text of *current* notifications, `RequestAccessAsync` prompt from a UI thread (<https://learn.microsoft.com/en-us/windows/apps/develop/notifications/app-notifications/notification-listener>). Works unpackaged per a Microsoft Q&A thread (<https://learn.microsoft.com/en-us/answers/questions/1128583/c-how-to-use-notification-listener>) but must be spike-tested — open question 5. `wpndatabase.db` is plain SQLite but rows are deleted on dismissal (<https://artefacts.help/windows_wpndatabase_db.html>). | `org.freedesktop.DBus.Monitoring.BecomeMonitor` on the session bus with a match rule for `org.freedesktop.Notifications.Notify` — how `dbus-monitor` works, no privilege needed (<https://discourse.gnome.org/t/use-gdbus-to-eavesdrop-notifications-using-becomemonitor/22952>). Needs a dedicated connection. Ship `app_name` only, never `summary`/`body`. |
| `knowledge_bluetooth` | `Windows.Devices.Enumeration.DeviceWatcher` with `BluetoothDevice::GetDeviceSelectorFromConnectionStatus(Connected)`; `windows` features `Devices_Bluetooth`, `Devices_Enumeration` (*inferred*, not spike-tested). | BlueZ `org.bluez` on the system bus: `Device1.Connected` / `Name` / `Alias`, `PropertiesChanged` gives exact span edges; reads need no polkit (<https://www.bluez.org/bluez-5-api-introduction-and-porting-guide/>). |
| `knowledge_display` (lock / unlock spans) | `WTSRegisterSessionNotification` / `OpenInputDesktop` (P4). | `org.freedesktop.login1` `Session.LockedHint` / `IdleHint` / `Active` on the system bus, readable without polkit (*inferred*, well-known interface). Also replaces the `who` shell-out at `vizier/src/observer/linux.rs:126-133`. |
| `knowledge_web_usage` (domains) | No system stream. Browser history SQLite (Chrome/Edge `History`, locked while running; Firefox `places.sqlite`) is possible but the **browser extension** is the platform's chosen surface (`CI-Engineering/architecture/platform-review.md:26,43`). | Same. |
| iMessage-lane analogue | **BlueBubbles** — already cross-platform (`src/bluebubbles/mod.rs:1-6`, no `cfg` gates, pure HTTP poller). | Same. |

Both hash the Bluetooth device id the way the macOS reader does
(`src/knowledge/reader.rs:93`) and reuse the `SENSITIVE_BUNDLE_IDS` /
`SENSITIVE_WEB_DOMAINS` policy (`src/knowledge/facts.rs:111-149`).

### 6.2 What is dead or sealed — declined

| Source | Verdict | Why |
|---|---|---|
| **Windows Timeline** (`ActivitiesCache.db`) | Decline | Microsoft removed Timeline from Windows 11 22H2/23H2 in Jan 2024; the DB now holds "minimal entries, primarily related to system activities like Wi-Fi" (<https://www.cyberengage.org/post/forensic-differences-between-windows-10-and-windows-11>, <https://securelist.com/forensic-artifacts-in-windows-11/117680/>, <https://artifacts-kb.readthedocs.io/en/latest/sources/windows/ActivitiesCacheDatabase.html>). It was only ever populated by apps calling `UserActivity` (write-only API), never a foreground log. At most an opt-in one-shot import on Windows 10. |
| **Windows Recall / Click to Do** | Decline as a source | Snapshots + OCR live in `ukg.db`, SQLite SEE AES-256-GCM encrypted with keys in a VBS enclave, Windows-Hello-gated, "unreadable to third-party apps" (<https://en.wikipedia.org/wiki/Windows_Recall>, <https://medium.com/@boutnaru/the-windows-forensic-journey-ukg-db-windows-recall-e2413841ade8>). Developer docs expose only outbound integration (<https://learn.microsoft.com/en-us/windows/apps/develop/windows-integration/recall/>). Copilot+ hardware only. One actionable item: `SetWindowDisplayAffinity(WDA_MONITOR)` on our own helper windows (P9). |
| **Windows SRUM** (`SRUDB.dat`) | Decline | ESE DB, locked, admin-only, hourly per-app CPU/network — not sessions. |
| **Phone Link** (`phone.db`) | Decline | Undocumented private schema, ~30-day window, linked Android phones only, a 2025-26 schema migration broke on some machines (<https://learn.microsoft.com/en-us/answers/questions/5584681/where-are-all-text-messages-stored-on-my-pc-using>, <https://learn.microsoft.com/en-us/answers/questions/5859660/title-phone-link-inbox-version-stuck-with-stale-me>). BlueBubbles is the iMessage path; a phone-side agent is the durable SMS source. |
| **GNOME Zeitgeist** | Decline | Last release 1.0.x (2021), not installed by default on current GNOME/Ubuntu/Fedora, logs app-donated file/URI events rather than focus (<https://en.wikipedia.org/wiki/Zeitgeist_(free_software)>, <https://discourse.gnome.org/t/zeitgeist-gnome-activity-journal-1-0/4521>). At most an opt-in import if the file exists. |
| **KDE `kactivitymanagerd` resources DB** | Low priority | Records documents/URLs opened by KActivities-aware apps — closer to the file tracker than to `/app/usage`; Plasma 6.5–6.7 corruption/bloat bugs (<https://www.mail-archive.com/kde-bugs-dist@kde.org/msg1209371.html>). KDE-only. |
| **Flatpak / Snap** | Non-goal | Every source above needs `--socket=session-bus` / `--socket=system-bus` / `--socket=wayland` holes that defeat the sandbox (<https://discourse.flathub.org/t/session-bus-access-for-forwarding-desktop-notifications/3697>). Native `.deb` / AppImage stay primary (open question 11). |

### 6.3 Chosen sessionizer approach

Windows and Linux get `knowledge_app_usage` from **one in-process focus
sessionizer**, not from an OS store:

- Input: vizier's `Observation.focus` (app id, title) + `idle_ms`, already
  sampled every capture tick (`src/worker.rs:163-173`) once P1/P5 provide real
  values.
- Sessionize: a span opens on a focus change, closes on the next change or when
  `idle_ms` exceeds the tick interval; idle time is excluded, as knowledgeC's
  `/app/usage` effectively does.
- Emit: `knowledge_app_usage` spans through the same `facts.rs` builders,
  floors (`min_duration_secs`, 30 s on 1:1 rows) and non-disableable
  suppression, with the bundle-id matcher extended to exe names / `app_id`s
  (`1password.exe`, `org.keepassxc.KeePassXC`); `knowledge_daily_app_usage` /
  `knowledge_daily_summary` digests through the existing D+1 gates
  (`06-knowledge.md` §6.2), with "covered" meaning "Capture was running".
- Identity: `source_id`s scoped per machine exactly as the macOS digests are.
- macOS keeps knowledgeC as its source — it covers time Capture was not
  running, which the sessionizer never can. The sessionizer is not enabled on
  macOS.
- Open question 4 (client- vs server-side derivation) can flip this to a
  server-side job over vizier focus events; the wire shape is the same either
  way.

The media / notification / Bluetooth / lock kinds are 1:1 enrichments on a
shared `zbus` (Linux) and `windows`-crate (Windows) scaffold, each S once the
scaffold exists.

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| A quick win regresses macOS (the platform every user is on today). | Every change is `cfg(not(target_os = "macos"))`-gated or provably identical on macOS (single display → same `is_primary` pick), with two noted exceptions: the locale source order (§5 row 2: `CFLocaleCopyPreferredLanguages` now precedes `AppleLocale`) and the empty-post skip, which on macOS can fire once every window is closed *and* `lsof` reports no established TCP rows (offline) — and only after the all-closed transition has been shipped (`vizier_should_skip_send`). Tests run on all three CI runners; `cargo clippy --all-targets -- -D warnings` clean. |
| Skipping empty vizier posts hides a *real* outage (observer broken on macOS). | The skip logs a one-time `warn` (later ticks at debug, gated by the `VIZIER_EMPTY_WARNED` `AtomicBool`) and increments `VIZIER_EMPTY_SKIPPED`, readable via `vizier_empty_ticks_skipped()`; P2's health row still has to surface it. The `fs_events`-only and net-only cases still post, and the first empty tick after a non-empty one is always sent. |
| `windows_subsystem = "windows"` makes Windows logs vanish (stderr closed). | Gated on `not(debug_assertions)` so `cargo run` keeps the console; P9 routes `env_logger` to a file in the data dir before the next Windows release. |
| Theme-adaptive glyph picks the wrong colour on panels that ignore the system theme (custom GNOME shells). | Only Windows/Linux change; a wrong pick is the status quo (black) at worst. P9 may ship a light/dark pair later. |
| `sys-locale` returns a tag the `rust-i18n` catalogue does not have (`es-419`, `en-GB`). | Existing `Language::from_tag` fallback chain (primary subtag → FALLBACK) is unchanged; test pins `es-MX` → `es` and unknown → English. |
| Per-OS copy variants drift between `en` and `es`. | `tests/locale_catalogue.rs` asserts every new key exists in both catalogues. |
| The Windows observer (P1) reads titles of elevated windows and fails silently under UIPI. | Documented caveat; `focus` falls back to exe name with no title; health row shows it. |
| Wayland portal restore token invalidated (compositor upgrade) → silent failure again. | P7 treats a failed `Start` as a readiness state ("re-grant screen sharing") rendered by P2, not a log line. |
| `UserNotificationListener` needs package identity on some builds. | Spike first (open question 5); notification kind stays macOS/Linux-only until proven. |
| Autostart (P3) turns a silent stub into a silent stub that runs at every login. | P3 is sequenced after P2's health surfacing so a lane that cannot run says so from the tray. |
| pacman artifact is unusable but README tells users to `pacman -U` it. | Open question 6; quick-win 9 only fixes the deb leak, and the README "Known limitations" pointer sends Arch users here until the format is dropped or made real. |
| Docs promise attestations / a CDN feed that do not match the workflow. | P10 reconciles; until then this document is the honest source. |

## 8. Open questions

1. Which Linux desktops actually matter for the next 6-12 months (GNOME/KDE
   Wayland vs X11 vs wlroots)? This decides whether the L-sized
   ScreenCast-portal backend (P7) is worth building now or whether "X11-only
   plus an explicit Wayland notice" is acceptable for a release or two.
2. Is Windows a real target audience today (any dl.ci.computer/NSIS install
   numbers)? P1 assumes yes; if Windows is aspirational, P2's honesty work plus
   a README "Known limitations" row could ship first and P1 could slip behind
   Linux (P5).
3. Should the vizier lane stay `enabled: true` by default off macOS while it
   ships empty/synthetic observations, or should the default flip per OS until
   a real observer lands? Each empty tick costs a server row.
4. For Windows/Linux app-usage (P12): should the sessionizer run client-side
   and emit `knowledge_app_usage`, or should CI-Server derive app-usage from
   vizier focus events server-side? CI-Engineering
   `data-mappings/ci-capture.md` item 2 (structured activity fields opaque in
   jsonb) is still open and affects the answer.
5. Windows notification capture via `UserNotificationListener` may require
   package identity (MSIX / sparse package) even though a Microsoft Q&A thread
   shows it working unpackaged. Is a sparse-package identity acceptable for
   the NSIS-installed exe, or should the notification kind be
   macOS/Linux-only?
6. Should the pacman/Arch channel be dropped, or made real via `makepkg` in
   CI? Today it produces an unusable PKGBUILD tarball and README tells users to
   `pacman -U` it.
7. Can the Developer ID cert be made available on the dev environment so alpha
   builds keep a stable signing identity (TCC grants for Screen Recording /
   FDA currently reset on every ad-hoc build per `release.yml:318-320`)?
8. Build-provenance attestation is promised in `RELEASING.md:129` and
   `README.md:209` but not implemented — implement it (`permissions:
   attestations: write`) or delete the claim?
9. Is the Screenpipe-style focus-change/idle-driven capture trigger
   (`docs/screenpipe-comparison.md` §4.1, marked Adopt #1) wanted now that
   idle/focus signals will exist on all OSes after P1/P4/P5, and does it
   change the 300 s interval contract with CI-Server?
10. Are the `legacy-minio` / aws-sdk features still needed? They are the
    reason for the private `__rustls-ring` reqwest feature and are never built
    in CI; deleting them simplifies the Windows MSVC story.
11. Flatpak/Snap: confirm "no Flatpak" as a deliberate non-goal (every Linux
    source in P5/P12 needs session/system-bus and Wayland socket holes that
    defeat the sandbox), so it can be documented alongside the existing
    non-goals.

## 9. Exit criteria

For this branch (§5):

- [ ] A release-profile Windows build launches the tray with no console
      window; `open_url` from Settings opens the browser without a flash;
      debug builds still print `env_logger` output.
- [ ] On a Spanish-language Windows install with `Language::System`, Settings
      renders in Spanish; a unit test pins `sys-locale` → `Language` mapping
      including `es-MX` → `es` and an unknown tag → English.
- [ ] Tray glyph is white-on-transparent on a dark Windows taskbar and dark
      GNOME/KDE panel, black on a light one; macOS still passes
      `with_icon_as_template(true)` and the rasteriser test for the macOS size
      is byte-identical to before.
- [ ] `capture_primary` on a two-monitor host where the primary is not
      enumerated first captures the primary; a unit test over a fake
      `DisplayInfo` list pins the pick and the fallback.
- [ ] `ci.yml` runs vizier's tests (16 on this branch; 13 before it) on macOS,
      Ubuntu and Windows and they pass, from a cached `vizier/target`.
- [ ] `BaselineObserver::snapshot` with `$SHELL` set returns `focus: None`,
      `windows: []` (test on all three OSes); the worker makes zero vizier
      POSTs against the mock server for an empty observation and logs the
      warn exactly once; an observation with only `fs_events` still posts.
- [ ] Off macOS the onboarding header shows Welcome / Sign in / Done and no
      "Permission" chip; `Step::visible()` test is cfg-split per OS.
- [ ] Off macOS Settings › Capture lists three lanes, no Device-activity
      section, no "this Mac", no "Paused while locked", and "system tray" /
      "tray" instead of "menu bar" in the six copy sites; on macOS every
      string is unchanged. `cargo test --test locale_catalogue` passes with
      the new keys in `en` and `es`.
- [ ] The next GitHub Release contains no `control.tar.gz` / `data.tar.gz`
      (dry-run: `find` with `-maxdepth 1` over a packager scratch tree in a
      shell test lists only installers).
- [ ] `README.md` has a "Known limitations" pointer next to the platform
      install notes and `docs/plan/00-overview.md` §7 lists this document.
- [ ] `cargo fmt --check` on the touched files, `cargo clippy --all-targets
      -- -D warnings`, and `cargo test --locked` pass on macOS, Ubuntu and
      Windows.

For the phase (§4), each priority closes its CI-Engineering issue (linked from its §4 heading) when:

- [ ] P1: a Windows host reports `focus`, `windows`, `displays`, `idle_ms` and
      net connections; the cfg-gated shape test passes on `windows-latest`.
- [ ] P2: Settings shows a per-lane last-success / last-error row on every OS
      and the tray status line reads "Last capture failed" after an injected
      screenshot error, with no TCC involvement.
- [ ] P3: after a reboot on each OS the tray is running; a second launch
      exits with a message box; Upload window cannot be opened twice.
- [ ] P4: a locked screen on each OS produces zero screenshots and zero vizier
      posts; idle > interval skips the tick; the macOS probe no longer spawns
      `pgrep`.
- [ ] P5: GNOME (extension), KDE, sway and X11 sessions each report real
      focus; `docs/test-plan.md:393` is corrected.
- [ ] P6 remainder: BlueBubbles has a Settings card; iMessage cannot be armed
      off macOS; README/architecture/test-plan document the three macOS-only
      lanes.
- [ ] P7: GNOME and KDE Wayland capture with one consent dialog per install;
      the restore token survives restart.
- [ ] P8: uploads carry display index/bounds; a 5K frame is ≤ 1920 px on the
      long edge before diff.
- [ ] P9 remainder: a Windows rename emits one `FileRenamed`;
      `is_ignored(r"C:\Users\x\AppData\...")` is true;
      `companioncapture://` resolves on Windows and Linux.
- [ ] P10: the release gate runs tests on all three OSes and launches each
      artifact with `--version`; docs match the workflow.
- [ ] P11: an installed build offers "Check for updates" and the Cask / Scoop
      manifests install the latest release.
- [ ] P12: a Windows or Linux host emits `knowledge_app_usage` spans and a
      `knowledge_daily_app_usage` digest whose totals match a hand-tallied
      focus log; media / notification / Bluetooth spans appear with app name
      only.
