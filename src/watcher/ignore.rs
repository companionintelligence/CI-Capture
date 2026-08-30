//! Gitignore-style + sensitive-path ignore rules.
//!
//! Two layers stack:
//! 1. A built-in **sensitive list** that can never be disabled (keychain,
//!    ssh keys, credential stores, password vaults).
//! 2. A user-controlled **glob list** built on top of [`globset::GlobSet`],
//!    seeded with [`DEFAULT_IGNORES`] but each default individually
//!    disable-able.
//!
//! The defaults are lifted from issue #4 §12 and
//! [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 7.
//!
//! Enforcement happens at *event time*, before fingerprinting or queueing,
//! so an ignored path never reaches disk-hash CPU or the sync engine.

use std::path::Path;

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};

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
    // Virtual machines
    "**/*.vmdk",
    "**/*.vdi",
    "**/*.qcow2",
];

/// Hard-wired sensitive patterns. The user **cannot** disable these — they
/// catch credential stores, key material, and password vaults. The applet
/// has no business shipping any of these to a memory server.
pub const SENSITIVE_PATTERNS: &[&str] = &[
    "**/Library/Keychains/**",
    "**/.ssh/**",
    "**/.aws/credentials",
    "**/.aws/config",
    "**/.gnupg/**",
    "**/.config/gcloud/**",
    "**/.kube/config",
    "**/.netrc",
    "**/.pgpass",
    // Dotenv files — routinely hold DB passwords and API keys.
    "**/.env",
    "**/.env.*",
    // Private keys, certificates, and keystores anywhere (not just ~/.ssh).
    // `*.key` can in theory match a flat Keynote export, but modern Keynote
    // files are bundles whose inner files don't end in `.key`; the risk of
    // leaking a TLS/SSH private key outweighs that rare false positive.
    "**/*.pem",
    "**/*.key",
    "**/*.p12",
    "**/*.pfx",
    "**/*.jks",
    "**/*.keystore",
    "**/id_rsa*",
    "**/id_dsa*",
    "**/id_ecdsa*",
    "**/id_ed25519*",
    // Common password-manager vaults
    "**/*.1pif",
    "**/*.kdbx",
    "**/*.agilekeychain/**",
];

/// Default max file size in bytes (100 MB). Files above this are skipped
/// by default; user-configurable up to [`HARD_CAP_FILE_BYTES`].
pub const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const HARD_CAP_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Compiled ignore rules. Cheap to query (`O(globs)` per check), expensive
/// to build (`O(globs)` once at startup). Build once, share across the
/// watcher.
#[derive(Clone)]
pub struct IgnoreRules {
    user: GlobSet,
    sensitive: GlobSet,
    max_file_bytes: u64,
}

impl IgnoreRules {
    /// Build an empty rule set — no defaults, sensitive patterns still on.
    pub fn empty() -> Result<Self> {
        Self::from_patterns(&[], DEFAULT_MAX_FILE_BYTES)
    }

    /// Build a rule set seeded with every entry in [`DEFAULT_IGNORES`].
    pub fn with_defaults() -> Result<Self> {
        Self::from_patterns(DEFAULT_IGNORES, DEFAULT_MAX_FILE_BYTES)
    }

    /// Build a rule set from an explicit list of user-controlled patterns.
    /// Sensitive patterns are always added on top of whatever the caller
    /// passes in.
    pub fn from_patterns(user_patterns: &[&str], max_file_bytes: u64) -> Result<Self> {
        let mut user_builder = GlobSetBuilder::new();
        for pat in user_patterns {
            user_builder.add(Glob::new(pat)?);
        }

        let mut sensitive_builder = GlobSetBuilder::new();
        for pat in SENSITIVE_PATTERNS {
            sensitive_builder.add(Glob::new(pat)?);
        }

        Ok(Self {
            user: user_builder.build()?,
            sensitive: sensitive_builder.build()?,
            max_file_bytes: max_file_bytes.min(HARD_CAP_FILE_BYTES),
        })
    }

    /// Returns the configured per-file size cap.
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// True if the path matches any ignore pattern (user-defined or
    /// sensitive).
    pub fn is_ignored(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        self.sensitive.is_match(path) || self.user.is_match(path)
    }

    /// True if the file is ignored *because* it matches a sensitive
    /// pattern. Useful for logging — we want to know we're explicitly
    /// refusing key material, not just skipping a build artifact.
    pub fn is_sensitive(&self, path: impl AsRef<Path>) -> bool {
        self.sensitive.is_match(path.as_ref())
    }

