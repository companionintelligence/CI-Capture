//! BlueBubbles lane — prototype.
//!
//! Polls a self-hosted BlueBubbles server (iMessage/SMS over that Mac's own
//! REST API — see [`client`]) and forwards new messages to CI-Server via the
//! same generic capture-event pipeline the vizier/file-tracker lanes use
//! (`server_ingest::ingest_bluebubbles_message` → `POST /v1/events`).
//!
//! **This is deliberately a smaller lane than file_tracker's.** It has no
//! durable SQLite queue or dead-letter table — just a JSON cursor file
//! (`last_date_created`) advanced only past messages CI-Server actually
//! acknowledged, so a crash or outage re-polls rather than loses anything;
//! it just re-polls the BlueBubbles server for a small overlap instead of
//! replaying from a local queue. Good enough to prove the connector works
//! end-to-end; the batching/backoff/dead-letter machinery `file_tracker.rs`
//! has can be grafted on once this is a real feature rather than a probe.
//!
//! **Known data-model gap, not yet resolved:** messages land in
//! `ci.events`/`ci.capture_events` as an opaque JSON blob (whatever
//! `POST /v1/events` produces), not as a `ci.communications` row with a
//! thread id and participant edges — which is what CI-Server's *own* native
//! BlueBubbles connector (`backend/apps/api/src/domains/bluebubbles/` in
//! CI-Server) already produces by having CI-Server poll the same API
//! directly. Until `/v1/events` (or a dedicated messaging ingest route)
//! grows that structure, routing through here trades richness for "runs on
//! the user's own Mac" — see the design note this module's tests link to.

pub mod check;
pub mod client;
pub mod import_once;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::time::{MissedTickBehavior, interval};

use crate::config::AppConfig;
use crate::server_ingest;

use client::{BlueBubblesClient, BlueBubblesClientConfig, BlueBubblesMessage, PAGE_LIMIT};

/// Floor on the poll interval, mirroring `worker.rs`'s `MIN_INTERVAL_SECS`
/// for the screenshot lane — nothing needs iMessage latency tighter than
/// this, and it keeps a misconfigured `1` in config.json from hammering the
/// user's own Mac.
const MIN_POLL_INTERVAL_SECS: u64 = 10;

/// True if the lane should poll: turned on, and enough to reach a server.
pub fn is_runnable(config: &AppConfig) -> bool {
    config.bluebubbles.enabled
        && !config.bluebubbles.host.trim().is_empty()
        && !config.bluebubbles.password.is_empty()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    /// Highest `dateCreated` (ms epoch) of a message CI-Server has
    /// acknowledged. `None` means "never synced" — the first poll fetches
    /// everything the server holds, oldest first.
    last_date_created: Option<i64>,
}

