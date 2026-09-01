//! Local iMessage/SMS lane — reads `~/Library/Messages/chat.db` on this Mac
//! directly and forwards new messages to CI-Server, the same way the
//! `bluebubbles` lane does for a *network* BlueBubbles server. This is the
//! path for a Mac that has no BlueBubbles server running at all: Companion
//! Capture already runs here as a trusted, permission-onboarded local agent,
//! so it can read the same database BlueBubbles itself would.
//!
//! Requires **Full Disk Access** (see `crate::permissions::full_disk_access_status`)
//! — unlike Screen Recording there is no system prompt to trigger
//! programmatically, only a deep link to System Settings and the user
//! manually adding Companion Capture, then relaunching it.
//!
//! **Fidelity gap, deliberately scoped down for this first pass** (see
//! `reader`'s module doc for the exact list): no `attributedBody` decoding
//! (recent macOS messages with no plain `text` come through empty), no full
//! chat-participant lists, no attachments/reactions/reply-threading. CI-Server's
//! own `datasources/imessage.ts` has all of that — this lane proves the local
//! read + forward pipeline works, matching the scope `bluebubbles`'s prototype
//! shipped at.

pub mod check;
#[cfg(test)]
mod e2e;
pub mod import_once;
pub mod reader;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::time::{MissedTickBehavior, interval};

use crate::config::AppConfig;
use crate::permissions::{self, PermissionStatus};
use crate::server_ingest;

use reader::{ImessageCursor, ImessageRow, MessagePage};

/// Floor on the poll interval — same reasoning as `bluebubbles`'s.
const MIN_POLL_INTERVAL_SECS: u64 = 10;

/// True if the lane should poll: turned on. (Full Disk Access is checked at
/// `run` time, not here — `is_runnable` mirrors `bluebubbles::is_runnable`'s
/// "would this lane do anything if started" contract, and a missing FDA
/// grant is a runtime failure mode to report, not a configuration choice.)
pub fn is_runnable(config: &AppConfig) -> bool {
    config.imessage.enabled
}

/// `~/Library/Messages/chat.db` — the one path this lane ever reads from.
/// No override: unlike `bluebubbles`'s host/port, there is exactly one
/// answer to "which Mac's Messages" — this one.
pub fn chat_db_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join("Library/Messages/chat.db"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    /// Compound `(message.date, ROWID)` in chat.db's own units — see
    /// [`reader::ImessageCursor`] for why a millisecond timestamp cannot serve
    /// as one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<ImessageCursor>,
    /// The only field the pre-compound format had. Read but never written,
    /// purely so a stale file is recognized and reported rather than looking
    /// like "never synced".
    #[serde(default, skip_serializing)]
    last_date_created_unix_ms: Option<i64>,
}

/// A cursor file older than the compound format is discarded, not converted:
/// turning a Unix-millisecond value back into a raw `message.date` means
/// guessing whether this chat.db counts in seconds or nanoseconds — the
/// database-wide guess this lane just stopped making — and guessing wrong
/// skips history silently, which is the failure being removed. Re-reading from
/// the beginning costs traffic instead, and CI-Server dedups on
/// `source_id = imessage:<guid>`, so the replay lands as duplicates-suppressed
/// rather than as duplicated events.
fn load_cursor(path: &Path) -> Cursor {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Cursor::default();
    };
    match serde_json::from_str::<Cursor>(&raw) {
        Ok(cursor) => {
            if cursor.last.is_none()
                && let Some(legacy_ms) = cursor.last_date_created_unix_ms
            {
                log::warn!(
                    "imessage: cursor file {} is in the pre-compound format (last_date_created_unix_ms={legacy_ms}); restarting from the oldest message — CI-Server dedups the replay",
                    path.display()
                );
            }
            cursor
        }
        Err(err) => {
            log::warn!(
                "imessage: cursor file {} is unreadable, restarting from the oldest message: {err}",
                path.display()
            );
            Cursor::default()
        }
    }
}

