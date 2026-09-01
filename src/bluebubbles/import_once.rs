//! `--import-bluebubbles-once` — runs ONE real poll-and-ingest pass against a
//! live BlueBubbles server and a live CI-Server, through the *exact* code
//! path the background lane uses (`poll_once` →
//! `server_ingest::ingest_bluebubbles_message` → `POST /v1/events`). This is
//! a demonstration that the pipeline works end-to-end, not a simulation of
//! it and not a bulk backfill tool — see `LIMIT` below.
//!
//! Run with:
//! ```text
//! BLUEBUBBLES_HOST=192.168.0.4 BLUEBUBBLES_PORT=1234 BLUEBUBBLES_PASSWORD=... \
//!   CI_SERVER_URL=http://localhost:55528 CI_SERVER_API_KEY=... \
//!   cargo run -- --import-bluebubbles-once
//! ```
//!
//! Same reasoning as `check.rs` for reading everything from the environment
//! rather than `ConfigManager`: this prototype has no settings-UI panel to
//! *put* either credential in the persisted config yet, and that config
//! resolves to the same on-disk file the user's real, installed Companion
//! Capture uses — a one-shot import command has no business writing there,
//! and CI_SERVER_API_KEY in particular should never land in a file this
//! process didn't get explicit instructions to write.
//!
//! Ignores any saved sync cursor: every run starts from `after: None`
//! (BlueBubbles' oldest messages) unless `BLUEBUBBLES_IMPORT_AFTER_MS` is
//! set, so re-running this is safe to repeat — CI-Server's own dedup
//! (`source_id = "bluebubbles:<guid>"`) is what actually prevents duplicates
//! on a second run, same as the background lane relies on.

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::config::AppConfig;

use super::client::{BlueBubblesClient, BlueBubblesClientConfig};
use super::{describe, poll_once};

/// Small and fixed, not configurable up: this command exists to *show* the
/// pipeline moving real data, not to hoover up someone's message history in
/// one shot. `run` (the real background lane) is what does ongoing sync.
const LIMIT: u32 = 5;

pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building import runtime")?;
    runtime.block_on(async { import_once().await })
}

async fn import_once() -> Result<()> {
    const TITLE: &str = "Companion Capture → BlueBubbles → CI-Server (one-shot import)";
    println!("{TITLE}");
    println!("{}\n", "=".repeat(TITLE.chars().count()));

    let host = std::env::var("BLUEBUBBLES_HOST").unwrap_or_default();
    let port: u16 = std::env::var("BLUEBUBBLES_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1234);
    let use_https = std::env::var("BLUEBUBBLES_HTTPS").is_ok_and(|v| v == "1" || v == "true");
    let password = std::env::var("BLUEBUBBLES_PASSWORD").unwrap_or_default();
    let after: Option<i64> = std::env::var("BLUEBUBBLES_IMPORT_AFTER_MS")
        .ok()
        .and_then(|v| v.parse().ok());

    let portal_url = std::env::var("CI_SERVER_URL").unwrap_or_default();
    let api_key = std::env::var("CI_SERVER_API_KEY").unwrap_or_default();

    if host.trim().is_empty() || portal_url.trim().is_empty() || api_key.trim().is_empty() {
        bail!(
            "set BLUEBUBBLES_HOST, BLUEBUBBLES_PASSWORD, CI_SERVER_URL and CI_SERVER_API_KEY, e.g.\n  BLUEBUBBLES_HOST=192.168.0.4 BLUEBUBBLES_PORT=1234 BLUEBUBBLES_PASSWORD=... \\\n    CI_SERVER_URL=http://localhost:55528 CI_SERVER_API_KEY=... \\\n    cargo run -- --import-bluebubbles-once"
        );
    }

    println!(
        "BlueBubbles: {}://{host}:{port}",
        if use_https { "https" } else { "http" }
    );
    println!("CI-Server  : {portal_url}");
    println!("Fetching   : up to {LIMIT} message(s), after={after:?}\n");

    let config = AppConfig {
        server: crate::config::ServerConfig {
            portal_url,
            applet_auth_token: Some(api_key),
            ..Default::default()
        },
        identity: crate::config::IdentityConfig {
            // No real device row for this one-shot run — CI-Server's
            // ingest endpoint accepts a nil device id the same way the
            // vizier lane already tolerates for API-key installs (see
            // `capture_vizier`'s fallback in worker.rs).
            device_id: Some(Uuid::nil()),
            user_id: None,
        },
        ..Default::default()
    };

    let client = BlueBubblesClient::new(BlueBubblesClientConfig {
        host,
        port,
        use_https,
        password,
    })?;

    // Fetch first so we can print what we're about to send, before sending it.
    let preview = client
        .query_messages(after, LIMIT)
        .await
        .context("querying BlueBubbles")?;
    if preview.is_empty() {
        println!("BlueBubbles returned 0 messages for this range — nothing to import.");
        return Ok(());
    }
    println!("About to ingest {} message(s):", preview.len());
    for message in &preview {
        println!("  - {}", describe(message));
    }
    println!();

    let outcome = poll_once(&client, &config, Uuid::nil(), after, LIMIT)
        .await
        .context("running the poll+ingest pass")?;

    println!("Result");
    println!("------");
    println!("fetched  : {}", outcome.fetched);
    println!("ingested : {}", outcome.ingested);
    if outcome.dropped_invalid_timestamp > 0 {
        println!(
            "dropped  : {} message(s) with no usable dateCreated (not retried — see the lane docs)",
            outcome.dropped_invalid_timestamp
        );
    }
    println!(
        "advanced to dateCreated: {}",
        outcome
            .advanced_to
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(none — first message failed)".to_string())
    );
    if outcome.ingested < outcome.fetched {
        println!(
            "\n{} message(s) did not make it — see the warnings above for why.",
            outcome.fetched - outcome.ingested
        );
    } else {
        println!("\nAll fetched messages were accepted by CI-Server.");
    }

    Ok(())
}
