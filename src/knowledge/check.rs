//! `--check-knowledge` — a live, read-only probe against this Mac's own
//! `~/Library/Application Support/Knowledge/knowledgeC.db`. No env vars
//! needed — there is exactly one store to point at — and nothing is sent
//! anywhere: this reports what the lane *would* ship under the persisted
//! config, so the per-stream floors and the opt-in streams can be tuned
//! before the lane is switched on.
//!
//! Run with:
//! ```text
//! cargo run -- --check-knowledge
//! ```
//!
//! Full Disk Access is inherited from the launching process: run from a
//! terminal app, it is the *terminal* that needs the grant, not the
//! Companion Capture bundle — which is why a ✓ here does not prove the
//! tray app can read the store, and a ✗ here may just be the terminal.

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite::types::Value;

use crate::config::{AppConfig, ConfigManager};
use crate::permissions::{self, PermissionStatus};

use super::{db_access_status, db_path, describe, facts, reader};

/// The persisted app config, read without the `load_or_default` side effect
/// of writing a default file — this probe is read-only end to end.
fn load_persisted_config() -> Result<AppConfig> {
    let manager = ConfigManager::new().context("locating config")?;
    if !manager.path().exists() {
        return Ok(AppConfig::default());
    }
    let raw = std::fs::read_to_string(manager.path())
        .with_context(|| format!("reading {}", manager.path().display()))?;
    serde_json::from_str(&raw).context("parsing config")
}

/// Per-stream numbers the table prints. Everything comes from one aggregate
/// query per stream, filtered the way the lane filters (`include_synced`)
/// so the "would ship" column matches what the crawl would post.
struct StreamStats {
    rows: i64,
    non_zero: i64,
    would_ship: i64,
    oldest_start: Option<f64>,
    newest_start: Option<f64>,
}

pub fn run() -> Result<()> {
    const TITLE: &str = "Companion Capture → macOS Knowledge store (knowledgeC.db) check";
    println!("{TITLE}");
    println!("{}\n", "=".repeat(TITLE.chars().count()));

    let config = load_persisted_config().unwrap_or_else(|err| {
        println!("(persisted config unreadable — using defaults: {err:#})\n");
        AppConfig::default()
    });
    let knowledge = &config.knowledge;

    let db_path = db_path().context("locating knowledgeC.db")?;
    println!("Database   : {}", db_path.display());
    println!("Exists     : {}", db_path.exists());

    let fda = db_access_status();
    println!(
        "Full Disk Access: {}",
        match fda {
            PermissionStatus::Granted => "✓ granted (for this process)",
            PermissionStatus::Denied => "✗ not granted",
            PermissionStatus::NotRequired => "n/a (no Knowledge store on this system)",
        }
    );
    match fda {
        PermissionStatus::Granted => {}
        PermissionStatus::NotRequired => {
            println!(
                "\nNothing to read: this system has no Knowledge store, so the lane has no\nsource. This is not a permissions problem."
            );
            return Ok(());
        }
        PermissionStatus::Denied => {
            println!(
                "\nGrant it in System Settings → Privacy & Security → Full Disk Access\n(add or enable the app/binary running this — from a terminal that is the\nterminal app itself), then relaunch and retry:\n  {}",
                permissions::full_disk_access_settings_url()
            );
            return Ok(());
        }
    }

    println!("\nLane config");
    println!("-----------");
    println!("enabled              : {}", knowledge.enabled);
    println!(
        "effective streams    : {:?}",
        facts::effective_streams(knowledge)
    );
    println!(
        "include_synced_devices: {}",
        knowledge.include_synced_devices
    );
    println!(
        "min_session_secs     : {} (app + web sessions)",
        knowledge.min_session_secs
    );
    println!("backfill_days        : {}", knowledge.backfill_days);
    println!("daily_digest         : {}", knowledge.daily_digest);

    println!("\nLive read");
    println!("---------");

    let conn = reader::open_readonly(&db_path).context("opening knowledgeC.db")?;
    let store_uuid = reader::read_store_uuid(&conn).context("reading store uuid")?;
    println!(
        "Store UUID : {}",
        store_uuid
            .as_deref()
            .unwrap_or("(none — no Z_METADATA row)")
    );
    let max_pk = reader::max_pk(&conn).context("reading MAX(Z_PK)")?;
    println!(
        "MAX(Z_PK)  : {}",
        max_pk
            .map(|p| p.to_string())
            .unwrap_or_else(|| "(empty store)".to_owned())
    );
    let floor = reader::first_run_floor(&conn, knowledge.backfill_days)
        .context("computing first-run floor")?;
    println!("First-run floor (Z_PK the crawl starts after): {floor}");

    let effective = facts::effective_streams(knowledge);
    print_stream_table(&conn, knowledge, &effective)?;

    println!("\nNewest 5 rows the lane would read (shape only — never a URL or domain):");
    let newest = newest_rows(&conn, &effective, knowledge.include_synced_devices, 5)?;
    if newest.rows.is_empty() {
        println!("  (none — no rows in the effective streams)");
    }
    for row in &newest.rows {
        println!("  - {}", describe(row, knowledge));
    }
    if newest.skipped_invalid_timestamp > 0 {
        println!(
            "  dropped {} row(s) whose timestamps are not representable",
            newest.skipped_invalid_timestamp
        );
    }

    print_warnings(knowledge, &effective);

    println!(
        "\nThis probe never sends anything to CI-Server — it only reads knowledgeC.db.\nThe polling lane (crate::knowledge::run) is what forwards events on, once\nthe lane is enabled in the persisted app config."
    );
    Ok(())
}

