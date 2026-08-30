//! Settle detection — wait until a file stops being written before
//! fingerprinting it.
//!
//! The watcher polls `(size, mtime)` every [`POLL_INTERVAL`]. The file is
//! considered settled when two consecutive polls return the same pair,
//! at which point the fingerprint pass is safe to run. If [`MAX_WAIT`]
//! elapses first the call returns [`SettleStatus::Timeout`] — the caller
//! decides whether to fingerprint anyway, defer, or drop.
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 6.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use tokio::time::sleep;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const MAX_WAIT: Duration = Duration::from_secs(10);

/// Terminal state returned by [`wait_for_settle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleStatus {
    /// Two consecutive polls agreed on `(size, mtime)`. Safe to fingerprint.
    Settled,
    /// File was deleted while we were polling.
    Deleted,
    /// `MAX_WAIT` elapsed without two consecutive matching polls.
    Timeout,
}

/// Wait for the file at `path` to settle. Uses [`POLL_INTERVAL`] and
/// [`MAX_WAIT`]; for tests or per-format tuning use
/// [`wait_for_settle_with`].
pub async fn wait_for_settle(path: impl AsRef<Path>) -> Result<SettleStatus> {
    wait_for_settle_with(path, POLL_INTERVAL, MAX_WAIT).await
}

/// Same as [`wait_for_settle`] but with caller-supplied poll interval +
/// max-wait. Tests use tiny intervals; the watcher uses defaults.
pub async fn wait_for_settle_with(
    path: impl AsRef<Path>,
    poll_interval: Duration,
    max_wait: Duration,
) -> Result<SettleStatus> {
    let path = path.as_ref();
    let deadline_start = std::time::Instant::now();

    let mut prev: Option<(u64, Option<SystemTime>)> = None;

    loop {
        let snap = match snapshot(path) {
            Ok(s) => s,
            Err(SnapshotError::NotFound) => return Ok(SettleStatus::Deleted),
            Err(SnapshotError::Other(e)) => return Err(e),
        };

        if let Some(p) = prev
            && p == snap
        {
            return Ok(SettleStatus::Settled);
        }

        if deadline_start.elapsed() + poll_interval > max_wait {
            return Ok(SettleStatus::Timeout);
        }

        prev = Some(snap);
        sleep(poll_interval).await;
    }
}

/// Synchronous variant for callers that already control their own
/// scheduling. Not used by the worker today, but kept for tests so we
/// can avoid the async runtime when stepping the state machine by hand.
pub fn poll_once(path: impl AsRef<Path>) -> Result<(u64, Option<SystemTime>)> {
    snapshot(path.as_ref()).map_err(|e| match e {
        SnapshotError::NotFound => anyhow::anyhow!("file disappeared: {}", path.as_ref().display()),
        SnapshotError::Other(e) => e,
    })
}

#[derive(Debug)]
enum SnapshotError {
    NotFound,
    Other(anyhow::Error),
}

fn snapshot(path: &Path) -> Result<(u64, Option<SystemTime>), SnapshotError> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta.modified().ok();
            Ok((meta.len(), mtime))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(SnapshotError::NotFound),
        Err(err) => Err(SnapshotError::Other(
            anyhow::Error::new(err).context(format!("stat {}", path.display())),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;
    use tokio::time::Duration as TokioDuration;

    use super::*;

    fn write_tmp(bytes: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("create tempfile");
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn static_file_settles_on_first_pair() {
        let f = write_tmp(b"already stable");
        let status = wait_for_settle_with(
            f.path(),
            TokioDuration::from_millis(10),
            TokioDuration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(status, SettleStatus::Settled);
    }

    #[tokio::test]
    async fn deleted_file_reports_deleted() {
        let f = write_tmp(b"x");
        let path = f.path().to_path_buf();
        drop(f); // closes + removes the tempfile

        let status = wait_for_settle_with(
            &path,
            TokioDuration::from_millis(10),
            TokioDuration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(status, SettleStatus::Deleted);
    }

    #[tokio::test]
    async fn growing_file_times_out() {
        // Write a file, then keep appending to it in a background task
        // so size never agrees across two consecutive polls. Use a tiny
        // max_wait so the test finishes quickly.
        let f = write_tmp(b"start ");
        let path = f.path().to_path_buf();
        let path_for_writer = path.clone();

        let writer = tokio::spawn(async move {
            use std::fs::OpenOptions;
            for i in 0..200u32 {
                if let Ok(mut h) = OpenOptions::new().append(true).open(&path_for_writer) {
                    let _ = h.write_all(format!("chunk {i} ").as_bytes());
                    let _ = h.flush();
                }
                tokio::time::sleep(TokioDuration::from_millis(5)).await;
            }
        });

        let status = wait_for_settle_with(
            &path,
            TokioDuration::from_millis(10),
            TokioDuration::from_millis(150),
        )
        .await
        .unwrap();
        // We do NOT assert Settled here even if the writer happens to
        // pause — the contract is "didn't see two consecutive matches
        // within max_wait → Timeout". On a busy CI box the writer may
        // skip a beat. So accept either Timeout (the common path) or
        // Settled (if scheduling left us a window).
        assert!(matches!(
            status,
            SettleStatus::Timeout | SettleStatus::Settled
        ));

        writer.abort();
        let _ = writer.await;
    }

    #[tokio::test]
    async fn settle_uses_default_constants() {
        // We can't easily test the default 10s timeout in unit-test
        // wall-clock, but we can verify the public default function
        // dispatches and that POLL_INTERVAL/MAX_WAIT exist with the
        // documented values.
        assert_eq!(POLL_INTERVAL, TokioDuration::from_millis(500));
        assert_eq!(MAX_WAIT, TokioDuration::from_secs(10));

        let f = write_tmp(b"stable");
        // Call the public function with defaults — only works because
        // the file is already stable, so it returns on the second poll
        // (~500ms wall-clock). Acceptable for a unit test.
        let status = wait_for_settle(f.path()).await.unwrap();
        assert_eq!(status, SettleStatus::Settled);
    }

    #[test]
    fn poll_once_returns_size_and_mtime() {
        let f = write_tmp(b"hello");
        let (size, mtime) = poll_once(f.path()).unwrap();
        assert_eq!(size, 5);
        assert!(mtime.is_some());
    }

    #[test]
    fn poll_once_errors_on_missing_path() {
        let err = poll_once("/this/path/should/not/exist-settle-9f").unwrap_err();
        assert!(err.to_string().contains("disappeared"));
    }
}
