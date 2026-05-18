//! Normalized event schema. The wire shape is documented in
//! [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 4.2.
//!
//! These types are the stable boundary between the OS-specific watcher
//! ([`super::notify_glue`]) and the sync engine. Keep them serializable.

#![allow(dead_code)]

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stable cross-platform event types. The set mirrors the issue #4 §7.2
/// recommendation; do not add OS-specific variants here — collapse them in
/// the normalizer instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    FileCreated,
    FileModified,
    FileMoved,
    FileRenamed,
    FileDeleted,
    FileRestored,
    FileMetadataChanged,
    DirectoryCreated,
    DirectoryDeleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mtime: DateTime<Utc>,
    /// Hex-encoded sha256.
    pub content_hash: String,
    /// Unix inode / Windows FileID. None on platforms where unavailable.
    pub inode: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub event_id: String,
    pub event_type: EventType,
    pub occurred_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub file: Option<FileFingerprint>,
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    /// Groups events that belong to the same logical edit (e.g. an atomic
    /// save sequence). Optional.
    pub correlation_id: Option<String>,
}
