//! Gitignore-style + sensitive-path ignore rules.
//!
//! Defaults match issue #4 §12 and [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 7.

#![allow(dead_code)]

/// Default ignore patterns — shipped on by default. Each entry is a glob
/// the user can individually disable from the settings UI.
pub const DEFAULT_IGNORES: &[&str] = &[
    // Source control
    "**/.git/**",
    "**/.hg/**",
    "**/.svn/**",
    // Build artifacts
    "**/node_modules/**",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/.next/**",
    "**/__pycache__/**",
    // OS / tool caches
    "**/.DS_Store",
    "**/Thumbs.db",
    "**/.Trash/**",
    "**/Library/Caches/**",
    "**/AppData/Local/Temp/**",
    // Browser profiles
    "**/Library/Application Support/Google/Chrome/**",
    "**/Library/Application Support/Firefox/**",
    "**/Library/Application Support/Arc/**",
    "**/AppData/Local/Google/Chrome/**",
    "**/AppData/Local/Mozilla/**",
    // Sensitive
    "**/Library/Keychains/**",
    "**/.ssh/**",
    "**/.aws/credentials",
    "**/.gnupg/**",
    // Virtual machines
    "**/*.vmdk",
    "**/*.vdi",
    "**/*.qcow2",
];

/// Default max file size in bytes (100 MB). Files above this are skipped
/// by default; user-configurable up to a hard cap of 2 GB.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const HARD_CAP_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