fn save_cursor(path: &Path, cursor: &Cursor) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cursor dir {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(cursor)?)
        .with_context(|| format!("writing cursor temp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("committing cursor to {}", path.display()))?;
    Ok(())
}

/// Run the lane until aborted — same lifecycle as `bluebubbles::run` /
/// `file_tracker::run`, spawned and stopped the same way from `worker.rs`.
pub async fn run(config: AppConfig, cursor_path: PathBuf) -> Result<()> {
    if !is_runnable(&config) {
        log::info!("imessage: not enabled, lane idle");
        return Ok(());
    }

    // Three states, not two: `NotRequired` means there is no chat.db on this
    // Mac at all (Messages has never run here), and telling that user to grant
    // Full Disk Access sends them to fix a permission that would change
    // nothing.
    match permissions::full_disk_access_status() {
        PermissionStatus::Granted => {}
        PermissionStatus::NotRequired => {
            log::info!(
                "imessage: no {} on this Mac — nothing to read, lane idle",
                chat_db_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "~/Library/Messages/chat.db".to_string())
            );
            return Ok(());
        }
        PermissionStatus::Denied => {
            log::warn!(
                "imessage: Full Disk Access not granted; lane idle until it is (System Settings → Privacy & Security → Full Disk Access)"
            );
            return Ok(());
        }
    }

    let device_id = config.identity.device_id.unwrap_or_else(uuid::Uuid::nil);
    let db_path = chat_db_path()?;
    let mut cursor = load_cursor(&cursor_path);
    let poll_secs = config
        .imessage
        .poll_interval_secs
        .max(MIN_POLL_INTERVAL_SECS);
    let mut tick = interval(Duration::from_secs(poll_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    log::info!(
        "imessage lane running: {}, cursor={:?}, poll every {poll_secs}s",
        db_path.display(),
        cursor.last
    );

    loop {
        tick.tick().await;

        if !server_ingest::is_configured(&config) {
            log::debug!("imessage: no Memory credential yet; skipping poll");
            continue;
        }

        match poll_once(
            &db_path,
            &config,
            device_id,
            cursor.last,
            reader::DEFAULT_LIMIT,
        )
        .await
        {
            Ok(outcome) => {
                if let Some(new_cursor) = outcome.advanced_to {
                    cursor.last = Some(new_cursor);
                    if let Err(err) = save_cursor(&cursor_path, &cursor) {
                        log::warn!("imessage: failed to persist cursor: {err:?}");
                    }
                }
            }
            Err(err) => log::warn!("imessage: poll failed, will retry next tick: {err:?}"),
        }
    }
}

/// What one [`poll_once`] pass did — same shape as `bluebubbles::PollOutcome`
/// so `--check-imessage`/`--import-imessage-once` (if added later, mirroring
/// `bluebubbles::import_once`) can report on it identically.
#[derive(Debug, Default)]
pub struct PollOutcome {
    pub fetched: usize,
    pub ingested: usize,
    /// Rows chat.db returned whose `message.date` is not a representable
    /// timestamp. They are dropped, so they are counted here rather than only
    /// shrinking `fetched` — see [`reader::MessagePage`].
    pub skipped_invalid_timestamp: usize,
    pub advanced_to: Option<ImessageCursor>,
}

/// One poll pass: open `chat.db` fresh (a long-lived handle would hold a
/// read transaction against a file Messages.app itself is actively
/// writing), read up to `limit` messages after `after`, forward each, report
/// what happened.
pub(crate) async fn poll_once(
    db_path: &Path,
    config: &AppConfig,
    device_id: uuid::Uuid,
    after: Option<ImessageCursor>,
    limit: u32,
) -> Result<PollOutcome> {
    let db_path = db_path.to_path_buf();
    // rusqlite::Connection is blocking and !Send in the way that matters
    // here (holds a raw pointer) — do the whole read on a blocking thread
    // rather than smuggle it across an await point.
    let page: MessagePage = tokio::task::spawn_blocking(move || {
        let conn = reader::open_readonly(&db_path)?;
        reader::read_messages(&conn, after, limit)
    })
    .await
    .context("joining chat.db read thread")??;

    let mut outcome = PollOutcome {
        fetched: page.rows.len(),
        skipped_invalid_timestamp: page.skipped_invalid_timestamp,
        ..PollOutcome::default()
    };

    if page.skipped_invalid_timestamp > 0 {
        log::warn!(
            "imessage: dropped {} row(s) with an unreadable message.date (logged individually above)",
            page.skipped_invalid_timestamp
        );
    }

    if page.rows.is_empty() {
        return Ok(outcome);
    }

    log::info!(
        "imessage: read {} message(s) since {after:?}",
        page.rows.len()
    );

    for row in &page.rows {
        match server_ingest::ingest_imessage_message(config, device_id, row).await {
            Ok(()) => {
                outcome.ingested += 1;
                outcome.advanced_to = Some(row.cursor());
            }
            Err(err) => {
                log::warn!(
                    "imessage: failed to ingest message rowid={}, stopping this page here: {err:?}",
                    row.rowid
                );
                break;
            }
        }
    }

    Ok(outcome)
}

/// Human-readable summary of one row, for `--check-imessage` — never the
/// message text itself, matching `bluebubbles::describe`'s reasoning: this
/// carries someone else's words, not just this device's own activity.
pub fn describe(row: &ImessageRow) -> String {
    let chat = row
        .chat_display_name
        .clone()
        .or_else(|| row.chat_identifier.clone())
        .unwrap_or_else(|| "?".to_string());
    let from = if row.is_from_me {
        "me".to_string()
    } else {
        row.handle.clone().unwrap_or_else(|| "?".to_string())
    };
    let len = row.text.as_deref().map(str::len).unwrap_or(0);
    format!(
        "rowid={} chat={chat} from={from} attachments={} text_len={len}",
        row.rowid, row.attachment_count
    )
}
