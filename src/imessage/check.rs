//! `--check-imessage` — a live, read-only probe against this Mac's own
//! `~/Library/Messages/chat.db`. No env vars needed (unlike `bluebubbles`'s
//! `check.rs`) — there is exactly one database to point at.
//!
//! Run with:
//! ```text
//! cargo run -- --check-imessage
//! ```

use anyhow::{Context, Result};

use crate::permissions::{self, PermissionStatus};

use super::{chat_db_path, describe, reader};

pub fn run() -> Result<()> {
    const TITLE: &str = "Companion Capture → local iMessage (chat.db) check";
    println!("{TITLE}");
    println!("{}\n", "=".repeat(TITLE.chars().count()));

    let db_path = chat_db_path().context("locating chat.db")?;
    println!("Database   : {}", db_path.display());
    println!("Exists     : {}", db_path.exists());

    let fda = permissions::full_disk_access_status();
    println!(
        "Full Disk Access: {}",
        match fda {
            PermissionStatus::Granted => "✓ granted",
            PermissionStatus::Denied => "✗ not granted",
            PermissionStatus::NotRequired => "n/a (no chat.db on this Mac)",
        }
    );

    if fda != PermissionStatus::Granted {
        println!(
            "\nGrant it in System Settings → Privacy & Security → Full Disk Access\n(add or enable the app/binary running this), then relaunch and retry:\n  {}",
            permissions::full_disk_access_settings_url()
        );
        return Ok(());
    }

    println!("\nLive read");
    println!("---------");

    let conn = reader::open_readonly(&db_path).context("opening chat.db")?;
    let page = reader::read_messages(&conn, None, 5).context("reading messages")?;

    println!(
        "Read {} message(s) (oldest, limit 5, no cursor):",
        page.rows.len()
    );
    for row in &page.rows {
        // `describe` reports shape only — see its doc comment.
        println!("  - {}", describe(row));
    }
    if page.skipped_invalid_timestamp > 0 {
        println!(
            "Dropped {} row(s) whose message.date is not a representable timestamp.",
            page.skipped_invalid_timestamp
        );
    }

    println!(
        "\nThis probe never sends anything to CI-Server — it only reads chat.db.\nThe polling lane (crate::imessage::run) is what forwards messages on, once\nthe lane is enabled in the persisted app config."
    );
    Ok(())
}
