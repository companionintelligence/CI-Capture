//! `--import-knowledge-once` — runs ONE real read-and-ship pass against this
//! Mac's own `knowledgeC.db`, through the exact code path the background
//! lane uses (`drain_once` → `facts::to_capture_event` →
//! `server_ingest::post_capture_events` → `POST /v1/events`). Mirrors
//! `imessage::import_once` — same reasoning throughout — with two extra
//! modes this lane needs because no CI-Server is configured on the machine
//! it was built on:
//!
//! ```text
//! # Dry run: write the exact CaptureEvent JSON lines the lane would post.
//! # No credentials, no network — bypasses `is_configured` entirely.
//! KNOWLEDGE_IMPORT_LIMIT=300 KNOWLEDGE_EXPORT_JSONL=/tmp/knowledge-events.jsonl \
//!   cargo run -- --import-knowledge-once
//!
//! # Live: post up to KNOWLEDGE_IMPORT_LIMIT rows (default 25) to a real server.
//! CI_SERVER_URL=http://localhost:55528 CI_SERVER_API_KEY=... \
//!   cargo run -- --import-knowledge-once
//!
//! # Replay a park file (knowledge-rejected.jsonl) against a real server.
//! # CI-Server dedups on source_id, so replaying twice is harmless.
//! KNOWLEDGE_REPLAY_REJECTED=~/Library/Application\ Support/.../knowledge-rejected.jsonl \
//!   CI_SERVER_URL=... CI_SERVER_API_KEY=... cargo run -- --import-knowledge-once
//! ```
//!
//! Reads CI-Server credentials from the environment, not the persisted app
//! config, for the same reason `bluebubbles::import_once` does; the
//! *lane* config (streams, floors, exclusions) does come from the persisted
//! config so the run reflects what the tray app would ship. Requires Full
//! Disk Access already granted — nothing here can request it.
//!
//! Small by default (25 rows) — a demonstration that the pipeline works,
//! not a bulk backfill tool; the background lane (`knowledge::run`) does
//! the ongoing crawl. The live mode uses a throw-away cursor so it never
//! moves the lane's real one.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Offset as _;
use serde_json::Value;
use uuid::Uuid;

use crate::config::{AppConfig, ConfigManager};
use crate::permissions::{self, PermissionStatus};
use crate::server_ingest::{self, CaptureEvent};

use super::{Cursor, db_access_status, db_path, describe, drain_once, facts, reader};

const DEFAULT_LIMIT: u32 = 25;

/// Events per replay request — the lane's own batch size.
const REPLAY_BATCH: usize = 200;

fn limit() -> u32 {
    std::env::var("KNOWLEDGE_IMPORT_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building import runtime")?;
    runtime.block_on(async { import_once().await })
}

/// The persisted lane config (streams, floors, exclusions), without the
/// `load_or_default` side effect of writing a default file.
fn persisted_knowledge_config() -> crate::config::KnowledgeConfig {
    let Ok(manager) = ConfigManager::new() else {
        return Default::default();
    };
    std::fs::read_to_string(manager.path())
        .ok()
        .and_then(|raw| serde_json::from_str::<AppConfig>(&raw).ok())
        .map(|cfg| cfg.knowledge)
        .unwrap_or_default()
}

/// Server credentials from the environment, as `imessage::import_once`.
fn server_config_from_env() -> Result<AppConfig> {
    let portal_url = std::env::var("CI_SERVER_URL").unwrap_or_default();
    let api_key = std::env::var("CI_SERVER_API_KEY").unwrap_or_default();
    if portal_url.trim().is_empty() || api_key.trim().is_empty() {
        bail!(
            "set CI_SERVER_URL and CI_SERVER_API_KEY, e.g.\n  CI_SERVER_URL=http://localhost:55528 CI_SERVER_API_KEY=... \\\n    cargo run -- --import-knowledge-once\n(or set KNOWLEDGE_EXPORT_JSONL=<path> for a dry run that needs neither)"
        );
    }
    println!("CI-Server  : {portal_url}");
    Ok(AppConfig {
        server: crate::config::ServerConfig {
            portal_url,
            applet_auth_token: Some(api_key),
            ..Default::default()
        },
        identity: crate::config::IdentityConfig {
            device_id: Some(Uuid::nil()),
            user_id: None,
        },
        knowledge: crate::config::KnowledgeConfig {
            enabled: true,
            ..persisted_knowledge_config()
        },
        ..Default::default()
    })
}