fn load_cursor(path: &Path) -> Cursor {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic save, matching `config::ConfigManager::save`'s tmp-then-rename —
/// the poll loop is the only writer, but a torn write on a hard kill would
/// otherwise resurrect messages as "new" on next launch, not lose them.
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

/// Run the BlueBubbles lane until aborted (shutdown/reconfigure — same
/// lifecycle as `file_tracker::run`, spawned and aborted the same way from
/// `worker.rs`).
pub async fn run(config: AppConfig, cursor_path: PathBuf) -> Result<()> {
    if !is_runnable(&config) {
        log::info!("bluebubbles: not configured, lane idle");
        return Ok(());
    }

    let device_id = config.identity.device_id.unwrap_or_else(uuid::Uuid::nil);
    let client = BlueBubblesClient::new(BlueBubblesClientConfig {
        host: config.bluebubbles.host.clone(),
        port: config.bluebubbles.port,
        use_https: config.bluebubbles.use_https,
        password: config.bluebubbles.password.clone(),
    })?;

    let mut cursor = load_cursor(&cursor_path);
    let poll_secs = config
        .bluebubbles
        .poll_interval_secs
        .max(MIN_POLL_INTERVAL_SECS);
    let mut tick = interval(Duration::from_secs(poll_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    log::info!(
        "bluebubbles lane running: {}:{}, cursor={:?}, poll every {poll_secs}s",
        config.bluebubbles.host,
        config.bluebubbles.port,
        cursor.last_date_created
    );

    loop {
        tick.tick().await;

        if !server_ingest::is_configured(&config) {
            log::debug!("bluebubbles: no Memory credential yet; skipping poll");
            continue;
        }

        match poll_once(
            &client,
            &config,
            device_id,
            cursor.last_date_created,
            PAGE_LIMIT,
        )
        .await
        {
            Ok(outcome) => {
                if let Some(new_cursor) = outcome.advanced_to {
                    cursor.last_date_created = Some(new_cursor);
                    if let Err(err) = save_cursor(&cursor_path, &cursor) {
                        log::warn!("bluebubbles: failed to persist cursor: {err:?}");
                    }
                }
            }
            Err(err) => log::warn!("bluebubbles: poll failed, will retry next tick: {err:?}"),
        }
    }
}

/// What one [`poll_once`] pass actually did — returned (not just logged) so
/// both the background lane and `--import-bluebubbles-once` can report on
/// the same call rather than one of them re-deriving it from log lines.
#[derive(Debug, Default)]
pub struct PollOutcome {
    /// Messages BlueBubbles returned for this page.
    pub fetched: usize,
    /// Of those, how many CI-Server acknowledged.
    pub ingested: usize,
    /// Highest `dateCreated` actually acknowledged by CI-Server — `None`
    /// means nothing new landed (empty page, or the very first message
    /// failed). Not necessarily the whole page: a mid-page ingest failure
    /// stops advancing here, so the failed message and everything after it
    /// is retried next call rather than skipped.
    pub advanced_to: Option<i64>,
}

/// One poll pass: fetch up to `limit` messages after `after`, forward each
/// in order, and report what happened. `pub(crate)` rather than private so
/// `import_once` can drive the exact same call the background lane makes.
pub(crate) async fn poll_once(
    client: &BlueBubblesClient,
    config: &AppConfig,
    device_id: uuid::Uuid,
    after: Option<i64>,
    limit: u32,
) -> Result<PollOutcome> {
    let page = client
        .query_messages(after, limit)
        .await
        .context("querying BlueBubbles for new messages")?;

    let mut outcome = PollOutcome {
        fetched: page.len(),
        ..PollOutcome::default()
    };

    if page.is_empty() {
        return Ok(outcome);
    }

    log::info!(
        "bluebubbles: fetched {} message(s) since {after:?}",
        page.len()
    );

    for message in &page {
        let Some(date_created) = message.date_created else {
            log::debug!(
                "bluebubbles: skipping message {} with no dateCreated",
                message.guid
            );
            continue;
        };

        match server_ingest::ingest_bluebubbles_message(config, device_id, message).await {
            Ok(()) => {
                outcome.ingested += 1;
                outcome.advanced_to = Some(date_created);
            }
            Err(err) => {
                log::warn!(
                    "bluebubbles: failed to ingest message {}, stopping this page here: {err:?}",
                    message.guid
                );
                break;
            }
        }
    }

    Ok(outcome)
}

/// Human-readable summary of one message, for the `--check-bluebubbles`
/// diagnostic — never logged at normal levels (see the module doc on
/// `server_ingest::ingest_bluebubbles_message` for why message text stays
/// out of logs).
pub fn describe(message: &BlueBubblesMessage) -> String {
    let chat = message
        .chats
        .first()
        .and_then(|c| c.display_name.clone().or_else(|| c.chat_identifier.clone()))
        .unwrap_or_else(|| "?".to_string());
    let from = if message.is_from_me.unwrap_or(false) {
        "me".to_string()
    } else {
        message
            .handle
            .as_ref()
            .map(|h| h.address.clone())
            .unwrap_or_else(|| "?".to_string())
    };
    let len = message.text.as_deref().map(str::len).unwrap_or(0);
    format!(
        "{} chat={chat} from={from} attachments={} text_len={len}",
        message.guid,
        message.attachments.len()
    )
}
