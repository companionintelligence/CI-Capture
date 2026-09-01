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

use reader::ImessageRow;

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
    last_date_created_unix_ms: Option<i64>,
}

fn load_cursor(path: &Path) -> Cursor {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
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

    if permissions::full_disk_access_status() != PermissionStatus::Granted {
        log::warn!(
            "imessage: Full Disk Access not granted; lane idle until it is (System Settings → Privacy & Security → Full Disk Access)"
        );
        return Ok(());
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
        cursor.last_date_created_unix_ms
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
            cursor.last_date_created_unix_ms,
            reader::DEFAULT_LIMIT,
        )
        .await
        {
            Ok(outcome) => {
                if let Some(new_cursor) = outcome.advanced_to {
                    cursor.last_date_created_unix_ms = Some(new_cursor);
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
    pub advanced_to: Option<i64>,
}

/// One poll pass: open `chat.db` fresh (a long-lived handle would hold a
/// read transaction against a file Messages.app itself is actively
/// writing), read up to `limit` messages after `after`, forward each, report
/// what happened.
pub(crate) async fn poll_once(
    db_path: &Path,
    config: &AppConfig,
    device_id: uuid::Uuid,
    after: Option<i64>,
    limit: u32,
) -> Result<PollOutcome> {
    let db_path = db_path.to_path_buf();
    // rusqlite::Connection is blocking and !Send in the way that matters
    // here (holds a raw pointer) — do the whole read on a blocking thread
    // rather than smuggle it across an await point.
    let rows: Vec<ImessageRow> = tokio::task::spawn_blocking(move || {
        let conn = reader::open_readonly(&db_path)?;
        reader::read_messages(&conn, after, limit)
    })
    .await
    .context("joining chat.db read thread")??;

    let mut outcome = PollOutcome {
        fetched: rows.len(),
        ..PollOutcome::default()
    };

    if rows.is_empty() {
        return Ok(outcome);
    }

    log::info!("imessage: read {} message(s) since {after:?}", rows.len());

    for row in &rows {
        match server_ingest::ingest_imessage_message(config, device_id, row).await {
            Ok(()) => {
                outcome.ingested += 1;
                outcome.advanced_to = Some(row.date_created_unix_ms);
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
