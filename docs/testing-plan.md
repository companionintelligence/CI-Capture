# CI-Applet — On-Device Testing Plan

> Branch under test: `companion/agitated-mendel-1bc892` ([PR #5](https://github.com/companionintelligence/CI-Applet/pull/5)).
> This branch contains the full architecture/roadmap docs **plus** every implementation sub-PR merged into one buildable tree, plus the integration wire-up that exposes everything via CLI subcommands.
>
> If you only want to review the per-feature sub-PRs without an integration build, see [PRs #6–#25](https://github.com/companionintelligence/CI-Applet/pulls). Each is independently mergeable and has its own focused tests.

---

## 1. Prerequisites

| Item | Why | How to satisfy |
|---|---|---|
| Rust toolchain (edition 2024, ≥ 1.85 stable) | Crate uses 2024 edition | `rustup default stable && rustup update` |
| Sibling [`vizier`](https://github.com/companionintelligence) checkout at `../../vizier` | Path dependency in `Cargo.toml` | `git clone <vizier-repo> ../../vizier` (relative to repo root) |
| macOS Screen Recording permission | Existing screenshot loop | First launch will prompt; allow in System Settings → Privacy & Security |
| macOS Accessibility (only if vizier needs it) | vizier observations | Allow in System Settings → Privacy & Security |
| Network access | `cargo build` first time (crate downloads) | n/a |
| Network access to `https://memory.ci.localhost` | **Only** needed for the pairing + ingest tests in §6 | Skip §6 if you don't have CI-Server running |

Nothing else needs to be installed — every dep is pulled by Cargo.

---

## 2. Build verification

```bash
cargo build               # debug build, ~3 min cold
cargo build --release     # release binary at target/release/CI-Applet
```

Expected: clean build with at most `dead_code` / `unused_variables` warnings. **Any compile error is a real bug** — capture the full message and either open an issue or `cargo update -p <crate>` to a compatible version.

Likely first-time issues + their fixes:

| Symptom | Fix |
|---|---|
| `notify` API mismatch in `src/watcher/notify_glue.rs` | `cargo update -p notify --precise 8.0.0` (or whatever's latest in the 8.x series). The translator only depends on `EventKind::{Create, Modify(ModifyKind::{Data, Name, Metadata, Any}), Remove, Access}` shapes that have been stable since 6.x. |
| `mail-parser` API mismatch in `src/extractors/eml.rs` | Pin to `mail-parser = "=0.9.4"` or the matching minor. |
| `epub` API mismatch in `src/extractors/epub.rs` | Pin to `epub = "=2.1.4"` or the matching minor. |
| `rusqlite` link error on Linux | The crate is using the `bundled` feature so SQLite ships statically; should be fine. If it errors, install `libsqlite3-dev`. |

---

## 3. Unit tests

```bash
cargo test
```

Expected counts (per-module breakdown — counts must match the totals shown):

| Module | Tests | Coverage |
|---|---:|---|
| `watcher::ignore` | 13 | gitignore + sensitive-path layers, size cap |
| `watcher::fingerprint` | 8 | sha256 stream-hash + inode |
| `watcher::settle` | 6 | async polling, deleted/timeout/settled |
| `watcher::normalizer` | 12 | coalescing + rename inference + atomic save |
| `watcher::notify_glue` | 10 | Event→RawObservation translation |
| `queue::rusqlite_impl` | 10 | enqueue/take/ack/requeue, attempts_over, persistence |
| `sync::batching` | 9 | adaptive batch + exponential backoff |
| `extractors::text` | 15 | txt/md/json/csv/tsv |
| `extractors::html` | 8 | html2md + title extraction + entities |
| `extractors::eml` | 9 | RFC5322 + multipart + html-fallback |
| `extractors::epub` | 10 | helpers (strip_html, chapter_heading, slugs) |
| `extractors` (router) | 7 | classify() dispatch + case-insensitivity |
| `conversion::chunk` | 9 | section-aware chunking + overlap |
| `conversion::markdown` | 15 | escape, table, YAML frontmatter |
| **Total** | **~141** | |

Run a single module with `cargo test --lib watcher::ignore -- --nocapture`. Add `RUST_LOG=debug` for log output.

---

## 4. Extractor smoke tests

The `--extract` subcommand runs a single file through `classify() → Extractor::extract() → print` without touching the server, watcher, or tray. Best place to verify the extraction pipeline.

### 4.1 Plain text family

```bash
echo "hello world" > /tmp/sample.txt
./target/release/CI-Applet --extract /tmp/sample.txt
```

Expected stderr (diagnostics):
```
--- text-extractor/txt/0.1 (text/plain) ---
extraction_time_ms=<small>  ocr_used=false  transcription_used=false
--- markdown (32 bytes) ---
```
Expected stdout:
```
# sample

```
hello world
```
```

### 4.2 Markdown passthrough

```bash
cat > /tmp/sample.md <<'MD'
# A real heading

A paragraph with **bold** and *italic*.

- one
- two
MD
./target/release/CI-Applet --extract /tmp/sample.md
```

Stdout = the input verbatim (passthrough), with a one-line synthetic Section internally so the chunker has a boundary.

### 4.3 JSON pretty-print

```bash
echo '{"a":1,"b":[2,3,4]}' > /tmp/sample.json
./target/release/CI-Applet --extract /tmp/sample.json
```

Expect a fenced ```` ```json ```` block with pretty-printed indentation.

### 4.4 CSV → GFM table

```bash
cat > /tmp/sample.csv <<'CSV'
name,age,city
Alice,30,NYC
Bob,25,SF
Carol,28,LA
CSV
./target/release/CI-Applet --extract /tmp/sample.csv
```

Expect a `| name | age | city |` table with separator row + 3 data rows.

### 4.5 CSV sampling on large files

```bash
( echo "n,v"; for i in $(seq 1 250); do echo "$i,x"; done ) > /tmp/big.csv
./target/release/CI-Applet --extract /tmp/big.csv
```

Expect a `_250 rows total; showing first/last 50 each._` banner and a `| … | … |` separator row mid-table.

### 4.6 HTML → Markdown

```bash
cat > /tmp/sample.html <<'HTML'
<html>
<head><title>Test Page</title></head>
<body>
<h1>Heading</h1>
<p>This is <strong>bold</strong> and <em>italic</em>.</p>
<ul><li>one</li><li>two</li></ul>
</body>
</html>
HTML
./target/release/CI-Applet --extract /tmp/sample.html
```

Expect `# Test Page` (from `<title>`), then `html2md`'s conversion of the body — `**bold**`, `*italic*`, list items.

### 4.7 Email (.eml)

```bash
cat > /tmp/sample.eml <<'EML'
From: Alice <alice@example.com>
To: Bob <bob@example.com>
Subject: Lunch?
Date: Wed, 18 May 2026 12:00:00 -0700
Message-ID: <abc@example.com>

Hey Bob,
Free for lunch Friday?
-- Alice
EML
./target/release/CI-Applet --extract /tmp/sample.eml
```

Expect `# Lunch?` heading, body text. Stderr diagnostics show `eml-extractor/0.1`.

### 4.8 EPUB (any real ebook)

Drag a real `.epub` (Project Gutenberg has free ones) into `/tmp/`:

```bash
./target/release/CI-Applet --extract /tmp/yourbook.epub | head -50
```

Expect `# <book title>` then per-chapter `## ...` headings.

### 4.9 Unsupported format

```bash
./target/release/CI-Applet --extract /tmp/some.pdf 2>&1 | head -3
```

Expect: `Error: unsupported file type: /tmp/some.pdf — see --help for the supported set`. PDF/DOCX/PPTX/XLSX/image/audio are intentionally deferred (each needs its own session — see [docs/plan/03-conversion.md](plan/03-conversion.md) § 4 and the [phase-3-conversion CHECKLIST](https://github.com/companionintelligence/CI-Applet/blob/phase-3-conversion/CHECKLIST.md)).

### 4.10 metadata-only mode (programmatic only)

There's no CLI flag yet — to test, add an integration test that calls `extractor.extract(path, ExtractOptions { metadata_only: true, .. })` and asserts `doc.markdown.is_empty()` + `doc.diagnostics.warnings.contains("metadata_only")`. The per-extractor unit tests already cover this internally.

---

## 5. File-watcher smoke tests (dry-run)

The `--watch` subcommand starts the **dry-run** pipeline: notify_glue → normalizer → log decisions. No queue, no server, no fingerprint cost. Best place to verify the file-event lane on your OS.

### 5.1 First touch

```bash
mkdir -p /tmp/watch-test
RUST_LOG=info ./target/release/CI-Applet --watch /tmp/watch-test
```

In a second shell:

```bash
echo "hello" > /tmp/watch-test/a.txt          # Created
echo "world" >> /tmp/watch-test/a.txt         # Modified
mv /tmp/watch-test/a.txt /tmp/watch-test/b.md # Renamed (inode-paired)
rm /tmp/watch-test/b.md                       # Deleted
```

Expected (first-shell log, with ~750ms debounce delay between operation and emission):

```
INFO  CI_Applet::watcher > watching /tmp/watch-test (dry-run)
INFO  CI_Applet::main    > decision: Created { path: "/tmp/watch-test/a.txt" }
INFO  CI_Applet::main    > decision: Modified { path: "/tmp/watch-test/a.txt" }
INFO  CI_Applet::main    > decision: Renamed { old_path: "/tmp/watch-test/a.txt", new_path: "/tmp/watch-test/b.md" }
INFO  CI_Applet::main    > decision: Deleted { path: "/tmp/watch-test/b.md" }
```

Notes:
- The `Created` + `Modified` may collapse to a single `Created` if they fall within the same 750 ms debounce window (this is the desired "Created+Modified → Created" rule from [normalizer.rs](../src/watcher/normalizer.rs)).
- The rename is emitted **immediately**, not after debounce — that's the design.

### 5.2 Editor "atomic save" (the important test)

```bash
# In another shell, edit /tmp/watch-test/c.md in any editor that does
# atomic-save (vim, VS Code on macOS, JetBrains products).
```

Expected: **one** `decision: Modified { path: ".../c.md" }`, not a flurry of Created/Modified/Renamed on `.tmp` files. This validates the atomic-save rule (Created tmp + Renamed tmp→target collapses to Modified target).

### 5.3 Ignore-list verification

```bash
mkdir -p /tmp/watch-test/node_modules
echo "should not appear" > /tmp/watch-test/node_modules/index.js
```

Expected: **nothing** logged. The default `IgnoreRules` blocks `node_modules`. Same for `.git`, `target`, browser profiles, etc. ([list](../src/watcher/ignore.rs)).

### 5.4 Sensitive-path block

```bash
echo "no" > /tmp/watch-test/.ssh-test/id_rsa  # won't actually catch — paths
# use a contrived equivalent inside watched dir:
mkdir -p "/tmp/watch-test/Library/Keychains"
echo "secret" > "/tmp/watch-test/Library/Keychains/login.keychain-db"
```

Expected: **nothing** logged. Sensitive patterns are non-disable-able.

### 5.5 Storm tolerance (manual)

```bash
for i in $(seq 1 1000); do echo "$i" > "/tmp/watch-test/burst-$i.txt"; done
```

Observe:
- All 1000 `Created` events should arrive eventually.
- Memory should not blow up. The normalizer collapses bursts on the same path; different paths flow through independently.
- Storm-summary mode (collapse to `directory.modified`) is **not** wired in yet (see [plan §9](plan/02-file-watcher.md)).

### 5.6 Stop the watcher

Ctrl-C in the watcher shell. Expect clean exit (no panics, no hung threads).

---

## 6. Pairing + tray smoke tests (CI-Server required)

**Skip this section if you don't have CI-Server + CI-Portal running locally.**

```bash
./target/release/CI-Applet
```

Expect:

| Step | Expected |
|---|---|
| Tray icon appears | macOS menu bar (template, white silhouette). Linux: tray applet. Windows: notification area. |
| Click tray → menu opens | Pause/Resume, Sign in via Portal, Settings, Upload Documents, Show Config Path, Quit |
| Click "Sign in via Portal" | Browser opens at `https://memory.ci.localhost/...` |
| Confirm pairing in browser | Tray app fires "Linked this computer to ..." dialog |
| `~/Library/Application Support/CI-Applet/config.json` (macOS path) | Now contains `applet_auth_token` |
| Capture loop | Every ≥ 30 s a screenshot is captured + uploaded |
| Pause | Capture pauses |

Re-run with `RUST_LOG=debug` to see vision-LLM calls, ingest mutations, etc.

---

## 7. End-to-end watch + extract (manual cross-cut)

There's no single command yet that ties `--watch` to `--extract` — they're separate testing entry points. To simulate the production pipeline:

```bash
# Terminal 1
RUST_LOG=info ./target/release/CI-Applet --watch ~/Documents

# Terminal 2 — when you see a "decision: Modified ..." for a file you care about:
./target/release/CI-Applet --extract <that-path>
```

What you'd see in production (post Phase 2/3 wire-up of `run_full_pipeline`):

```
notify → RawObservation → Normalizer → NormalizedDecision → fingerprint → NormalizedEvent → SQLite queue → sync engine → CI-Server
                                                                                              ↓
                                                                                          conversion runner
                                                                                              ↓
                                                                              Extractor → ExtractedDocument → POST /history/documents
```

The conversion runner (`src/conversion/mod.rs` consumer side) is not yet implemented — it's the missing piece to fully glue (5) and (4) together. Tracked in [docs/plan/03-conversion.md](plan/03-conversion.md) § 6.

---

## 8. Performance sanity check

Idle with the tray running, no files changing:

| Measurement | How | Target |
|---|---|---|
| CPU % | macOS Activity Monitor / `htop` | < 1 % steady-state |
| RAM | Same | < 200 MB |
| Disk I/O | `fs_usage -w -f filesys $(pgrep CI-Applet)` (macOS) | minimal at idle |

With `--watch ~/Documents` active and no file changes:

| Measurement | Target |
|---|---|
| CPU % | < 2 % steady-state |
| RAM | < 250 MB |

With `--watch ~/Documents` while doing normal editing:

| Measurement | Target |
|---|---|
| CPU % during edit | < 5 % spike |

If you see anything dramatically worse, capture a `cargo flamegraph` or `samply record` and open an issue.

---

## 9. What's NOT yet testable (and why)

| Feature | Why not | Tracked |
|---|---|---|
| DOCX / PPTX / XLSX extraction | Need `docx-rs` / `calamine` integration sub-PRs | [phase-3 CHECKLIST](https://github.com/companionintelligence/CI-Applet/blob/phase-3-conversion/CHECKLIST.md) |
| PDF extraction | Needs `pdfium-render` + Pdfium binaries deployed per-OS | Same |
| Image OCR | Needs Tesseract data files bundled | Same |
| Audio transcription | Needs Whisper sidecar (separate binary, GBs of model data) | Same |
| ZIP recursive extraction | Builds on DOCX/PPTX/XLSX | Same |
| Full file-event sync to CI-Server | Needs `POST /api/history/file-events/batch` endpoint on CI-Server | [docs/plan/02-file-watcher.md § 9](plan/02-file-watcher.md) |
| Conversion runner end-to-end | Needs the worker-pool + crash-isolation glue | [docs/plan/03-conversion.md § 6](plan/03-conversion.md) |
| Tauri shell | Multi-day rewrite; needs npm + signed-cert setup | [docs/plan/01-tauri-shell.md](plan/01-tauri-shell.md) |
| First-run wizard / Settings tabs (Phase 4) | Gated on Tauri | [docs/plan/04-polish.md](plan/04-polish.md) |
| Token rotation / signed URLs / device revocation | Gated on CI-Server endpoint work | Same |

---

## 10. Troubleshooting

### `cargo build` fails on vizier

```
error: failed to load source for dependency `vizier`
```

→ `vizier` is a path dep at `../../vizier`. Clone the sibling repo there. If your layout is different, edit `Cargo.toml`'s `vizier = { path = "..." }` to match.

### `--watch` exits immediately

The default `IgnoreRules` may be eating every event your test exercises. Set `RUST_LOG=debug` and re-run; check whether `notify_glue` is filtering the events.

### Tray icon doesn't appear on Linux

`tray-icon` needs `libappindicator3` on most distros. `sudo apt install libappindicator3-1 gir1.2-appindicator3-0.1`.

### `--extract` on .html prints empty / mangled output

`html2md` has known limitations on heavily-styled pages. The `html-extractor` tests assert structural markers only ([extractors/html.rs](../src/extractors/html.rs)); if a real page renders poorly, open an issue with the source URL or a stripped-down repro.

### `notify` error: "couldn't create watcher"

Hit ulimits on Linux (`inotify_init` returns `EMFILE`). Bump `fs.inotify.max_user_watches`:

```bash
sudo sysctl -w fs.inotify.max_user_watches=524288
```

### Pairing browser doesn't open

The applet shells out to `open` (macOS), `xdg-open` (Linux), or `cmd /c start` (Windows). If your environment doesn't have these, copy the URL from the log (`RUST_LOG=info`) and open it manually.

---

## 11. Reporting findings

When something doesn't work as expected, please capture:

1. `cargo --version && rustc --version`
2. OS + version
3. The full `RUST_LOG=debug ./target/release/CI-Applet ...` output to a file
4. The exact command and any input files (if shareable)
5. Either:
   - Reply on the relevant sub-PR (#11–#25 depending on what failed), or
   - Open a new issue with these details and reference [PR #5](https://github.com/companionintelligence/CI-Applet/pull/5)

---

## 12. Test-pass shorthand

Once you've run §2 + §3 + §4 + §5 successfully:

> Verified on **\<macOS-version / Linux-distro / Windows-version\>** at commit `<sha>`:
> - `cargo build --release` clean
> - `cargo test` — 141/141 pass
> - `--extract` works for txt / md / json / csv / html / eml / epub
> - `--watch` reports Created / Modified / Renamed / Deleted with debounce + rename inference
> - Idle CPU < 1 %, RAM < 200 MB
