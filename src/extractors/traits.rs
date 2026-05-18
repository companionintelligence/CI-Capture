//! Extractor trait + the canonical `ExtractedDocument` output shape.
//!
//! Skeleton — see [`docs/plan/03-conversion.md`](../../../docs/plan/03-conversion.md) § 3.

#![allow(dead_code)]

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub heading: String,
    pub anchor: String,
    pub markdown: String,
    pub order: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionDiagnostics {
    pub extractor: String,
    pub ocr_used: bool,
    pub transcription_used: bool,
    pub warnings: Vec<String>,
    pub extraction_time_ms: u64,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedDocument {
    pub document_id: String,
    pub file_id: String,
    pub version_id: String,
    pub source_path: String,
    pub content_type: String,
    pub markdown: String,
    pub sections: Vec<Section>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub diagnostics: ExtractionDiagnostics,
}

#[derive(Debug, Clone, Copy)]
pub struct ExtractOptions {
    /// When `true`, the extractor only fills metadata and skips body content.
    pub metadata_only: bool,
    /// When `true`, image OCR / audio transcription is allowed (if the
    /// extractor supports it).
    pub allow_heavy_models: bool,
}

pub trait Extractor: Send + Sync {
    fn name(&self) -> &'static str;
    fn extract(
        &self,
        path: &Path,
        opts: ExtractOptions,
    ) -> Result<ExtractedDocument>;
}
