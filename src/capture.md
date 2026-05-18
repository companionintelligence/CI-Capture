# capture.rs

## Purpose
Captures the primary display, holds the resulting raw RGBA buffer, encodes it to PNG, and computes a pixel-diff percentage against a previous frame. This module owns *no* ingest, analysis, or persistence — those live in `worker.rs` and downstream modules.

## Components

### `CapturedImage`
- **Does**: Carries width, height, and the raw RGBA byte buffer for one frame.
- **Interacts with**: `worker.rs` (queues frames), `analysis.rs` (consumes PNG bytes), `server_ingest.rs` / `minio.rs` (consume PNG bytes for upload).
- **Rationale**: Decouples the capture format from downstream consumers. Most consumers want PNG; a few want the in-memory `RgbaImage` for filesystem dumps.

### `CapturedImage::to_png_bytes`
- **Does**: Encodes the RGBA buffer to PNG via `image::codecs::png::PngEncoder`.
- **Interacts with**: Anyone uploading the frame.

### `CapturedImage::to_image`
- **Does**: Builds an `image::RgbaImage` from the buffer (a copy).
- **Interacts with**: `save_to_path` and any code that wants to mutate the frame before saving.

### `capture_primary`
- **Does**: Calls `screenshots::Screen::all()`, takes the first screen, captures it, returns the result as a `CapturedImage`.
- **Interacts with**: `worker.rs::capture_once`.
- **Rationale**: Always uses the first enumerated display. Multi-monitor handling is not in scope today; an issue should track it before changing behaviour.

### `diff_percent`
- **Does**: Returns the percentage of pixels that changed between two `CapturedImage`s. Mismatched dimensions return `100.0`.
- **Interacts with**: `worker.rs` (used to dedupe near-identical frames so we don't analyze the same screen twice).
- **Rationale**: A simple pixel equality check on RGBA tuples; cheap relative to vision-LLM analysis.

### `save_to_path`
- **Does**: Persists a `CapturedImage` to disk, creating parent directories as needed.
- **Interacts with**: Legacy paths that write to a local capture directory before upload (the paired path uploads directly).

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | `capture_primary()` returns a `CapturedImage` synchronously and never blocks for more than ~100ms on a healthy system | Adding async-only capture or removing the in-memory buffer |
| Ingest paths (`server_ingest`, `minio`) | `to_png_bytes()` always returns a valid PNG byte sequence | Switching the default encoding to a non-PNG format |
| `worker.rs` diff dedup | `diff_percent()` is bounded [0.0, 100.0] and treats dimension mismatches as a full change | Returning a different scale or signalling "incomparable" instead of `100.0` |

## Notes
- The `image` crate is configured with `default-features = false, features = ["png"]` in `Cargo.toml`. Other formats (JPEG, WebP) are intentionally not pulled in.
- Only the primary display is captured. Multi-display support is unimplemented.
- The `screenshots` crate requires Screen Recording permission on macOS; first run prompts the user via the OS permission dialog.