    /// True if the file exceeds the configured size cap.
    pub fn exceeds_size_limit(&self, size_bytes: u64) -> bool {
        size_bytes > self.max_file_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> IgnoreRules {
        IgnoreRules::with_defaults().expect("default patterns compile")
    }

    #[test]
    fn node_modules_is_ignored() {
        let r = defaults();
        assert!(r.is_ignored("/Users/x/proj/node_modules/lodash/index.js"));
        assert!(r.is_ignored("repo/node_modules/.bin/tsc"));
    }

    #[test]
    fn git_internal_is_ignored() {
        let r = defaults();
        assert!(r.is_ignored("/Users/x/proj/.git/HEAD"));
        assert!(r.is_ignored("/Users/x/proj/.git/refs/heads/main"));
    }

    #[test]
    fn target_dir_is_ignored() {
        let r = defaults();
        assert!(r.is_ignored("/Users/x/proj/target/release/CI-Applet"));
    }

    #[test]
    fn regular_documents_pass() {
        let r = defaults();
        assert!(!r.is_ignored("/Users/x/Documents/CES/pitch.docx"));
        assert!(!r.is_ignored("/Users/x/Documents/notes.md"));
        assert!(!r.is_ignored("/Users/x/Desktop/photo.jpg"));
    }

    #[test]
    fn ssh_keys_are_sensitive() {
        let r = IgnoreRules::empty().unwrap();
        // Empty user list — but sensitive patterns still fire.
        assert!(r.is_ignored("/Users/x/.ssh/id_rsa"));
        assert!(r.is_sensitive("/Users/x/.ssh/id_rsa"));
    }

    #[test]
    fn aws_credentials_are_sensitive() {
        let r = IgnoreRules::empty().unwrap();
        assert!(r.is_ignored("/Users/x/.aws/credentials"));
        assert!(r.is_sensitive("/Users/x/.aws/credentials"));
    }

    #[test]
    fn dotenv_and_private_keys_are_sensitive() {
        let r = IgnoreRules::empty().unwrap();
        // Dotenv secrets.
        assert!(r.is_sensitive("/Users/x/proj/.env"));
        assert!(r.is_sensitive("/Users/x/proj/.env.production"));
        // Private keys / certs / keystores outside ~/.ssh.
        assert!(r.is_sensitive("/Users/x/Downloads/server.pem"));
        assert!(r.is_sensitive("/Users/x/Downloads/tls.key"));
        assert!(r.is_sensitive("/Users/x/certs/bundle.p12"));
        assert!(r.is_sensitive("/Users/x/app/release.keystore"));
        assert!(r.is_sensitive("/Users/x/keys/id_ed25519"));
        assert!(r.is_sensitive("/Users/x/keys/id_rsa_backup"));
        // Ordinary documents are not.
        assert!(!r.is_sensitive("/Users/x/Documents/notes.md"));
        assert!(!r.is_sensitive("/Users/x/Documents/budget.csv"));
    }

    #[test]
    fn keychain_is_sensitive() {
        let r = IgnoreRules::empty().unwrap();
        assert!(r.is_ignored("/Users/x/Library/Keychains/login.keychain-db"));
        assert!(r.is_sensitive("/Users/x/Library/Keychains/login.keychain-db"));
    }

    #[test]
    fn user_pattern_works_with_no_defaults() {
        let r = IgnoreRules::from_patterns(&["**/*.tmp"], DEFAULT_MAX_FILE_BYTES).unwrap();
        assert!(r.is_ignored("/Users/x/foo.tmp"));
        assert!(!r.is_ignored("/Users/x/foo.txt"));
        // Defaults are NOT active here.
        assert!(!r.is_ignored("/Users/x/proj/node_modules/index.js"));
        // Sensitive is still active even without defaults.
        assert!(r.is_ignored("/Users/x/.ssh/id_rsa"));
    }

    #[test]
    fn size_limit_respected() {
        let r = defaults();
        assert_eq!(r.max_file_bytes(), DEFAULT_MAX_FILE_BYTES);
        assert!(!r.exceeds_size_limit(1024));
        assert!(!r.exceeds_size_limit(DEFAULT_MAX_FILE_BYTES));
        assert!(r.exceeds_size_limit(DEFAULT_MAX_FILE_BYTES + 1));
    }

    #[test]
    fn size_limit_capped_at_hard_max() {
        let r = IgnoreRules::from_patterns(&[], HARD_CAP_FILE_BYTES * 4).unwrap();
        // Above the hard cap, the rule clamps down.
        assert_eq!(r.max_file_bytes(), HARD_CAP_FILE_BYTES);
    }

    #[test]
    fn vm_disk_images_are_ignored_by_default() {
        let r = defaults();
        assert!(r.is_ignored("/Users/x/VMs/win10.vmdk"));
        assert!(r.is_ignored("/Users/x/VMs/disk.qcow2"));
    }

    #[test]
    fn browser_profiles_are_ignored_by_default() {
        let r = defaults();
        assert!(r.is_ignored("/Users/x/Library/Application Support/Google/Chrome/Default/Cookies"));
        assert!(r.is_ignored(
            "/Users/x/Library/Application Support/Firefox/Profiles/default/places.sqlite"
        ));
    }

    #[test]
    fn ds_store_is_ignored() {
        let r = defaults();
        assert!(r.is_ignored("/Users/x/Documents/.DS_Store"));
    }
}
