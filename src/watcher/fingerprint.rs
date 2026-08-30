//! Stream-hash the settled file, capture inode/size/mtime.
//!
//! The watcher calls [`fingerprint`] *after* settle-detection has reported
//! the file is no longer being written. The hash is sha256 over the file
//! bytes, streamed in 64 KiB chunks so we don't load multi-GB files into
//! memory.
//!
//! Inode (Unix) / FileID (Windows) is captured when available — the
//! normalizer uses it to infer rename-vs-delete+create.
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 6.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use super::types::FileFingerprint;

/// Chunk size for streaming hashing. 64 KiB matches typical filesystem
/// readahead and keeps the working set small.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// Stream-hash the file at `path` and return a `FileFingerprint`.
///
/// Returns an error if the file doesn't exist, can't be opened, or can't
/// be read mid-hash. Caller is responsible for checking ignore rules /
/// size limits *before* invoking this — `fingerprint` doesn't know about
/// policy, only mechanics.
pub fn fingerprint(path: impl AsRef<Path>) -> Result<FileFingerprint> {
    let path = path.as_ref();
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let size_bytes = metadata.len();
    let mtime = system_time_to_utc(metadata.modified().ok());
    let inode = file_id(&metadata);

    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(HASH_CHUNK_BYTES, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];

    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let digest = hasher.finalize();
    let content_hash = format!("sha256:{}", hex_lower(&digest));

    Ok(FileFingerprint {
        path: path.to_path_buf(),
        size_bytes,
        mtime,
        content_hash,
        inode,
    })
}

/// True if `b` describes the same content + identity as `a`. Used by the
/// fingerprint cache to skip re-hashing.
pub fn is_same_content(a: &FileFingerprint, b: &FileFingerprint) -> bool {
    a.content_hash == b.content_hash && a.size_bytes == b.size_bytes
}

fn system_time_to_utc(maybe: Option<SystemTime>) -> DateTime<Utc> {
    let st = maybe.unwrap_or(SystemTime::UNIX_EPOCH);
    DateTime::<Utc>::from(st)
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(windows)]
fn file_id(_meta: &std::fs::Metadata) -> Option<u64> {
    // A stable Windows file ID needs the unstable `windows_by_handle` API
    // (`MetadataExt::file_index`). Until it stabilizes we skip inode-based
    // rename inference on Windows; the normalizer falls back gracefully to
    // treating a rename as a delete + create.
    None
}

#[cfg(not(any(unix, windows)))]
fn file_id(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_tmp(bytes: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("create tempfile");
        f.write_all(bytes).expect("write tempfile");
        f.flush().expect("flush tempfile");
        f
    }

    #[test]
    fn empty_file_hashes_to_known_sha256() {
        let f = write_tmp(b"");
        let fp = fingerprint(f.path()).unwrap();
        assert_eq!(fp.size_bytes, 0);
        // The well-known sha256 of the empty string.
        assert_eq!(
            fp.content_hash,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn small_file_hashes_to_known_sha256() {
        let f = write_tmp(b"abc");
        let fp = fingerprint(f.path()).unwrap();
        assert_eq!(fp.size_bytes, 3);
        assert_eq!(
            fp.content_hash,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hash_is_stable_across_chunk_boundaries() {
        // > HASH_CHUNK_BYTES (64 KiB) so the loop runs multiple iterations.
        let payload: Vec<u8> = (0u32..200_000).map(|i| (i % 251) as u8).collect();
        let f1 = write_tmp(&payload);
        let f2 = write_tmp(&payload);
        let h1 = fingerprint(f1.path()).unwrap();
        let h2 = fingerprint(f2.path()).unwrap();
        assert_eq!(h1.content_hash, h2.content_hash);
        assert_eq!(h1.size_bytes, payload.len() as u64);
    }

    #[test]
    fn different_content_different_hash() {
        let a = write_tmp(b"hello world");
        let b = write_tmp(b"hello world!");
        let ha = fingerprint(a.path()).unwrap();
        let hb = fingerprint(b.path()).unwrap();
        assert_ne!(ha.content_hash, hb.content_hash);
        assert!(!is_same_content(&ha, &hb));
    }

    #[test]
    fn same_content_is_same() {
        let a = write_tmp(b"hello world");
        let b = write_tmp(b"hello world");
        let ha = fingerprint(a.path()).unwrap();
        let hb = fingerprint(b.path()).unwrap();
        assert!(is_same_content(&ha, &hb));
    }

    #[test]
    fn missing_file_errors_cleanly() {
        let err = fingerprint("/this/path/should/not/exist-9e8a2f").unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("stat") || msg.contains("/this/path/"),
            "error should mention the path or stat step: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_inode_is_captured() {
        let f = write_tmp(b"x");
        let fp = fingerprint(f.path()).unwrap();
        assert!(fp.inode.is_some(), "inode should be present on Unix");
    }

    #[test]
    fn hex_lower_round_trip() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_lower(&[]), "");
        assert_eq!(hex_lower(&[0xab, 0xcd]), "abcd");
    }
}
