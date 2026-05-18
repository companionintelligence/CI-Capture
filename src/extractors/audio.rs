//! Audio extractor: `lofty` for metadata; optional Whisper sidecar for
//! transcription. Sidecar invocation is gated on the per-folder
//! `allow_heavy_models` policy AND a global "audio transcription" toggle.

#![allow(dead_code)]