/// One aggregate per known stream, under the lane's own floor for it, so
/// the "would ship/day" column is the number the crawl would actually post
/// (before the sensitive-bundle and user-exclusion rules, which need the
/// per-row subject and are applied by the mapping, not here).
fn print_stream_table(
    conn: &Connection,
    config: &crate::config::KnowledgeConfig,
    effective: &[String],
) -> Result<()> {
    println!("\nPer-stream (rows the lane would read under the current config):");
    println!(
        "  {:<24} {:>7} {:>10} {:>12} {:>11} {:>11}  state",
        "stream", "rows", "nonzero/d", "would ship/d", "oldest", "newest"
    );
    for stream in facts::KNOWN_STREAMS {
        let stats = stream_stats(conn, stream, config)?;
        let days = span_days(stats.oldest_start, stats.newest_start);
        let per_day = |n: i64| -> String {
            if days > 0.0 {
                format!("{:.0}", n as f64 / days)
            } else {
                "-".to_owned()
            }
        };
        let state = if effective.iter().any(|s| s == stream) {
            "enabled"
        } else if facts::OPT_IN_STREAMS.contains(stream) {
            "disabled (opt-in)"
        } else {
            "disabled"
        };
        println!(
            "  {:<24} {:>7} {:>10} {:>12} {:>11} {:>11}  {}",
            stream,
            stats.rows,
            per_day(stats.non_zero),
            per_day(stats.would_ship),
            date_only(stats.oldest_start),
            date_only(stats.newest_start),
            state
        );
    }
    println!(
        "  (would ship/d applies the duration floors only; password-manager,\n   sign-in-domain and user exclusions are applied per row by the mapping)"
    );
    Ok(())
}

/// Aggregate query for one stream. All-anonymous placeholders bound in
/// textual order (`min_secs`, `stream`, `include_synced`) — never `?N` mixed
/// with `?`, per the rusqlite rule the reader documents.
fn stream_stats(
    conn: &Connection,
    stream: &str,
    config: &crate::config::KnowledgeConfig,
) -> Result<StreamStats> {
    // A point stream ships every row; an interval stream ships rows at or
    // above its floor; the display stream ships only "on" rows.
    let would_ship_expr = match facts::min_duration_secs(stream, config) {
        None => "1".to_owned(),
        Some(_) if stream == facts::STREAM_DISPLAY => {
            "(o.ZVALUEINTEGER = 1 AND o.ZENDDATE - o.ZSTARTDATE >= ?)".to_owned()
        }
        Some(_) => "(o.ZENDDATE - o.ZSTARTDATE >= ?)".to_owned(),
    };
    let sql = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(o.ZENDDATE > o.ZSTARTDATE), 0),
                COALESCE(SUM({would_ship_expr}), 0),
                MIN(o.ZSTARTDATE), MAX(o.ZSTARTDATE)
         FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
         WHERE o.ZSTREAMNAME = ? AND (? OR s.ZDEVICEID IS NULL)"
    );
    let mut values: Vec<Value> = Vec::with_capacity(3);
    if let Some(min) = facts::min_duration_secs(stream, config) {
        values.push(Value::Integer(min as i64));
    }
    values.push(Value::Text(stream.to_owned()));
    values.push(Value::Integer(i64::from(config.include_synced_devices)));
    let mut stmt = conn.prepare(&sql).context("preparing stream stats")?;
    debug_assert_eq!(stmt.parameter_count(), values.len());
    stmt.query_row(rusqlite::params_from_iter(values), |row| {
        Ok(StreamStats {
            rows: row.get(0)?,
            non_zero: row.get(1)?,
            would_ship: row.get(2)?,
            oldest_start: row.get(3)?,
            newest_start: row.get(4)?,
        })
    })
    .with_context(|| format!("aggregating {stream}"))
}