async fn import_once() -> Result<()> {
    const TITLE: &str = "Companion Capture → macOS Knowledge store → CI-Server (one-shot import)";
    println!("{TITLE}");
    println!("{}\n", "=".repeat(TITLE.chars().count()));

    // Replay needs no store access at all — the park file is the source.
    if let Some(park_file) = env_path("KNOWLEDGE_REPLAY_REJECTED") {
        let config = server_config_from_env()?;
        return replay_rejected(&config, &park_file).await;
    }

    let db_path = db_path().context("locating knowledgeC.db")?;
    match db_access_status() {
        PermissionStatus::Granted => {}
        PermissionStatus::NotRequired => {
            bail!(
                "no Knowledge store on this system ({}) — nothing to import.\nThis is not a permissions problem.",
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

    let limit = limit();
    println!("Database   : {}", db_path.display());

    if let Some(export_path) = env_path("KNOWLEDGE_EXPORT_JSONL") {
        let knowledge = crate::config::KnowledgeConfig {
            enabled: true,
            ..persisted_knowledge_config()
        };
        return export_jsonl(&db_path, &knowledge, limit, &export_path);
    }

    let config = server_config_from_env()?;
    println!("Fetching   : up to {limit} row(s) after the first-run floor\n");

    // Preview through the same reader the lane uses.
    let streams = facts::effective_streams(&config.knowledge);
    let conn = reader::open_readonly(&db_path).context("opening knowledgeC.db")?;
    let floor = reader::first_run_floor(&conn, config.knowledge.backfill_days)
        .context("computing first-run floor")?;
    let preview = reader::read_events(
        &conn,
        floor,
        &streams,
        config.knowledge.include_synced_devices,
        limit,
    )
    .context("reading rows")?;
    drop(conn); // release before the async pass reopens it on its own blocking thread

    if preview.rows_returned == 0 {
        println!("knowledgeC.db returned 0 rows after Z_PK {floor} — nothing to import.");
        return Ok(());
    }
    println!(
        "Read {} row(s) to map (short/sensitive/system skips are reported in Result):",
        preview.rows.len()
    );
    for row in &preview.rows {
        println!("  - {}", describe(row, &config.knowledge));
    }
    println!();

    // A throw-away cursor and park file in the temp dir: this one-shot must
    // never move the lane's real cursor or pollute its park file. A zero
    // budget means "one acknowledged chunk, then stop", and the chunk is
    // sized to the whole page, so exactly `limit` rows are considered.
    let scratch = std::env::temp_dir().join(format!(
        "ci-capture-knowledge-import-{}",
        std::process::id()
    ));
    let cursor_path = scratch.join("cursor.json");
    let rejected_path = scratch.join("knowledge-rejected.jsonl");
    let mut cursor = Cursor::default();
    let outcome = drain_once(
        &db_path,
        &config,
        Uuid::nil(),
        &mut cursor,
        &cursor_path,
        &rejected_path,
        limit,
        limit as usize,
        Duration::ZERO,
    )
    .await
    .context("running the drain pass")?;

    println!("Result");
    println!("------");
    println!("rows returned : {}", outcome.rows_returned);
    println!("fetched       : {}", outcome.fetched);
    println!("ingested      : {}", outcome.ingested);
    println!("duplicate     : {}", outcome.duplicate);
    println!("skipped short : {}", outcome.skipped_short);
    println!("skipped sens. : {}", outcome.skipped_sensitive);
    println!("skipped system: {}", outcome.skipped_system);
    if outcome.skipped_invalid_timestamp > 0 {
        println!(
            "dropped       : {} row(s) with an unrepresentable timestamp",
            outcome.skipped_invalid_timestamp
        );
    }
    if outcome.parked > 0 {
        println!(
            "parked        : {} event(s) → {}",
            outcome.parked,
            rejected_path.display()
        );
    }
    println!(
        "advanced to   : {}",
        outcome
            .advanced_to
            .map(|pk| format!("Z_PK {pk}"))
            .unwrap_or_else(|| "(none — the first batch failed)".to_owned())
    );
    if outcome.parked == 0 {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    Ok(())
}

/// Dry run: map rows exactly as the lane would and write one `CaptureEvent`
/// per line — the same bytes `post_capture_events` would serialize, minus
/// the envelope. Nothing is posted and no credential is read.
fn export_jsonl(
    db_path: &std::path::Path,
    knowledge: &crate::config::KnowledgeConfig,
    limit: u32,
    export_path: &std::path::Path,
) -> Result<()> {
    println!("Mode       : dry run → {}", export_path.display());
    let streams = facts::effective_streams(knowledge);
    println!("Streams    : {streams:?}");
    println!("Fetching   : up to {limit} row(s) after the first-run floor\n");

    let conn = reader::open_readonly(db_path).context("opening knowledgeC.db")?;
    let store_uuid = reader::read_store_uuid(&conn).context("reading store uuid")?;
    let floor =
        reader::first_run_floor(&conn, knowledge.backfill_days).context("computing floor")?;
    let page = reader::read_events(
        &conn,
        floor,
        &streams,
        knowledge.include_synced_devices,
        limit,
    )
    .context("reading rows")?;
    drop(conn);

    let ctx = facts::FactContext {
        config: knowledge,
        device_id: Uuid::nil(),
        store_uuid: store_uuid.as_deref(),
        web_effective: streams.iter().any(|s| s == facts::STREAM_WEB_USAGE),
    };

    if let Some(parent) = export_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(export_path)
            .with_context(|| format!("creating {}", export_path.display()))?,
    );
    let (mut written, mut short, mut sensitive, mut system) = (0usize, 0usize, 0usize, 0usize);
    for row in &page.rows {
        match facts::to_capture_event(row, &ctx) {
            Ok(event) => {
                serde_json::to_writer(&mut out, &event).context("serializing event")?;
                out.write_all(b"\n").context("writing event")?;
                written += 1;
            }
            Err(facts::Skip::Short) => short += 1,
            Err(facts::Skip::Sensitive) => sensitive += 1,
            Err(facts::Skip::System) => system += 1,
        }
    }
    let digest_written = if std::env::var("KNOWLEDGE_DIGEST").is_ok_and(|v| v == "1") {
        export_newest_digest(db_path, knowledge, &ctx, &mut out)?
    } else {
        0
    };
    out.flush().context("flushing export")?;

    println!("Result");
    println!("------");
    println!("rows returned : {}", page.rows_returned);
    println!("events written: {written}");
    if digest_written > 0 {
        println!("digest events : {digest_written}");
    }
    println!("skipped short : {short}");
    println!("skipped sens. : {sensitive}");
    println!("skipped system: {system}");
    if page.skipped_invalid_timestamp > 0 {
        println!(
            "dropped       : {} row(s) with an unrepresentable timestamp",
            page.skipped_invalid_timestamp
        );
    }
    println!(
        "\nNothing was sent anywhere. Inspect {} with jq.",
        export_path.display()
    );
    Ok(())
}

/// `KNOWLEDGE_DIGEST=1`: append the newest digestable day's
/// `knowledge_daily_*` events to the dry-run export, exactly as the lane
/// would build them — same gates ([`super::plan_digest_days`] with the
/// backfill window taken from this run's clock, as a first tick would), same
/// builder, same suppression.
/// Returns how many events were written.
///
/// "Now" is the UTC clock placed in the zone of the store's newest
/// `/app/usage` row rather than the host's `chrono::Local`: the lane keeps
/// `Local` to its one call site in `drain_once`, and the store's own
/// offset is what the digest buckets by anyway.
fn export_newest_digest(
    db_path: &std::path::Path,
    knowledge: &crate::config::KnowledgeConfig,
    ctx: &facts::FactContext<'_>,
    out: &mut impl std::io::Write,
) -> Result<usize> {
    let conn = reader::open_readonly(db_path).context("reopening knowledgeC.db for digests")?;
    let tz = reader::newest_tz_offset(&conn, facts::STREAM_APP_USAGE)
        .context("reading the newest zone offset")?
        .unwrap_or(0);
    let offset = i32::try_from(tz)
        .ok()
        .and_then(chrono::FixedOffset::east_opt)
        .unwrap_or_else(|| chrono::Utc.fix());
    let now = chrono::Utc::now().with_timezone(&offset);
    println!("Digest     : evaluating days as of {now} (zone from the store)");

    let now_cocoa = (now.timestamp_millis() - reader::COCOA_EPOCH_OFFSET_MS) as f64 / 1000.0;
    let window_start = (knowledge.backfill_days > 0)
        .then(|| now_cocoa - f64::from(knowledge.backfill_days) * 86_400.0);
    let plans = super::plan_digest_days(&conn, knowledge, None, window_start, now)
        .context("planning digest days")?;
    drop(conn);

    let mut held: Option<String> = None;
    let mut newest: Option<(i64, i64, reader::DailyUsage, Option<i64>, bool)> = None;
    for plan in plans {
        match plan {
            super::DayPlan::Skip { day, reason } => {
                println!(
                    "  {}: skipped — {reason}",
                    reader::day_ordinal_to_date(day)
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| day.to_string())
                );
            }
            super::DayPlan::Hold { day, reason } => {
                held = Some(format!(
                    "{}: held — {reason}",
                    reader::day_ordinal_to_date(day)
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| day.to_string())
                ));
            }
            super::DayPlan::Emit {
                day,
                tz,
                usage,
                display_on_secs,
                fallback,
            } => newest = Some((day, tz, usage, display_on_secs, fallback)),
        }
    }
    if let Some(line) = held {
        println!("  {line}");
    }
    let Some((day, tz, usage, display_on_secs, fallback)) = newest else {
        println!("  no day is digestable yet (see the gates in knowledge::plan_digest_days)");
        return Ok(0);
    };
    let Some(date) = reader::day_ordinal_to_date(day) else {
        return Ok(0);
    };
    let digest = facts::DailyDigest {
        date,
        day,
        tz_offset_secs: tz,
        usage: &usage,
        display_on_secs,
        observed_at: now.with_timezone(&chrono::Utc),
    };
    let events = facts::daily_digest_events(&digest, ctx);
    println!(
        "  {date}: {} digest event(s){}{}",
        events.len(),
        if fallback {
            " (D+3 fallback, no flush evidence)"
        } else {
            ""
        },
        display_on_secs
            .map(|on| format!(
                ", display on {}",
                facts::humanize_duration(on.max(0) as u64)
            ))
            .unwrap_or_default()
    );
    for event in &events {
        serde_json::to_writer(&mut *out, event).context("serializing digest event")?;
        out.write_all(b"\n").context("writing digest event")?;
    }
    Ok(events.len())
}

/// Re-post every line of a park file. Each line is the `ParkRecord` the
/// lane wrote; only its `event` object is sent, rebuilt field by field
/// (`CaptureEvent` is write-only on the wire, so there is no `Deserialize`
/// to lean on — and rebuilding it means a hand-edited line still has to be
/// a valid event).
async fn replay_rejected(config: &AppConfig, park_file: &std::path::Path) -> Result<()> {
    println!("Mode       : replay {}", park_file.display());
    let raw = std::fs::read_to_string(park_file)
        .with_context(|| format!("reading {}", park_file.display()))?;

    let mut events: Vec<CaptureEvent> = Vec::new();
    let mut unreadable = 0usize;
    for (n, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|record| event_from_value(record.get("event")?))
        {
            Some(event) => events.push(event),
            None => {
                unreadable += 1;
                println!("  line {}: not a park record, skipped", n + 1);
            }
        }
    }
    println!(
        "Replaying  : {} event(s){}\n",
        events.len(),
        if unreadable > 0 {
            format!(" ({unreadable} unreadable line(s) skipped)")
        } else {
            String::new()
        }
    );
    if events.is_empty() {
        println!("Nothing to replay.");
        return Ok(());
    }

    // The same range stack the lane itself ships with (see `drain_once`):
    // CI-Server validates the whole request body, so one still-invalid
    // event 400s its entire batch — and a park file by construction holds
    // events the server already refused once. Halving on a permanent
    // answer lets every acceptable event through and names the ones that
    // are still refused by `source_id`, which is the whole point of a
    // replay tool. A transient answer (outage, 5xx) says nothing about any
    // one event, so that range fails as a unit, as it would in the lane.
    let (mut ingested, mut duplicate, mut failed) = (0u32, 0u32, 0usize);
    let mut stack: Vec<(usize, usize)> = (0..events.len())
        .step_by(REPLAY_BATCH)
        .map(|lo| (lo, (lo + REPLAY_BATCH).min(events.len())))
        .rev()
        .collect();
    while let Some((lo, hi)) = stack.pop() {
        match server_ingest::post_capture_events(config, &events[lo..hi]).await {
            Ok(ack) => {
                ingested += ack.ingested;
                duplicate += ack.duplicate;
            }
            Err(err) if err.permanent && hi - lo > 1 => {
                let mid = lo + (hi - lo) / 2;
                stack.push((mid, hi));
                stack.push((lo, mid));
            }
            Err(err) => {
                failed += hi - lo;
                if hi - lo == 1 {
                    println!(
                        "  still rejected: source_id={} kind={} ({}): {}",
                        events[lo].source_id,
                        events[lo].kind,
                        if err.permanent {
                            "permanent"
                        } else {
                            "transient"
                        },
                        super::redact_urls(&err.to_string())
                    );
                } else {
                    println!(
                        "  batch of {} failed (transient): {}",
                        hi - lo,
                        super::redact_urls(&err.to_string())
                    );
                }
            }
        }
    }

    println!("Result");
    println!("------");
    println!("ingested : {ingested}");
    println!("duplicate: {duplicate}");
    println!("failed   : {failed}");
    if failed == 0 {
        println!(
            "\nEvery event was accepted or was already known. The park file was left in\nplace; delete it yourself once you are satisfied."
        );
    }
    Ok(())
}

