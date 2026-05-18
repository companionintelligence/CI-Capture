# Markdown conversion pipeline — working checklist

This branch implements [docs/plan/03-conversion.md](docs/plan/03-conversion.md). Skeleton modules under `src/extractors/` and `src/conversion/` are committed but **not** wired into the binary yet — they land via sub-PRs after Phase 2's watcher + queue + sync are in place.

Read the plan doc first.

---

## Prerequisites

- [ ] **Phase 2 merged.** The conversion pipeline consumes the `file.modified` / `file.created` event stream Phase 2 produces. Don't start sub-PRs here until that's live.
- [ ] **CI-Server endpoints exist** for `POST /history/blobs/upload`, `POST /history/documents/upsert`, `POST /history/documents/{id}/chunks`. Coordinate with the CI-Server repo.

## Deps to add (`Cargo.toml`)

| Dep | Used by |
|---|---|
| `infer = "0.16"` | MIME sniffing fallback when extension is misleading. |
| `docx-rs = "0.4"` | DOCX extractor (native Rust). |
| `calamine = "0.26"` | XLSX extractor. |
| `quick-xml = "0.36"` + `zip = "2"` | PPTX extractor (manual zip+xml walk). |
| `pdfium-render = "0.8"` | PDF extractor — primary path. |
| `tesseract-rs = "0.1"` | OCR fallback for images and scan-only PDFs. |
| `kamadak-exif = "0.5"` | Image EXIF metadata. |
| `lofty = "0.21"` | Audio metadata. |
| `epub = "2"` | EPUB extractor. |
| `mail-parser = "0.9"` | .eml / .mbox extractor. |
| `scraper = "0.20"` + `html2md = "0.2"` | HTML extractor. |
| `csv = "1"` | CSV / TSV extractor. |
| `tiktoken-rs = "0.6"` | Token-aware chunking. |
| `serde_yaml = "0.9"` | Frontmatter emission. |

Sidecar binaries (downloaded by the installer, not Cargo):

- [ ] `whisper.cpp` (audio transcription) — opt-in per folder.
- [ ] `tesseract` language data (OCR) — bundled or downloaded on first need.

## `src/extractors/`

Each `Extractor` impl returns an `ExtractedDocument` (shape in plan doc § 3).

- [ ] `traits.rs` — `trait Extractor` with `extract(path, fingerprint, opts) -> Result<ExtractedDocument>`.
- [ ] `mod.rs::classify(path, mime_hint) -> Option<Box<dyn Extractor>>` — the router.
- [ ] `text.rs` — txt, csv, json, xml, md.
- [ ] `html.rs` — `scraper` + `html2md`.
- [ ] `docx.rs` — `docx-rs` walk.
- [ ] `pptx.rs` — `zip` + `quick-xml` over `ppt/slides/slide*.xml`.
- [ ] `xlsx.rs` — `calamine` per-sheet.
- [ ] `pdf.rs` — `pdfium-render` text pass; OCR fallback per-page when text density < threshold.
- [ ] `image.rs` — EXIF + OCR (+ optional vision-caption stub).
- [ ] `audio.rs` — `lofty` metadata + optional Whisper sidecar.
- [ ] `epub.rs` — chapter walk + TOC.
- [ ] `zip.rs` — recursive: enumerate, extract supported children, build parent doc with links.
- [ ] `eml.rs` — headers + body (HTML→MD if HTML body).

## `src/conversion/`

- [ ] `mod.rs::run_conversion(events_rx, sync, opts)` — worker pool that pulls events from Phase 2's queue and drives the classified extractor.
- [ ] `chunk.rs` — section-aware chunking, 800 tokens target, 100 overlap, `tiktoken-rs::cl100k_base`.
- [ ] `markdown.rs` — shared helpers: escape, header levels, table assembly, YAML frontmatter, language detection.

## Privacy + size gates

- [ ] Per-folder policy (`full` / `metadata-only` / `ignore`) is read **before** classifying — if metadata-only, never invoke an extractor.
- [ ] Size gate: `> DEFAULT_MAX_FILE_BYTES` → metadata-only.
- [ ] `no_blob_upload` toggle: skip `POST /history/blobs/upload` even when extraction runs.
- [ ] Audio transcription is opt-in **per folder**, not global.

## Concurrency

- [ ] Worker pool size = `min(num_cpus / 2, 4)`. Lower bound 1.
- [ ] Per-format `tokio::sync::Semaphore`:
  - `pdf`: 1 concurrent when OCR is involved, else 2
  - `audio`: 1
  - everything else: 2
- [ ] `tokio::task::spawn_blocking` + `catch_unwind` per extract call; 3rd failure → dead-letter.

## Tests

- [ ] Golden-file tests per format: a tiny `.docx` / `.pptx` / `.xlsx` / `.pdf` in `tests/fixtures/`, snapshot the produced Markdown.
- [ ] Chunker tests: heading-respecting splits, overlap correctness, token budget.
- [ ] Privacy gate tests: metadata-only folder yields no `markdown` body.
- [ ] Crash isolation test: an extractor that panics is caught; queue continues.

## Verification (matches plan §11)

- [ ] Word save → blob + markdown + chunks on server.
- [ ] Mixed PDF (text + scanned) gets text-extraction on text pages, OCR on scan pages, correct order.
- [ ] 50-slide PPTX → one section per slide, notes inline.
- [ ] EXIF-rich JPEG → frontmatter + OCR text.
- [ ] MP3 with transcription enabled → transcript with sentence-level timestamps.
- [ ] Failing extraction does not block event ingestion.
- [ ] Metadata-only folder → zero body content.
- [ ] Idle CPU < 5% with extraction enabled.

---

## Branch hygiene

- One sub-PR per format extractor (the failure cost of one bad extractor crate is low — easier to isolate when small).
- The pool / queue integration is its own sub-PR, landing last.
- Sidecar binaries (Whisper, Tesseract) ship under a separate installer-bundling PR; the in-tree code only needs to be able to invoke them.
