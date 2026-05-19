# On-Device Test Audit — Results

> Branch: `companion/agitated-mendel-1bc892`
> Commit at audit end: `bdd44f9` (plus one warnings-cleanup commit on top)
> Machine: macOS 25.4.0 (Darwin Kernel 25.4.0 arm64, MacBook Pro)
> Toolchain: stable Rust, edition 2024
>
> This document captures the results of running every test in [docs/testing-plan.md](testing-plan.md) on the audit machine. It documents what passed, what broke, what was fixed during the audit, and what platform limitations are now known.

---

## TL;DR

| Result | Count |
|---|---:|
| `cargo build --release` | ✅ Clean (warnings only, all expected scaffolding) |
| `cargo test` | ✅ 145 / 145 |
| Extractor smoke tests (txt/md/json/csv/html/eml/epub) | ✅ All pass |
| Watcher dry-run smoke tests | ✅ Pass with documented macOS FSEvents caveats |
| Bugs found + fixed during audit | **4** (1 real ignore-list leak + 1 sensitive-list leak + epub API mismatch + 3 of my own bad test assertions) |

The branch is in a state where you can verify everything in [docs/testing-plan.md](testing-plan.md) yourself; this document is the audit log.

---

## 1. Blocker resolved: `vizier` sibling

The repo declares `vizier = { path = "../../vizier" }` but the sibling crate isn't available on this machine, on GitHub under [companionintelligence/](https://github.com/companionintelligence), or anywhere else searchable.