/// Rebuild a `CaptureEvent` from its own serialized form.
fn event_from_value(value: &Value) -> Option<CaptureEvent> {
    let string = |key: &str| value.get(key)?.as_str().map(str::to_owned);
    Some(CaptureEvent {
        source_system: "ci-applet",
        source_id: string("source_id")?,
        kind: string("kind")?,
        occurred_at: string("occurred_at")?,
        ended_at: string("ended_at"),
        observed_at: string("observed_at"),
        title: string("title"),
        description: string("description"),
        // `{}` and not `null`: `CaptureEvent.payload` always goes on the wire,
        // and the server's schema only substitutes its own `{}` default for
        // an *absent* payload — an explicit `null` is a permanent 400.
        payload: value
            .get("payload")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        capture_tick_id: string("capture_tick_id"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A park line round-trips into the same wire event, so a replay sends
    /// exactly what the lane tried to send — including a payload that the
    /// server search-indexes by exact-case key.
    #[test]
    fn a_park_record_rebuilds_into_the_event_it_recorded() {
        let original = CaptureEvent {
            source_system: "ci-applet",
            source_id: "knowledge:ABC".to_owned(),
            kind: "knowledge_media_usage".to_owned(),
            occurred_at: "2026-09-01T00:00:00.000Z".to_owned(),
            ended_at: Some("2026-09-01T00:02:00.000Z".to_owned()),
            observed_at: None,
            title: Some("Played media in Music for 2 min".to_owned()),
            description: None,
            payload: serde_json::json!({ "Title": "Song", "z_pk": 7 }),
            capture_tick_id: None,
        };
        let line = serde_json::json!({ "pk": 7, "event": original });
        let rebuilt = event_from_value(line.get("event").unwrap()).expect("rebuild");
        assert_eq!(
            serde_json::to_value(&rebuilt).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        assert!(event_from_value(&serde_json::json!({ "kind": "x" })).is_none());
    }

    /// The lane's own `park()` always writes a payload, but a hand-edited or
    /// foreign park line may not. `CaptureEvent.payload` is not optional on
    /// the wire, and CI-Server only substitutes its `{}` default for an
    /// *absent* payload — an explicit `null` fails its object schema with a
    /// permanent 400. So a missing payload must rebuild as `{}`, never null.
    #[test]
    fn a_park_record_without_a_payload_rebuilds_with_an_empty_object_not_null() {
        let rebuilt = event_from_value(&serde_json::json!({
            "source_id": "knowledge:ABC",
            "kind": "knowledge_app_usage",
            "occurred_at": "2026-09-01T00:00:00.000Z",
        }))
        .expect("rebuild");
        assert_eq!(rebuilt.payload, serde_json::json!({}));
        assert_ne!(rebuilt.payload, Value::Null);
    }

    /// A park file holds events the server already refused once, and
    /// CI-Server validates the whole body, so a replay that posts the file
    /// in flat batches would report every event as failed the moment one
    /// is still bad. Replay must bisect exactly as the lane does: every
    /// acceptable event is posted (and accepted) on its own side of the
    /// split, and the still-bad one ends up posted alone, where the report
    /// can name it.
    #[tokio::test(flavor = "multi_thread")]
    async fn replay_bisects_a_permanent_rejection_down_to_the_one_event_still_refused() {
        use super::super::e2e::{lane_config, source_ids, spawn_mock_server, status_rule};

        let (origin, recorded) =
            spawn_mock_server(vec![status_rule("knowledge:POISON", 400, true)]);
        let config = lane_config(&origin);

        let dir = tempfile::tempdir().unwrap();
        let park_file = dir.path().join("rejected.jsonl");
        let mut lines = String::new();
        for (pk, id) in [
            (1, "knowledge:GOOD-1"),
            (2, "knowledge:POISON"),
            (3, "knowledge:GOOD-2"),
        ] {
            let event = CaptureEvent {
                source_system: "ci-applet",
                source_id: id.to_owned(),
                kind: "knowledge_app_usage".to_owned(),
                occurred_at: "2026-09-01T00:00:00.000Z".to_owned(),
                ended_at: None,
                observed_at: None,
                title: None,
                description: None,
                payload: serde_json::json!({}),
                capture_tick_id: None,
            };
            lines.push_str(&serde_json::json!({ "pk": pk, "event": event }).to_string());
            lines.push('\n');
        }
        std::fs::write(&park_file, lines).unwrap();

        replay_rejected(&config, &park_file).await.expect("replay");

        let bodies: Vec<Vec<String>> = recorded
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, body)| source_ids(body))
            .collect();
        // [G1 P G2] → 400 → [G1] ok, [P G2] → 400 → [P] refused alone, [G2] ok.
        assert_eq!(bodies.len(), 5, "requests: {bodies:?}");
        assert!(
            bodies.contains(&vec!["knowledge:POISON".to_owned()]),
            "the still-refused event was never posted alone: {bodies:?}"
        );
        for good in ["knowledge:GOOD-1", "knowledge:GOOD-2"] {
            assert!(
                bodies.iter().any(|ids| ids.iter().any(|id| id == good)
                    && !ids.iter().any(|id| id == "knowledge:POISON")),
                "{good} was never posted without the poison event: {bodies:?}"
            );
        }
        assert!(park_file.exists(), "replay never deletes the park file");
    }
}
