# Companion Capture — File-tracking plan (rebuilt)

> Consolidates the Phase 0–4 branch series into one shipped feature and
> records the **real** server contract (not the originally-assumed one).
> Supersedes the per-phase PRs #6–#25 and the Tauri-shell PRs #2/#7.

## 1. What the applet is now

A macOS menu-bar applet that grows your **Companion Memory** through four
capture lanes that share one worker runtime, config, identity, and CI-Server
connection:

| Lane | What it captures | Ships via |
|------|------------------|-----------|
| **Screenshots** (existing) | Diff-gated primary-display frames | `POST /api/files/upload` |
| **vizier** (existing) | System/activity snapshots + diffs | `POST /api/v1/events` |
| **File tracker** (this work) | Edits to watched folders → fingerprint + Markdown | `POST /api/v1/events` |
| **Device activity** (macOS, off by default) | `knowledgeC.db` app/media/notification/Bluetooth rows + daily digests | `POST /api/v1/events` |

The file-tracking lane is the answer to *"track the important files I edit and
let my AI remember them."*

## 2. File-tracking pipeline (shipped)

```text
notify (FSEvents/inotify/RDCW)
  → notify_glue        raw OS events → RawObservation, best-effort inode
  → normalizer         debounce storms, collapse atomic saves, infer renames
  → settle             wait until the file stops being written
  → fingerprint        sha256 (streamed) + size + mtime + inode
  → ignore             two-layer rules: defaults + non-disableable sensitive list
  → SQLite queue       durable; survives crash + network outage (at-least-once)
  → extract            changed file → Markdown (text family always; rich opt-in)
  → sync               adaptive batch size + exponential backoff
  → CI-Server          POST /api/v1/events (kind="file_event")
```

Module map (all under `src/`):

- `watcher/` — `mod.rs` (driver + `start_watcher`), `notify_glue`, `normalizer`,
  `settle`, `fingerprint`, `ignore`, `types`.
- `queue/` — `rusqlite_impl` (durable event queue), `schema.sql`.
- `sync/` — `batching` (`BatchController`: adaptive size + backoff).
- `extractors/` — `mod` (router), `traits`, `text` (txt/md/json/csv/tsv **+ code/config**),
  and opt-in `html`/`eml`/`epub` behind `--features rich-extractors`.
- `conversion/` — `chunk` (section-aware, token-budgeted), `markdown` (assembly helpers).
- `file_tracker.rs` — the lane runtime: owns the queue, drains the watcher,
  extracts, and syncs.
- `server_check.rs` — `--check-server` connectivity self-test.

## 3. The key revision: ride the unified capture ingest channel

The original Phase 2/3 plans assumed CI-Server would grow a family of
history-specific REST endpoints (`/history/file-events/batch`,
`/history/documents/upsert`, `/history/blobs/upload`, …). CI-Server now exposes
the mainline unified capture ingest endpoint at `POST /api/v1/events`, guarded
by `JwtOrApiKeyAuthGuard`.

So the file-tracking lane writes capture events there instead of inventing
endpoints the server can't serve:

- `kind = "file_event"`
- `source_system = "ci-applet"` — **frozen wire identifier**, deliberately not
  renamed with the product so existing CI-Server rows keep matching
- `source_id = NormalizedEvent.event_id`
- `payload = { event: NormalizedEvent, content: { markdown, section_count, extractor, content_truncated } }`

This keeps the applet working against mainline CI-Server. JWT/OIDC credentials
are sent as bearer auth; CI-Server API keys are sent as `x-api-key`.

## 4. CI-Server connection — verified state (2026-06-21)

| Check | Result |
|-------|--------|
| Transport + GraphQL reachable | ✅ applet probes the configured CI-Server gateway via `--check-server` |
| File upload endpoint | ✅ applet posts screenshots/documents to `POST /api/files/upload` |
| Unified capture ingest | ✅ applet posts activity/file events to `POST /api/v1/events` |
| API-key auth | ✅ applet sends 64-character CI-Server API keys as `x-api-key` |

**To go end-to-end:** configure `server.portal_url`, either sign in via the
configured OIDC issuer or paste a CI-Server API key, then run
`cargo run -- --check-server` against that origin.

Self-test usage:

```bash
cargo run -- --check-server                       # against configured portal
cargo run -- --check-server --portal https://your-ci-server.example.com
```

The applet probes both `/api/...` and bare API routes for the configured
CI-Server URL.

## 5. Privacy posture (shipped)

- Capture is **off until the user adds a watched folder** — nothing is watched
  by default.
- Two-layer ignore: disable-able defaults (`node_modules`, `.git`, build dirs,
  caches, browser profiles, VM images) + a **non-disableable** sensitive list
  (`.ssh`, keychains, `.aws/credentials`, `.gnupg`, password vaults), enforced
  *before* hashing.
- Size cap (default 100 MB) gates hashing of huge files.
- `send_content=false` ships metadata only (path/size/hash, no body).
- `max_content_bytes` bounds embedded Markdown per event.

## 6. What's next (not in this change)

- **Settings UI**: folder picker + ignore-list editor + per-folder
  metadata-only toggle (config fields exist; UI wiring is follow-up).
- **Tray status**: "Tracking N folders · queue depth M".
- **Rich/heavy extractors**: PDF (pdfium + OCR), DOCX, PPTX, XLSX, images
  (EXIF/OCR), audio (Whisper) — see `03-conversion.md` §4. Stubs are in place.
- **First-class server endpoint**: richer typed capture schemas on top of
  `/api/v1/events` when the server is ready.
- **Privacy/intelligence**: see `04-polish.md` and `05-intelligence.md`.
- **Device activity (macOS)**: a fourth, off-by-default lane that crawls the
  Mac's own Knowledge store (`knowledgeC.db`) for app-usage, media,
  notification and Bluetooth spans plus daily per-app digests — see
  `06-knowledge.md`.

## 7. Design records

The original per-phase design docs are preserved alongside this overview:
`02-file-watcher.md`, `03-conversion.md`, `04-polish.md`, `05-intelligence.md`,
and `06-knowledge.md` (the device-activity lane, with the empirical findings
from the macOS Knowledge store that shaped it). They remain the detailed
rationale; this overview is the source of truth for what actually shipped.