/// The newest `limit` rows in the effective streams, via the lane's own
/// `read_events` (so the same joins, columns and timestamp guard apply):
/// find the `Z_PK` `limit` rows from the top and read ascending from there.
fn newest_rows(
    conn: &Connection,
    streams: &[String],
    include_synced: bool,
    limit: u32,
) -> Result<reader::EventPage> {
    if streams.is_empty() {
        return Ok(reader::EventPage::default());
    }
    let placeholders = vec!["?"; streams.len()].join(",");
    let sql = format!(
        "SELECT o.Z_PK FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
         WHERE o.ZSTREAMNAME IN ({placeholders}) AND (? OR s.ZDEVICEID IS NULL)
         ORDER BY o.Z_PK DESC LIMIT 1 OFFSET ?"
    );
    let mut values: Vec<Value> = streams.iter().map(|s| Value::Text(s.clone())).collect();
    values.push(Value::Integer(i64::from(include_synced)));
    values.push(Value::Integer(i64::from(limit.saturating_sub(1))));
    let mut stmt = conn.prepare(&sql).context("preparing newest-rows probe")?;
    debug_assert_eq!(stmt.parameter_count(), values.len());
    let after_pk: i64 = stmt
        .query_row(rusqlite::params_from_iter(values), |row| {
            row.get::<_, i64>(0)
        })
        .map(|pk| pk - 1)
        .unwrap_or(0);
    reader::read_events(conn, after_pk, streams, include_synced, limit)
        .context("reading newest rows")
}

fn print_warnings(config: &crate::config::KnowledgeConfig, effective: &[String]) {
    let mut warnings: Vec<String> = Vec::new();
    if effective.is_empty() {
        warnings.push(
            "no effective streams — every default is in disabled_streams and no extra_streams is known; the lane would read nothing".to_owned(),
        );
    }
    if effective.iter().any(|s| s == facts::STREAM_INTENTS) && !config.include_synced_devices {
        warnings.push(
            "/app/intents is enabled but include_synced_devices is false — every intent row is donated by an iPhone/iPad, so nothing from that stream will ship".to_owned(),
        );
    }
    for name in config
        .extra_streams
        .iter()
        .chain(config.disabled_streams.iter())
    {
        if !facts::KNOWN_STREAMS.contains(&name.as_str()) {
            warnings.push(format!(
                "stream name {name:?} in the config is not one this lane knows; it is ignored"
            ));
        }
    }
    warnings.push(
        "Full Disk Access is inherited from the launching process: a ✓ above from a terminal proves the terminal can read the store, not necessarily the tray app".to_owned(),
    );
    println!("\nWarnings:");
    for warning in warnings {
        println!("  ! {warning}");
    }
}

fn span_days(oldest: Option<f64>, newest: Option<f64>) -> f64 {
    match (oldest, newest) {
        (Some(a), Some(b)) if b > a => ((b - a) / 86_400.0).max(1.0),
        (Some(_), Some(_)) => 1.0,
        _ => 0.0,
    }
}

fn date_only(cocoa_secs: Option<f64>) -> String {
    cocoa_secs
        .and_then(reader::cocoa_secs_to_unix_ms)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".to_owned())
}
