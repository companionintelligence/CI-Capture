# File-watcher MVP — working checklist

This branch implements [docs/plan/02-file-watcher.md](docs/plan/02-file-watcher.md). Skeleton modules under `src/watcher/`, `src/queue/`, and `src/sync/` are committed but **not** wired into `src/main.rs` yet — that happens in the first sub-PR so reviewers see the wiring as its own change.

Read the plan doc first. This file is a checklist tied to its sections.

---

## Working checklist

### Deps

- [ ] Add to `Cargo.toml`:
  - `notify = "8"`              (cross-platform watcher)
  - `rusqlite = { version = "0.32", features = ["bundled"] }`  (local queue)
  - `globset = "0.4"`           (ignore patterns)
  - `sha2 = "0.10"`             (content hashing)
- [ ] Add `mod watcher; mod queue; mod sync;` to `src/main.rs` (or, post-Phase-1, to `src/lib.rs`).
- [ ] `cargo build` clean.

### `src/watcher/`

- [ ] `notify_glue.rs` — translate `notify::Event` into our `NormalizedEvent`. Handles platform-specific event ordering on macOS / Linux / Windows.
- [ ] `normalizer.rs` — debounce window (default 750 ms, max 5 s); rename inference via inode/FileID equality; settle-detection wrapper.
- [ ] `settle.rs` — poll size + mtime every 500 ms; settled when two consecutive reads match; cap 10 s.
- [ ] `fingerprint.rs` — stream-hash via `sha2::Sha256`; record size, mtime, inode; skip if `(path, size, mtime, inode)` matches a cached fingerprint.
- [ ] `ignore.rs` — `globset::GlobSet` over the default ignore list (plan §7) plus user overrides.
- [ ] `types.rs` — `NormalizedEvent`, `FileFingerprint`, `EventType`.
- [ ] Top-level `mod.rs::start_watcher(cfg) -> WatcherHandle`.

### `src/queue/`

- [ ] `schema.sql` — the four tables from plan §8.
- [ ] `rusqlite_impl.rs` — `Queue::open(path)`, `enqueue(event)`, `take_batch(n)`, `mark_acked(event_ids)`, `bump_attempt(event_id, next_at)`, dead-letter API.
- [ ] WAL mode; sync = NORMAL.
- [ ] Migration step: bootstrap schema on first open via `PRAGMA user_version`.

### `src/sync/`

- [ ] `http.rs` — `reqwest::Client` builder reusing `server_common::server_origin`; bearer-authed.
- [ ] `batching.rs` — adaptive batch size (start 16, grow up to 256 on success, halve on 5xx); per-endpoint backoff 1s→2s→4s capped at 5 min.
- [ ] `mod.rs::run_sync(queue, cfg, shutdown)` loop.

### Server coordinate

- [ ] Open a tracking issue in CI-Server for endpoints:
  - `POST /devices/register`, `POST /devices/heartbeat`
  - `POST /history/file-events/batch`
- [ ] Each applet sub-PR is blocked on the corresponding CI-Server endpoint shipping first.

### Settings UI

- [ ] Watched-folder picker (multi-select).
- [ ] Ignore-list editor with the defaults from plan §7 pre-populated, individually toggleable.
- [ ] Per-folder policy dropdown: `full` / `metadata-only` / `ignore`.

### Worker integration

- [ ] Spawn the watcher task from `worker.rs::WorkerHandle::spawn` (or its Tauri equivalent post-Phase-1).
- [ ] Tray Pause/Resume halts the watcher as well as the screenshot+vizier loops.
- [ ] Tray status string reflects events-in-queue: "Sync OK" / "12 pending" / "Paused".

### Tests

- [ ] Unit tests for `normalizer` (atomic-save sequence, rename inference, storm collapse).
- [ ] Integration test using a temp dir + real `notify` watcher: create/modify/rename/delete each fire the right `NormalizedEvent`.
- [ ] Queue roundtrip test: enqueue → take_batch → mark_acked.
- [ ] Backoff test: simulate 5xx, observe exponential delays.

### Verification (matches plan §10 exit criteria)

- [ ] Watched folder + edit → event lands on CI-Server in < 5 s.
- [ ] Rename produces one `file.renamed` event, not delete+create.
- [ ] Pull network, edit files, plug in — queue drains.
- [ ] `node_modules` edits ignored.
- [ ] Idle CPU < 1%.
- [ ] No panics on permission-denied paths.

---

## Branch hygiene

- Sub-PRs into this branch should each land one of the `src/watcher/`, `src/queue/`, `src/sync/` modules with tests, not all at once.
- Wire-up PR (declare `mod watcher;` etc. in `main.rs` and spawn from `WorkerHandle::spawn`) is the last sub-PR; do not merge it before the server endpoints exist.
