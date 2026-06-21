# CI-Applet — File-tracking plan (rebuilt)

> Consolidates the Phase 0–4 branch series into one shipped feature and
> records the **real** server contract (not the originally-assumed one).
> Supersedes the per-phase PRs #6–#25 and the Tauri-shell PRs #2/#7.

## 1. What the applet is now

A macOS menu-bar applet that grows your **Companion Memory** through three
capture lanes that share one worker runtime, config, identity, and CI-Server
connection:

| Lane | What it captures | Ships via |
|------|------------------|-----------|
| **Screenshots** (existing) | Diff-gated primary-display frames | `POST /api/files/upload` |
| **vizier** (existing) | System/activity snapshots + diffs | `ingestUserSnapshot` GraphQL |
| **File tracker** (this work) | Edits to watched folders → fingerprint + Markdown | `ingestUserSnapshot` GraphQL |

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
  → CI-Server          ingestUserSnapshot(snapshot_kind="file_event")
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

## 3. The key revision: ride the existing ingest channel

The original Phase 2/3 plans assumed CI-Server would grow a family of new REST
endpoints (`/history/file-events/batch`, `/history/documents/upsert`,
`/history/blobs/upload`, …). **Those endpoints do not exist** — not on
mainline CI-Server, not on any feature branch.

What *does* exist (on the `CI-Server-wt-applet-pairing` branch) is the
`ingestUserSnapshot` GraphQL mutation the vizier lane already uses. Its
`IngestUserSnapshotInput` carries an arbitrary `payload: JSON!` plus
`snapshotKind`, `deviceId`, `sessionId`, `observedAt`, `fsEventCount`,
`focusApp`/`focusTitle`/`cwd`.

So the file-tracking lane **reuses that channel** instead of inventing
endpoints the server can't serve:

- `snapshot_kind = "file_event"`
- `payload = { event: NormalizedEvent, content: { markdown, section_count, extractor, content_truncated } }`

This keeps the applet working against the same server it already talks to.
When a first-class `/history/file-events` API eventually lands, only
`server_ingest::ingest_file_event` changes — the whole pipeline above is
untouched.

## 4. CI-Server connection — verified state (2026-06-21)

| Check | Result |
|-------|--------|
| Transport + GraphQL reachable | ✅ **live** — applet reached the running CI-Server gateway at `http://localhost:8642/api/graphql` (HTTP 200, valid GraphQL) via `--check-server` |
| `IngestUserSnapshotInput` field contract | ✅ exact match with what the applet sends (verified against `CI-Server-wt-applet-pairing` schema) |
| `ingestUserSnapshot` / `startAppletPairing` on the **running** server | ⚠️ absent — the running gateway serves **mainline** CI-Server; these mutations exist only on `CI-Server-wt-applet-pairing` |

**To go end-to-end:** merge/deploy the `applet-pairing` GraphQL onto the
running CI-Server, then `cargo run -- --check-server` against that origin and
pair via the tray.

Self-test usage:

```bash
cargo run -- --check-server                       # against configured portal
cargo run -- --check-server --portal http://localhost:8642
```

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
- **First-class server endpoint**: a real `/history/file-events` so events
  don't have to ride `ingestUserSnapshot`.
- **Privacy/intelligence**: see `04-polish.md` and `05-intelligence.md`.

## 7. Design records

The original per-phase design docs are preserved alongside this overview:
`02-file-watcher.md`, `03-conversion.md`, `04-polish.md`, `05-intelligence.md`.
They remain the detailed rationale; this overview is the source of truth for
what actually shipped.