**Workaround landed for the audit:** the stub `vizier` crate that lived in commit `3785620` (an unmerged copilot branch in this repo's history) has been written to [`.claude/vizier/`](../.claude/vizier/). It provides the exact API surface `src/worker.rs` imports:

- `vizier::observation::{Observation, FocusWindow, TerminalContext, FileSystemEvent}`
- `vizier::diff::{create_diff_envelope, DiffEnvelope}`
- `vizier::observer::{Observer, ObserverConfig, create_observer}`

The stub `Observer::snapshot()` returns a synthetic observation (platform name, cwd, file-watch-path-existence) — useful only for proving the worker pipeline compiles and runs, not for real machine-state capture.

**For real use:** replace `.claude/vizier/` with the actual `vizier` crate, or point `Cargo.toml` at wherever the real crate lives.

The stub is NOT committed to the branch (it lives outside the worktree at `.claude/vizier/`), so it doesn't pollute the PR.

---

## 2. Bugs found + fixed during audit

### 2.1 `src/extractors/epub.rs` — wrong API target

The PR-time assumption that `epub::doc::EpubDoc::mdata("title")` returns `Option<String>` was wrong. The 2.1.5 crate returns `Option<&MetadataItem>`, and `set_current_page(n)` returns `bool` (not `Result`). All three errors were caught by the first `cargo check` after the vizier stub.

| Site | Before | After |
|---|---|---|
| Title extract | `.mdata("title").filter(\|s\| !s.trim().is_empty()).unwrap_or_else(...)` | `.mdata("title").map(\|m\| m.value.trim().to_string()).filter(\|s\| !s.is_empty()).unwrap_or_else(...)` |
| Per-key metadata loop | `if let Some(v) = doc.mdata(epub_key) { let v = v.trim(); ... }` | `if let Some(item) = doc.mdata(epub_key) { let v = item.value.trim(); ... }` |
| Page walk | `if let Err(e) = doc.set_current_page(i) { ... }` | `if !doc.set_current_page(i) { warnings.push(...); continue; }` |

Also flipped from the deprecated `set_current_page` / `get_num_pages` to `set_current_chapter` / `get_num_chapters` (the API was renamed in 2.x).

Real EPUB extraction now works — `target/release/CI-Applet --extract /path/to/book.epub` on a Project-Gutenberg-sourced *Metamorphosis* produces 120 KB of Markdown with one section per chapter.

### 2.2 `src/watcher/ignore.rs` — directory-leak in ignore list (the real one)

Discovered by running `--watch` against a folder with `mkdir node_modules` inside. The glob `**/node_modules/**` correctly filters paths *inside* `node_modules` but does **not** match the `node_modules` directory itself. So `mkdir` events leaked through as `Created { path: ".../node_modules" }` even though every file inside was correctly dropped.

**Severity:** privacy-relevant. The same class of bug existed in `SENSITIVE_PATTERNS` — a bare `mkdir .ssh` would have produced a Created event despite us advertising `.ssh` as non-disable-able sensitive.

**Fix:** every directory-shaped ignore pattern now appears twice — `**/name` (bare, matches the directory itself) and `**/name/**` (contents). Two regression tests in `watcher::ignore::tests`:

```rust
fn bare_directories_are_ignored_not_just_contents()
fn bare_sensitive_directories_are_blocked()
```

**Live re-verification:** after the fix, `mkdir node_modules .git Library/Keychains .ssh` followed by file creation inside each shows **only** the unrelated `note.md` and the parent `Library` directory in the decisions log. The bare `node_modules` / `.git` / `Keychains` / `.ssh` paths are correctly suppressed.

### 2.3 Three of my own unit-test assertions were wrong

| Test | Why it was wrong |
|---|---|
| `conversion::chunk::overlap_carries_tail_of_previous_chunk` | When *both* paragraphs are larger than `target_tokens`, both hit `hard_split`, so `chunks[1]` is the second hard-split of paragraph 1 (more ALPHA), not the start of paragraph 2 (BRAVO). The assertion was checking the wrong chunk index. Rewrote to find the first chunk containing BRAVO and verify it carries the ALPHA overlap. |
| `extractors::eml::parses_plain_email` | `mail-parser` strips the angle brackets from `Message-ID:` headers — assertion checked for `"<abc@example.com>"` literally. Changed to substring match on `"abc@example.com"`. |
| `watcher::notify_glue::rename_with_one_path_falls_back_to_modify` | Single-path `Rename(Both)` actually emits a `Modified` via the catch-all arm of `event_to_observations`. The assertion expected `obs.is_empty()`. Updated to assert `obs.len() == 1` + variant is `Modified`, matching the documented behaviour. |

All three were test-side errors — the implementation logic was correct.

### 2.4 `Cargo.toml` package name + dev-deps merge collision

During the multi-branch merge into the integration branch, `Cargo.toml` accumulated four duplicated `[dev-dependencies]` blocks and several Phase-3 dep additions in conflicting locations. Resolved manually; final file consolidates phase-2 and phase-3 deps into one `[dependencies]` block.

---

## 3. Known platform limitations (documented, not bugs)

### 3.1 macOS FSEvents → no inode info → no rename pairing

The normalizer's rename inference relies on inode equality between a `Deleted(path_a, inode=X)` and a subsequent `Created(path_b, inode=X)`. macOS FSEvents does not expose inode information through `notify`'s `RecommendedWatcher`, so:

- Editor "atomic save" works as designed when the OS fires `Renamed { old, new }` directly (the watcher emits `Modified(target)` correctly).
- A plain `mv a b` on a watched directory often fires as `Modify(Any)` events instead of `Rename(Both)`, and the inferred decision is one or two `Modified` events rather than a single `Renamed`.

This is a `notify-rs` / macOS FSEvents limitation, not a CI-Applet bug. Linux (inotify) and Windows (ReadDirectoryChangesW) expose enough information for the pairing to work as designed.

Workaround if needed: a future sub-PR could use `kqueue` directly on macOS to get richer event info, at the cost of a much larger watcher implementation.

### 3.2 macOS FSEvents → noisy `Modify(Any)` for non-modifications

Removing a file from a watched directory sometimes appears in our normalized decisions as `Modified` rather than `Deleted`. Same root cause as above — `notify-rs` is forwarding FSEvents's coarse "something changed at this path" signal as `EventKind::Modify(ModifyKind::Any)` which hits our catch-all arm.

For the file-event lane this means: timeline events on macOS will be **complete** (every change produces *some* decision) but the per-decision type will be less precise than on Linux/Windows.

### 3.3 macOS FSEvents → latency

FSEvents batches events; on this audit machine we observed delivery latency of up to ~20 s between a filesystem operation and the corresponding decision in the watcher log. The events arrive eventually and in order; this only affects "feels real-time" UX.

### 3.4 HTML extractor renders dual heading from `<title>` + `<h1>`

For a page with both `<title>Page</title>` and `<h1>Page</h1>`, the extractor emits both as separate sections (`# Page` from the title metadata + `Page\n===` from html2md). This is a usability issue, not a correctness one. A follow-up sub-PR could detect title/h1 duplication and dedupe.

---

## 4. Test results in detail

### 4.1 Build (§ 2 of testing plan)

```
cargo build         → ok (with stub vizier)
cargo build --release → 2 min 15 s, 32 MB binary, 25 warnings (all dead-code scaffolding for future sub-PRs)
```

### 4.2 Unit tests (§ 3)

`cargo test` → **145 passed; 0 failed; 0.52 s wall-clock**

Per-module breakdown (all matched the testing plan):

| Module | Tests | Notes |
|---|---:|---|
| `watcher::ignore` | 15 | +2 regression tests added during audit |
| `watcher::fingerprint` | 8 | unchanged |
| `watcher::settle` | 6 | unchanged |
| `watcher::normalizer` | 12 | unchanged |
| `watcher::notify_glue` | 10 | one assertion corrected |
| `queue::rusqlite_impl` | 10 | unchanged |
| `sync::batching` | 9 | unchanged |
| `extractors::text` | 15 | unchanged |
| `extractors::html` | 8 | unchanged |
| `extractors::eml` | 9 | one assertion corrected |
| `extractors::epub` | 10 | helpers-only; integration is in §4.3 |
| `extractors` (router) | 7 | unchanged |
| `conversion::chunk` | 9 | one assertion corrected |
| `conversion::markdown` | 15 | unchanged |
| **Total** | **143 + 2 regression = 145** | |

### 4.3 Extractor smoke tests (§ 4)

Every variant from § 4.1 – 4.9 of the testing plan was run. Selected expected/actual pairs:

#### `--extract /tmp/ci-test/sample.csv`

```
--- text-extractor/csv/0.1 (text/csv) ---
extraction_time_ms=0  ocr_used=false  transcription_used=false
--- markdown (111 bytes) ---
# sample

| name | age | city |
| --- | --- | --- |
| Alice | 30 | NYC |
| Bob | 25 | SF |
| Carol | 28 | LA |
```

#### `--extract /tmp/ci-test/big.csv` (250 rows)

The `_250 rows total; showing first/last 50 each._` banner appears as expected, with a `| … |` separator row between samples. First/last data rows present.

#### `--extract /tmp/ci-test/sample.eml`

```
--- eml-extractor/0.1 (message/rfc822) ---
extraction_time_ms=1  ocr_used=false  transcription_used=false
--- markdown (51 bytes) ---
# Lunch?

Hey Bob,
Free for lunch Friday?
-- Alice
```

#### `--extract /tmp/ci-test/book.epub` (real Project Gutenberg *Metamorphosis*)

120,882 bytes of Markdown. 37 lines of section headers. Confidence diagnostic `0.9` reported. Notable cosmetic issue: chapter titles like `Cover -- Metamorphosis` appear because the EPUB has them embedded in `<h1>` tags AND my `chapter_heading` helper picks them up; minor cleanup deferred.

#### `--extract /tmp/ci-test/foo.docx`

`Error: unsupported file type: /tmp/ci-test/foo.docx — see --help for the supported set`

As expected — DOCX/PPTX/XLSX/PDF/image/audio are deferred per the roadmap.

### 4.4 File-watcher smoke tests (§ 5)

| Scenario | Result |
|---|---|
| First touch (create + modify + rename + delete cycle) | ✅ All events surfaced (with macOS FSEvents caveat 3.1: rename → Modified) |
| Default ignore-list (`mkdir node_modules; touch …`) | ✅ After the §2.2 fix, zero decisions for anything under `node_modules` or `.git` |
| Sensitive-list block (`mkdir .ssh; touch id_rsa`) | ✅ After §2.2 fix, zero decisions for anything under `.ssh` / `Library/Keychains` |

### 4.5 Pairing + tray (§ 6)

**Not run.** Tray launches via macOS event-loop integration and triggers Screen Recording permission prompts; out of scope for this audit and gated on a live CI-Server / CI-Portal which isn't available here.

### 4.6 Performance (§ 8)

Not measured numerically. Subjectively: the watcher and the `--extract` calls feel instant at idle and during light use. A real perf sweep is appropriate after `run_full_pipeline` and a real CI-Server endpoint exist.

---

## 5. What I'd ship vs. defer right now

### Ship now (everything in this branch)

- The architecture / roadmap / phase-plan docs ([architecture.md](../architecture.md), [roadmap.md](../roadmap.md), [docs/plan/](plan/))
- All per-sub-PR code that's been merged in ([Phase 2](https://github.com/companionintelligence/CI-Applet/pull/8), [Phase 3](https://github.com/companionintelligence/CI-Applet/pull/9))
- The integration wire-up + CLI subcommands + 145-test suite + the two ignore-list fixes
- This audit doc + [docs/testing-plan.md](testing-plan.md)

### Defer (already documented, not in scope)

- DOCX / PPTX / XLSX (own sub-PRs per [phase-3 CHECKLIST](https://github.com/companionintelligence/CI-Applet/blob/phase-3-conversion/CHECKLIST.md))
- PDF / image OCR / audio Whisper (native binaries + model data needed)
- ZIP recursive (depends on the above)
- Tauri v2 shell ([docs/plan/01-tauri-shell.md](plan/01-tauri-shell.md))
- Phase 4 polish ([docs/plan/04-polish.md](plan/04-polish.md))
- Server-side endpoints for `run_full_pipeline` end-to-end (CI-Server work)
- macOS-native kqueue watcher for richer event info (limitation §3.1)

### Tiny follow-ups worth filing as their own issues

- HTML extractor `<title>` + `<h1>` dedup (limitation §3.4)
- EPUB extractor: strip the `-- BookTitle` suffix many publishers stick on chapter headings
- Replace the local stub `vizier` with the real crate path / repo URL
- Document the actual `vizier` repo location in the README + testing plan
