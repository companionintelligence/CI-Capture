//! `--import-imessage-once` — runs ONE real read-and-ingest pass against
//! this Mac's own `chat.db` and a live CI-Server, through the exact code
//! path the background lane uses (`poll_once` →
//! `server_ingest::ingest_imessage_message` → `POST /v1/events`). Mirrors
//! `bluebubbles::import_once` — same reasoning throughout.
//!
//! Run with:
//! ```text
//! CI_SERVER_URL=http://localhost:55528 CI_SERVER_API_KEY=... \
//!   cargo run -- --import-imessage-once
//! ```
//!
//! Reads CI-Server credentials from the environment, not the persisted app
//! config, for the same reason `bluebubbles::import_once` does. Requires
//! Full Disk Access already granted — this does not attempt to request it
//! (there is nothing to request; see `crate::permissions`).
//!
//! Small by default (5 messages) — a demonstration that the pipeline works,
//! not a bulk backfill tool; the background lane (`imessage::run`) is what
//! does ongoing sync. `IMESSAGE_IMPORT_LIMIT` raises it for an explicit,
//! opt-in larger one-off run — the default a bare invocation gets stays 5.

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::permissions::{self, PermissionStatus};

use super::{chat_db_path, describe, poll_once, reader};

const DEFAULT_LIMIT: u32 = 5;

fn limit() -> u32 {
    std::env::var("IMESSAGE_IMPORT_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building import runtime")?;
    runtime.block_on(async { import_once().await })
}

async fn import_once() -> Result<()> {
    const TITLE: &str = "Companion Capture → local iMessage → CI-Server (one-shot import)";
    println!("{TITLE}");
    println!("{}\n", "=".repeat(TITLE.chars().count()));

    let db_path = chat_db_path().context("locating chat.db")?;
    // `NotRequired` is not a permission problem — it means Messages has never
    // run on this Mac, so there is no chat.db to be denied access to. Sending
    // that user to the Full Disk Access pane would have them grant a
    // permission that changes nothing.
    match permissions::full_disk_access_status() {
        PermissionStatus::Granted => {}
        PermissionStatus::NotRequired => {
            bail!(
                "no Messages database on this Mac ({}) — nothing to import.\nThis is not a permissions problem: Messages has never stored history here.",
                db_path.display()
            );
        }
        PermissionStatus::Denied => {
            bail!(
                "Full Disk Access not granted for this process. Grant it in\nSystem Settings → Privacy & Security → Full Disk Access, then relaunch\nand retry:\n  {}",
                permissions::full_disk_access_settings_url()
            );
        }
    }

    let portal_url = std::env::var("CI_SERVER_URL").unwrap_or_default();
    let api_key = std::env::var("CI_SERVER_API_KEY").unwrap_or_default();
    if portal_url.trim().is_empty() || api_key.trim().is_empty() {
        bail!(
            "set CI_SERVER_URL and CI_SERVER_API_KEY, e.g.\n  CI_SERVER_URL=http://localhost:55528 CI_SERVER_API_KEY=... \\\n    cargo run -- --import-imessage-once"
        );
    }

    let limit = limit();
    println!("Database   : {}", db_path.display());
    println!("CI-Server  : {portal_url}");
    println!("Fetching   : up to {limit} message(s), oldest first\n");

    let config = AppConfig {
        server: crate::config::ServerConfig {
            portal_url,
            applet_auth_token: Some(api_key),
            ..Default::default()
        },
        identity: crate::config::IdentityConfig {
            device_id: Some(Uuid::nil()),
            user_id: None,
        },
        ..Default::default()
    };

    let conn = reader::open_readonly(&db_path).context("opening chat.db")?;
    let preview = reader::read_messages(&conn, None, limit).context("reading messages")?;
    drop(conn); // release before the async pass reopens it on its own blocking thread

    if preview.rows.is_empty() {
        println!("chat.db returned 0 messages for this range — nothing to import.");
        return Ok(());
    }
    println!("About to ingest {} message(s):", preview.rows.len());
    for row in &preview.rows {
        println!("  - {}", describe(row));
    }
    println!();

    let outcome = poll_once(&db_path, &config, Uuid::nil(), None, limit)
        .await
        .context("running the poll+ingest pass")?;

    println!("Result");
    println!("------");
    println!("fetched  : {}", outcome.fetched);
    println!("ingested : {}", outcome.ingested);
    if outcome.skipped_reactions > 0 {
        println!(
            "reactions: {} tapback(s) skipped — they quote the message they react to",
            outcome.skipped_reactions
        );
    }
    if outcome.skipped_invalid_timestamp > 0 {
        println!(
            "dropped  : {} row(s) with an unreadable message.date",
            outcome.skipped_invalid_timestamp
        );
    }
    println!(
        "advanced to: {}",
        outcome
            .advanced_to
            .map(|c| format!("date_raw={} rowid={}", c.date_raw, c.rowid))
            .unwrap_or_else(|| "(none — first message failed)".to_string())
    );
    // Skipped reactions and unreadable timestamps are decisions, not failures,
    // so they must not be reported as "did not make it" — that reads as an
    // outage and sends someone looking for a warning that was never logged.
    let accounted =
        outcome.ingested + outcome.skipped_reactions + outcome.skipped_invalid_timestamp;
    if accounted < outcome.fetched {
        println!(
            "\n{} message(s) did not make it — see the warnings above for why.",
            outcome.fetched - accounted
        );
    } else {
        println!("\nEvery fetched message was accepted, skipped or dropped as reported above.");
    }

    Ok(())
}
