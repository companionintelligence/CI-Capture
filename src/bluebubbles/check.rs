//! `--check-bluebubbles` — a live, read-only probe against a BlueBubbles
//! server, independent of the persisted app config.
//!
//! Run with:
//! ```text
//! BLUEBUBBLES_HOST=192.168.0.4 BLUEBUBBLES_PORT=1234 BLUEBUBBLES_PASSWORD=... \
//!   cargo run -- --check-bluebubbles
//! ```
//!
//! Deliberately reads from the environment rather than `ConfigManager` (as
//! `--check-server` does): this prototype has no settings-UI panel yet to
//! *put* BlueBubbles credentials in the persisted config, and
//! `ConfigManager` resolves to the same on-disk file the user's real,
//! installed Companion Capture uses (`directories::ProjectDirs` is keyed by
//! bundle identifier, not by build/checkout) — a probe command has no
//! business writing there. It never mutates anything on the BlueBubbles
//! server either.

use anyhow::{Context, Result, bail};

use super::client::{BlueBubblesClient, BlueBubblesClientConfig};
use super::describe;

pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building probe runtime")?;
    runtime.block_on(async { report().await })
}

async fn report() -> Result<()> {
    const TITLE: &str = "Companion Capture → BlueBubbles connection check";
    println!("{TITLE}");
    println!("{}\n", "=".repeat(TITLE.chars().count()));

    let host = std::env::var("BLUEBUBBLES_HOST").unwrap_or_default();
    let port: u16 = std::env::var("BLUEBUBBLES_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1234);
    let use_https = std::env::var("BLUEBUBBLES_HTTPS").is_ok_and(|v| v == "1" || v == "true");
    let password = std::env::var("BLUEBUBBLES_PASSWORD").unwrap_or_default();

    if host.trim().is_empty() {
        bail!(
            "set BLUEBUBBLES_HOST (and BLUEBUBBLES_PORT, BLUEBUBBLES_PASSWORD) and re-run — e.g.\n  BLUEBUBBLES_HOST=192.168.0.4 BLUEBUBBLES_PORT=1234 BLUEBUBBLES_PASSWORD=... cargo run -- --check-bluebubbles"
        );
    }

    println!(
        "Server     : {}://{host}:{port}",
        if use_https { "https" } else { "http" }
    );
    println!(
        "Password   : {}",
        if password.is_empty() {
            "✗ not set (BLUEBUBBLES_PASSWORD)"
        } else {
            "set"
        }
    );

    let client = BlueBubblesClient::new(BlueBubblesClientConfig {
        host,
        port,
        use_https,
        password,
    })?;

    println!("\nLive probe");
    println!("----------");

    match client.ping().await {
        Ok(info) => {
            println!("GET /server/info       : ✓ reachable");
            if let Some(version) = info.get("server_version").and_then(|v| v.as_str()) {
                println!("  server_version       : {version}");
            }
            if let Some(private_api) = info.get("private_api").and_then(|v| v.as_bool()) {
                println!(
                    "  private_api          : {} — {}",
                    private_api,
                    if private_api {
                        "sending is possible"
                    } else {
                        "read-only (Private API not enabled on that server)"
                    }
                );
            }
        }
        Err(err) => {
            println!("GET /server/info       : ✗ {err}");
            return Ok(());
        }
    }

    match client.query_messages(None, 5).await {
        Ok(messages) => {
            println!(
                "POST /message/query    : ✓ {} message(s) in the oldest page (limit 5, no cursor)",
                messages.len()
            );
            for message in &messages {
                // `describe` reports shape (chat, sender, lengths), never
                // message text — see its doc comment.
                println!("  - {}", describe(message));
            }
        }
        Err(err) => println!("POST /message/query    : ✗ {err}"),
    }

    println!(
        "\nThis probe never sends anything to CI-Server — it only talks to BlueBubbles.\nThe polling lane (crate::bluebubbles::run) is what forwards messages on, once\na host/port/password are saved in the persisted app config."
    );
    Ok(())
}
