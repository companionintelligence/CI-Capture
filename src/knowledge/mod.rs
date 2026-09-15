//! Device-activity lane — reads this Mac's own Apple "Knowledge" store,
//! `~/Library/Application Support/Knowledge/knowledgeC.db` (CoreDuet's
//! `knowledged`), and forwards what it holds to CI-Server as memory events:
//! which app was frontmost and for how long, media playback, notifications
//! received, Bluetooth device spans, and — opt-in — Safari domains and
//! App-Intent donations synced from the user's iPhone/iPad. A daily digest
//! per app and per day turns the raw sessions into the headline "facts".
//!
//! Requires **Full Disk Access**, exactly like the `imessage` lane
//! (`crate::permissions::protected_file_status`) — there is no system prompt
//! to trigger, only the deep link to System Settings and a relaunch.
//!
//! The lane deliberately reads a *subset* of the store. Never selected, so
//! never in process memory: full page URLs (`…WEBPAGEURL`), Bluetooth MAC
//! addresses, notification identifiers, contact identifiers, and the
//! serialized intent blobs. See `facts` for the per-stream table and the
//! non-disableable password-manager / sign-in-domain suppression lists.
//!
//! # Why the cursor is `Z_PK`, and why the lane bisects
//!
//! `knowledged` writes a row when a session *ends*, so `ZCREATIONDATE` is
//! always at or after `ZENDDATE` — up to eleven hours after it for
//! `/app/usage` on a real Mac. A start-date cursor therefore skips rows
//! that land late; `Z_PK` is monotonic within a store file and never reused,
//! so it is the one value that a "give me what I have not seen" query can
//! trust. `Z_METADATA.Z_UUID` names the store file; a new file (reinstall,
//! migration) restarts the sequence, so the cursor remembers which file it
//! belongs to and resets when the file changes.
//!
//! CI-Server's `POST /v1/events` is all-or-nothing: one event failing
//! validation 400s the whole batch, and an HTTP 200 whose `rejected[]` is
//! non-empty means a server-side materialize failure that is safe to
//! re-send. [`drain_once`] therefore ships pages in chunks, bisects a
//! permanently rejected chunk down to the single event at fault, and parks
//! *that one* to `knowledge-rejected.jsonl` (replayable with
//! `KNOWLEDGE_REPLAY_REJECTED=<path> --import-knowledge-once`) instead of
//! re-posting it forever — the failure mode the imessage lane still has.

pub mod app_names;
pub mod check;
#[cfg(test)]
mod e2e;
pub mod facts;
pub mod import_once;
pub mod reader;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::time::{MissedTickBehavior, interval};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::permissions::{self, PermissionStatus};
use crate::server_ingest::{self, CaptureEvent};

use facts::{FactContext, Skip};
use reader::{EventPage, KnowledgeRow};

/// Floor on the poll interval. Higher than the imessage lane's 10 s: the
/// store gains rows minutes apart, and every tick opens the file fresh and
/// probes `ZSTRUCTUREDMETADATA`'s 239-column layout.
const MIN_POLL_INTERVAL_SECS: u64 = 15;

/// Rows read from the store per SQL page. Bounded so one tick's memory is
/// bounded whatever the backlog (a first crawl is ~10⁴ rows).
const PAGE_LIMIT: u32 = 1000;

/// Events per `POST /v1/events`. CI-Server caps a request at 1000 events
/// and 10 MB; 200 keeps a bisect short (eight halvings to isolate one
/// event) while still amortizing the request overhead.
const BATCH_SIZE: usize = 200;

/// Upper bound on wall-clock spent in one drain — same reasoning as
/// `file_tracker::TICK_BUDGET`: a slow server must not monopolize the lane,
/// and whatever is left is still in the store next tick.
const TICK_BUDGET: Duration = Duration::from_secs(20);

/// Cap on events parked per tick, `file_tracker::MAX_PARKS_PER_TICK`'s
/// twin: parking is for the rare poison row, and a *systemic* 4xx (a
/// server-side schema change rejecting everything) must not flush the whole
/// backlog into the park file in seconds. Hitting the cap ends the tick and
/// backs off like an outage.
const MAX_PARKS_PER_TICK: usize = 5;

/// How long the same head-of-line event may keep failing *transiently*
/// before its chunk is bisected on transient failures too and the event
/// that still fails *on its own* is parked — `file_tracker`'s
/// `DEAD_LETTER_AFTER` precedent. A server that 500s on one specific event
/// looks transient forever, and without this valve that one event would
/// wedge the lane behind it. The valve never parks an event the server
/// has not just refused: an old stall against a server that has since
/// recovered ships normally, and a transport failure (the server never
/// answered) neither starts the clock nor fires the valve.
const STALL_AFTER: Duration = crate::sync::batching::DEAD_LETTER_AFTER;

/// Ceiling on `backoff_level` (and so on `backoff_ticks`) after repeated
/// systemic rejections, and the fixed window a permanently rejected digest
/// batch waits before it is re-posted.
const MAX_BACKOFF_TICKS: u32 = 64;

/// True if the lane should poll: turned on. Full Disk Access is a runtime
/// failure mode reported from `run`, not a configuration choice — same
/// contract as `imessage::is_runnable`.
pub fn is_runnable(config: &AppConfig) -> bool {
    config.knowledge.enabled
}

/// `~/Library/Application Support/Knowledge/knowledgeC.db` — the one path
/// this lane ever reads. No override: there is exactly one Knowledge store
/// per user on a Mac.
///
/// The home directory comes from `directories::BaseDirs`, not `$HOME`:
/// Windows populates `USERPROFILE` only, and a `HOME`-less build there
/// must still resolve a path so [`db_access_status`] reaches the
/// cfg-gated probe and reports `NotRequired` — the "no Knowledge store on
/// this system" verdict — rather than failing early as `Denied` and
/// showing the Full Disk Access copy on a system that has no such thing.
pub fn db_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("home directory could not be determined")?;
    Ok(base
        .home_dir()
        .join("Library/Application Support/Knowledge/knowledgeC.db"))
}

/// Whether this process can read the Knowledge store. Probes *this* file,
/// not `chat.db`: a Mac that never ran Messages has no `chat.db` and the
/// imessage probe reports `NotRequired` there, while `knowledgeC.db` exists
/// on every Mac and stays TCC-gated.
pub fn db_access_status() -> PermissionStatus {
    match db_path() {
        Ok(path) => permissions::protected_file_status(&path),
        Err(_) => PermissionStatus::Denied,
    }
}

// ── Cursor ──────────────────────────────────────────────────────────────────

/// Where the lane is in the store, persisted as JSON next to the imessage
/// cursor. Every field is `#[serde(default)]` so a cursor written by an
/// older build (or a hand-edited one) loads with the missing fields at
/// their zero values instead of being thrown away.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    /// `Z_METADATA.Z_UUID` of the store file `last_pk` belongs to. A
    /// different UUID on disk means a new file with a fresh `Z_PK`
    /// sequence, and the cursor restarts from the first-run floor.
    #[serde(default)]
    pub(crate) store_uuid: Option<String>,
    /// Highest `Z_PK` CI-Server has acknowledged (or the lane has decided
    /// to step past). `None` = never crawled.
    #[serde(default)]
    pub(crate) last_pk: Option<i64>,
    /// The last local day whose digest batch was acked. Owned by the
    /// digest stage; carried here so one file holds the lane's whole state.
    #[serde(default)]
    pub(crate) last_digest_day: Option<chrono::NaiveDate>,
    /// Cocoa seconds the first-run raw window began at (`now −
    /// backfill_days` at the raw lane's first tick). Digest days whose
    /// local midnight precedes it are skipped, because the raw lane never
    /// shipped their sessions. Pinned here rather than recomputed from
    /// `now` at digest time: the first digest pass routinely runs ticks or
    /// days after the raw floor was taken (backfill spans several ticks,
    /// the server may be down), and a floor that drifted forward with the
    /// clock would silently step past a day whose sessions *were* shipped.
    /// `None` = no floor.
    #[serde(default)]
    pub(crate) backfill_window_start: Option<f64>,
    /// Ticks still to skip after a systemic-rejection tick — the countdown
    /// `run` decrements. Set from `backoff_level`; a clean tick clears
    /// both.
    #[serde(default)]
    pub(crate) backoff_ticks: u32,
    /// The last systemic-rejection backoff that was issued — the level the
    /// next cap hit doubles from (`2n+1`, capped at [`MAX_BACKOFF_TICKS`]).
    /// Kept apart from `backoff_ticks` because `run` only reaches
    /// `drain_once` once the countdown is back to zero, so doubling from
    /// the countdown itself would always yield 1. Cleared by a clean tick
    /// together with `backoff_ticks`.
    #[serde(default)]
    pub(crate) backoff_level: u32,
    /// Ticks still to skip `digest_once` after a permanent digest
    /// rejection — a shape bug is re-posted once per window
    /// ([`MAX_BACKOFF_TICKS`] ticks), not per tick, and the raw lane keeps
    /// running meanwhile.
    #[serde(default)]
    pub(crate) digest_backoff_ticks: u32,
    /// `(source_id, unix_ms)` of the head-of-line event whose chunk has
    /// been refused transiently by the server since `unix_ms`. Once it has
    /// been stuck for [`STALL_AFTER`], the next tick bisects that chunk on
    /// transient failures too and parks the one event that still fails on
    /// its own — an event the server accepts by then simply ships.
    #[serde(default)]
    pub(crate) stalled_head: Option<(String, i64)>,
}

/// Load the cursor, or start over on a missing or unreadable file — same
/// reasoning as the imessage lane and `config.rs`: garbage on disk restarts
/// the lane rather than killing it, and CI-Server dedups on `source_id`, so
/// the replay lands as duplicates-suppressed rather than duplicated.
pub(crate) fn load_cursor(path: &Path) -> Cursor {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Cursor::default();
    };
    match serde_json::from_str::<Cursor>(&raw) {
        Ok(cursor) => cursor,
        Err(err) => {
            log::warn!(
                "knowledge: cursor file {} is unreadable, restarting from the first-run floor: {err}",
                path.display()
            );
            Cursor::default()
        }
    }
}

/// Write the cursor atomically (temp file + rename), so a crash mid-write
/// leaves the previous cursor rather than half a JSON document.
pub(crate) fn save_cursor(path: &Path, cursor: &Cursor) -> Result<()> {
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

// ── Lane lifecycle ──────────────────────────────────────────────────────────

/// Run the lane until aborted — same lifecycle as `imessage::run`, spawned
/// and stopped the same way from `worker.rs`.
pub async fn run(config: AppConfig, cursor_path: PathBuf) -> Result<()> {
    if !is_runnable(&config) {
        log::info!("knowledge: not enabled, lane idle");
        return Ok(());
    }

    // Three states: `NotRequired` is a system with no Knowledge store at all
    // (every non-macOS build, or a Mac whose store has not been created yet)
    // — telling that user to grant Full Disk Access would change nothing.
    match db_access_status() {
        PermissionStatus::Granted => {}
        PermissionStatus::NotRequired => {
            log::info!(
                "knowledge: no {} on this system — nothing to read, lane idle",
                db_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| {
                        "~/Library/Application Support/Knowledge/knowledgeC.db".to_string()
                    })
            );
            return Ok(());
        }
        PermissionStatus::Denied => {
            log::warn!(
                "knowledge: Full Disk Access not granted; lane idle until it is (System Settings → Privacy & Security → Full Disk Access, then relaunch): {}",
                permissions::full_disk_access_settings_url()
            );
            return Ok(());
        }
    }

    let device_id = config.identity.device_id.unwrap_or_else(Uuid::nil);
    let db_path = db_path()?;
    let rejected_path = crate::config::knowledge_rejected_path()?;
    let mut cursor = load_cursor(&cursor_path);
    let poll_secs = config
        .knowledge
        .poll_interval_secs
        .max(MIN_POLL_INTERVAL_SECS);
    let mut tick = interval(Duration::from_secs(poll_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    log::info!(
        "knowledge lane running: {}, cursor={:?}, streams={:?}, poll every {poll_secs}s",
        db_path.display(),
        cursor.last_pk,
        facts::effective_streams(&config.knowledge)
    );

    loop {
        tick.tick().await;

        if !server_ingest::is_configured(&config) {
            log::debug!("knowledge: no Memory credential yet; skipping poll");
            continue;
        }

        if cursor.backoff_ticks > 0 {
            cursor.backoff_ticks -= 1;
            log::debug!(
                "knowledge: backing off after systemic rejections ({} tick(s) left)",
                cursor.backoff_ticks
            );
            if let Err(err) = save_cursor(&cursor_path, &cursor) {
                log::warn!("knowledge: failed to persist cursor: {err:?}");
            }
            continue;
        }

        match drain_once(
            &db_path,
            &config,
            device_id,
            &mut cursor,
            &cursor_path,
            &rejected_path,
            PAGE_LIMIT,
            BATCH_SIZE,
            TICK_BUDGET,
        )
        .await
        {
            Ok(outcome) => {
                if outcome.rows_returned > 0 || outcome.parked > 0 {
                    log::info!("knowledge: {outcome:?}");
                }
            }
            Err(err) => log::warn!("knowledge: drain failed, will retry next tick: {err:?}"),
        }
    }
}

/// What one [`drain_once`] pass did. Every row the SQL returned is
/// accounted for by exactly one of `ingested + duplicate + parked +
/// skipped_*` or by "still unsent" (a transient stop), so a shrinking page
/// is always explainable.
#[derive(Debug, Default)]
pub struct PollOutcome {
    /// Rows the reader handed back with representable timestamps.
    pub fetched: usize,
    /// Rows the SQL returned, dropped ones included.
    pub rows_returned: usize,
    pub ingested: usize,
    pub duplicate: usize,
    pub skipped_invalid_timestamp: usize,
    /// Below the stream's duration floor (`facts::min_duration_secs`).
    pub skipped_short: usize,
    /// Password managers, sign-in domains, user exclusions.
    pub skipped_sensitive: usize,
    /// Not a user-activity fact by policy (system notification centre,
    /// display-off spans, rows with no subject).
    pub skipped_system: usize,
    /// Events CI-Server permanently rejected and the lane wrote to the
    /// park file.
    pub parked: usize,
    /// Highest `Z_PK` the cursor moved to during this pass, `None` when it
    /// did not move.
    pub advanced_to: Option<i64>,
}

/// Where a first page starts after the store has been inspected, and why.
struct Start {
    store_uuid: Option<String>,
    after_pk: i64,
    reason: Option<&'static str>,
    /// Cocoa seconds the backfill window began at, when `after_pk` was
    /// taken from a first-run floor with a non-zero `backfill_days` — the
    /// same instant the digest floor is pinned to.
    window_start: Option<f64>,
}

/// One parked event as written to `knowledge-rejected.jsonl`: enough to
/// replay it verbatim and to see at a glance what was refused.
#[derive(Serialize)]
struct ParkRecord<'a> {
    pk: i64,
    source_id: &'a str,
    kind: &'a str,
    stream: &'a str,
    reason: String,
    rejected_at: String,
    event: &'a CaptureEvent,
}

/// One drain pass: inspect the store, then read pages after the cursor,
/// map each page's rows to events, ship them in `batch_size` chunks, and
/// advance the cursor only as far as CI-Server has acknowledged.
///
/// Sizes and the budget are injected so tests can force page boundaries
/// and chunk splits with a handful of fixture rows. `run` passes
/// [`PAGE_LIMIT`] / [`BATCH_SIZE`] / [`TICK_BUDGET`].
///
/// A `budget` of zero means "one acknowledged chunk, then stop" — the
/// budget is checked *after* each ack, never before the first, so a tick
/// always makes progress if the server is answering.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drain_once(
    db_path: &Path,
    config: &AppConfig,
    device_id: Uuid,
    cursor: &mut Cursor,
    cursor_path: &Path,
    rejected_path: &Path,
    page_limit: u32,
    batch_size: usize,
    budget: Duration,
) -> Result<PollOutcome> {
    let tick_start = Instant::now();
    let mut outcome = PollOutcome::default();
    let batch_size = batch_size.clamp(1, 1000);
    let streams = facts::effective_streams(&config.knowledge);
    let include_synced = config.knowledge.include_synced_devices;
    let backfill_days = config.knowledge.backfill_days;

    // ── Where to start ──────────────────────────────────────────────────
    // rusqlite::Connection holds a raw pointer and must not cross an await
    // point, so every store access is a fresh open on a blocking thread —
    // also what keeps a read transaction from being held against a file
    // `knowledged` is writing while a POST is in flight.
    let known_uuid = cursor.store_uuid.clone();
    let known_pk = cursor.last_pk;
    let path = db_path.to_path_buf();
    let start: Start = tokio::task::spawn_blocking(move || -> Result<Start> {
        let conn = reader::open_readonly(&path)?;
        let store_uuid = reader::read_store_uuid(&conn)?;
        let max_pk = reader::max_pk(&conn)?;

        let reason = match known_pk {
            None => Some("first run"),
            Some(_) if known_uuid.is_some() && known_uuid != store_uuid => {
                Some("store file changed (Z_METADATA.Z_UUID differs)")
            }
            Some(pk) if max_pk.is_some_and(|m| m < pk) => {
                Some("cursor is above MAX(Z_PK) — store was replaced")
            }
            Some(_) => None,
        };
        // One clock reading for both floors, so the raw window and the
        // digest window start at the very same instant.
        let now_cocoa =
            (chrono::Utc::now().timestamp_millis() - reader::COCOA_EPOCH_OFFSET_MS) as f64 / 1000.0;
        let (after_pk, window_start) = match (reason, known_pk) {
            (None, Some(pk)) => (pk, None),
            _ => (
                reader::first_run_floor_at(&conn, backfill_days, now_cocoa)?,
                (backfill_days > 0).then(|| now_cocoa - f64::from(backfill_days) * 86_400.0),
            ),
        };
        Ok(Start {
            store_uuid,
            after_pk,
            reason,
            window_start,
        })
    })
    .await
    .context("joining knowledgeC.db inspection thread")??;

    if let Some(reason) = start.reason {
        if cursor.last_pk.is_some() {
            log::warn!(
                "knowledge: {reason}; resetting cursor from {:?} to first-run floor {}",
                cursor.last_pk,
                start.after_pk
            );
        } else {
            log::info!(
                "knowledge: {reason}; starting after Z_PK {} (backfill_days={backfill_days})",
                start.after_pk
            );
        }
        cursor.last_pk = Some(start.after_pk);
        cursor.store_uuid = start.store_uuid.clone();
        // The digest floor is pinned to this raw floor only while the
        // digest stage has not started: a store reset after digests have
        // shipped must not re-floor days that were already summarized.
        if cursor.last_digest_day.is_none() {
            cursor.backfill_window_start = start.window_start;
        }
        save_cursor(cursor_path, cursor)?;
    } else if cursor.store_uuid != start.store_uuid {
        // A store that gained a UUID (or a cursor from before UUIDs were
        // recorded): remember it without moving.
        cursor.store_uuid = start.store_uuid.clone();
        save_cursor(cursor_path, cursor)?;
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    // The head-of-line valve: a source id whose chunk the server has been
    // refusing transiently for longer than STALL_AFTER. This tick that chunk
    // is bisected on transient answers too, so the event that still fails
    // on its own is the one parked.
    let stalled_id: Option<String> = cursor
        .stalled_head
        .as_ref()
        .filter(|(_, since)| now_ms.saturating_sub(*since) > STALL_AFTER.as_millis() as i64)
        .map(|(id, _)| id.clone());

    let ctx = FactContext {
        config: &config.knowledge,
        device_id,
        store_uuid: start.store_uuid.as_deref(),
        web_effective: streams.iter().any(|s| s == facts::STREAM_WEB_USAGE),
    };

    let mut parked = 0usize;
    let mut stall_parks = 0usize; // the valve parks at most one event per tick
    let mut stopped = false; // transient failure or park cap: cursor stays put
    let mut budget_hit = false;
    let mut caught_up = false; // the last page was short: nothing unsent remains

    // ── Pages ───────────────────────────────────────────────────────────
    loop {
        let after_pk = cursor.last_pk.unwrap_or(0);
        let page = read_page(db_path, after_pk, &streams, include_synced, page_limit).await?;
        outcome.rows_returned += page.rows_returned;
        outcome.fetched += page.rows.len();
        outcome.skipped_invalid_timestamp += page.skipped_invalid_timestamp;
        if page.skipped_invalid_timestamp > 0 {
            log::warn!(
                "knowledge: dropped {} row(s) with an unrepresentable timestamp in this page",
                page.skipped_invalid_timestamp
            );
        }
        if page.rows_returned == 0 {
            caught_up = true;
            break;
        }

        // ── Map ─────────────────────────────────────────────────────────
        let mut events: Vec<CaptureEvent> = Vec::with_capacity(page.rows.len());
        let mut meta: Vec<(i64, String)> = Vec::with_capacity(page.rows.len());
        let mut short_by_stream: Vec<(String, usize)> = Vec::new();
        for row in &page.rows {
            match facts::to_capture_event(row, &ctx) {
                Ok(event) => {
                    events.push(event);
                    meta.push((row.pk, row.stream.clone()));
                }
                Err(Skip::Short) => {
                    outcome.skipped_short += 1;
                    match short_by_stream.iter_mut().find(|(s, _)| *s == row.stream) {
                        Some((_, n)) => *n += 1,
                        None => short_by_stream.push((row.stream.clone(), 1)),
                    }
                }
                Err(Skip::Sensitive) => outcome.skipped_sensitive += 1,
                Err(Skip::System) => outcome.skipped_system += 1,
            }
        }
        if !short_by_stream.is_empty() {
            log::info!(
                "knowledge: {} row(s) below the duration floor: {}",
                outcome.skipped_short,
                short_by_stream
                    .iter()
                    .map(|(s, n)| format!("{s}={n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // ── Ship ────────────────────────────────────────────────────────
        // An explicit stack of half-open index ranges over the owned Vec:
        // chunks are pushed in reverse so the earliest pops first, and a
        // permanent rejection splits its range in two (later half pushed
        // first) so the cursor only ever advances over a contiguous acked
        // prefix. No event is cloned — `post_capture_events` borrows the
        // slice.
        let mut stack: Vec<(usize, usize)> = (0..events.len())
            .step_by(batch_size)
            .map(|lo| (lo, (lo + batch_size).min(events.len())))
            .rev()
            .collect();

        // The stalled chunk is the first one on this page iff its head is
        // the stalled id (the cursor sat at the last acked chunk all along,
        // so the same rows page up again). Every sub-range of that chunk
        // bisects on a transient answer just as it would on a permanent
        // one — the whole chunk has been refused for a day, so each of its
        // events has stalled that long — and the one that fails *alone*
        // is parked. Anything outside it keeps the ordinary transient
        // handling, and so does the chunk once one park has been taken.
        let stalled_chunk_hi = match (stalled_id.as_deref(), events.first()) {
            (Some(id), Some(head)) if id == head.source_id => Some(batch_size.min(events.len())),
            _ => None,
        };

        while let Some((lo, hi)) = stack.pop() {
            let in_stalled_chunk =
                stall_parks == 0 && stalled_chunk_hi.is_some_and(|end| hi <= end);
            let result = server_ingest::post_capture_events(config, &events[lo..hi]).await;

            match result {
                Ok(ack) => {
                    outcome.ingested += ack.ingested as usize;
                    outcome.duplicate += ack.duplicate as usize;
                    let acked_pk = meta[hi - 1].0;
                    cursor.last_pk = Some(acked_pk);
                    outcome.advanced_to = Some(acked_pk);
                    save_cursor(cursor_path, cursor)?;
                    if tick_start.elapsed() >= budget {
                        budget_hit = true;
                        break;
                    }
                }
                // A transport failure inside the stalled chunk is not the
                // server refusing anything — it never answered — so it is
                // an outage like any other, never a park.
                Err(err) if err.permanent || (in_stalled_chunk && err.reached_server) => {
                    if hi - lo > 1 {
                        let mid = lo + (hi - lo) / 2;
                        stack.push((mid, hi));
                        stack.push((lo, mid));
                        continue;
                    }
                    // Exactly one event at fault: it was just posted alone
                    // and refused. Park it — persisted and replayable, never
                    // dropped — and step past it.
                    let reason = if err.permanent {
                        err.to_string()
                    } else {
                        stall_parks += 1;
                        format!(
                            "head-of-line event still failing transiently on its own after stalling the lane for more than {} h; parked to keep the lane moving: {err}",
                            STALL_AFTER.as_secs() / 3600
                        )
                    };
                    let (pk, stream) = &meta[lo];
                    if let Err(park_err) =
                        park(rejected_path, *pk, stream, &events[lo], &reason, now_ms)
                    {
                        // Cannot record the rejection ⇒ cannot safely step
                        // past it. Same as an outage: retry next tick.
                        log::error!(
                            "knowledge: cannot append to park file {}: {park_err:?}; leaving the cursor at {:?}",
                            rejected_path.display(),
                            cursor.last_pk
                        );
                        stopped = true;
                        break;
                    }
                    log::error!(
                        "knowledge: parked permanently rejected event kind={} stream={stream} pk={pk}: {}",
                        events[lo].kind,
                        redact_urls(&reason)
                    );
                    parked += 1;
                    outcome.parked += 1;
                    cursor.last_pk = Some(*pk);
                    outcome.advanced_to = Some(*pk);
                    if cursor.stalled_head.as_ref().map(|(id, _)| id.as_str())
                        == Some(events[lo].source_id.as_str())
                    {
                        cursor.stalled_head = None;
                    }
                    save_cursor(cursor_path, cursor)?;
                    if parked >= MAX_PARKS_PER_TICK {
                        cursor.backoff_level =
                            (cursor.backoff_level * 2 + 1).min(MAX_BACKOFF_TICKS);
                        cursor.backoff_ticks = cursor.backoff_level;
                        log::error!(
                            "knowledge: {parked} events parked in one tick — treating as systemic rejection, backing off {} tick(s)",
                            cursor.backoff_ticks
                        );
                        save_cursor(cursor_path, cursor)?;
                        stopped = true;
                        break;
                    }
                }
                Err(err) => {
                    // Transient: the cursor stays at the last acked chunk
                    // and the whole remainder is re-read next tick. When
                    // the server *answered*, remember which event is at
                    // the head so a poison one can be parked once it has
                    // stalled the lane long enough. A transport failure
                    // says nothing about the event, so it neither starts
                    // nor resets that clock — a day-long outage or sleep
                    // must not mature into a park of a healthy event.
                    if err.reached_server {
                        let head = events[lo].source_id.as_str();
                        let since = match &cursor.stalled_head {
                            Some((id, since)) if id == head => *since,
                            _ => now_ms,
                        };
                        cursor.stalled_head = Some((head.to_owned(), since));
                        save_cursor(cursor_path, cursor)?;
                    }
                    log::warn!(
                        "knowledge: batch of {} event(s) failed transiently, stopping this tick at cursor {:?}: {}",
                        hi - lo,
                        cursor.last_pk,
                        redact_urls(&err.to_string())
                    );
                    stopped = true;
                    break;
                }
            }
        }

        if stopped || budget_hit {
            break;
        }

        // Every chunk of this page is acked (including "there were none"):
        // step past the rows the policy skipped and the ones the reader
        // dropped, so a page of nothing but sub-floor blips is not re-read
        // forever.
        if let Some(last_seen) = page.last_seen_pk
            && cursor.last_pk.is_none_or(|pk| pk < last_seen)
        {
            cursor.last_pk = Some(last_seen);
            outcome.advanced_to = Some(last_seen);
            save_cursor(cursor_path, cursor)?;
        }

        // Last page, or out of time: whatever is left waits for next tick.
        if page.rows_returned < page_limit as usize {
            caught_up = true;
            break;
        }
        if tick_start.elapsed() >= budget {
            break;
        }
    }

    // A clean tick — no transient stop, no park cap — clears the outage
    // state; a tick that parked a handful of poison rows still counts as
    // clean (file_tracker's "per-event problem, not an outage").
    if !stopped
        && (cursor.backoff_ticks != 0 || cursor.backoff_level != 0 || cursor.stalled_head.is_some())
    {
        cursor.backoff_ticks = 0;
        cursor.backoff_level = 0;
        cursor.stalled_head = None;
        save_cursor(cursor_path, cursor)?;
    }

    // Digests run only once the raw backlog is fully acked in a clean tick,
    // so a day's digest never precedes the sessions it summarizes and an
    // outage that stopped the raw lane is not retried a second time here.
    // A permanently rejected digest batch sits out its own backoff window
    // first (see `digest_once`), while the raw lane above keeps shipping.
    // This is the lane's one and only `chrono::Local` — `digest_once` and
    // everything below it take the clock as a parameter.
    if !stopped && caught_up {
        if cursor.digest_backoff_ticks > 0 {
            cursor.digest_backoff_ticks -= 1;
            log::debug!(
                "knowledge: digest backing off after a permanent rejection ({} tick(s) left)",
                cursor.digest_backoff_ticks
            );
            save_cursor(cursor_path, cursor)?;
        } else {
            match digest_once(
                db_path,
                config,
                device_id,
                cursor,
                cursor_path,
                chrono::Local::now().fixed_offset(),
            )
            .await
            {
                Ok(digest) => {
                    if digest.days_emitted > 0 || digest.days_skipped > 0 {
                        log::info!("knowledge: {digest:?}");
                    }
                }
                Err(err) => {
                    log::warn!("knowledge: digest pass failed, will retry next tick: {err:?}")
                }
            }
        }
    }

    Ok(outcome)
}

// ── Daily digests ───────────────────────────────────────────────────────────

/// What one [`digest_once`] pass did.
#[derive(Debug, Default)]
pub struct DigestOutcome {
    /// Days whose digest batch CI-Server acknowledged this pass.
    pub days_emitted: usize,
    /// Days stepped over without a digest: outside the store's coverage,
    /// before the first-run backfill floor, or empty after suppression.
    pub days_skipped: usize,
    /// The day the pass stopped at because it is not final yet (or its
    /// batch failed), if any — it is retried next tick.
    pub held: Option<chrono::NaiveDate>,
    pub ingested: usize,
    pub duplicate: usize,
}

/// One candidate day's verdict from [`plan_digest_days`]: the gates are
/// evaluated in SQL-adjacent blocking code and returned as data, so the
/// async half only has to post and move the cursor.
#[derive(Debug)]
pub(crate) enum DayPlan {
    /// Final, but nothing will ever be emitted for it — advance past it.
    Skip { day: i64, reason: &'static str },
    /// Not final yet — stop here and retry next tick.
    Hold { day: i64, reason: &'static str },
    /// Every gate passed; `usage` and `tz` are what the events are built
    /// from. `fallback` marks a day released by the D+3 valve rather than
    /// by the evidence gates 2 and 3 normally wait for.
    Emit {
        day: i64,
        tz: i64,
        usage: reader::DailyUsage,
        display_on_secs: Option<i64>,
        fallback: bool,
    },
}

impl DayPlan {
    pub(crate) fn day(&self) -> i64 {
        match self {
            DayPlan::Skip { day, .. } | DayPlan::Hold { day, .. } | DayPlan::Emit { day, .. } => {
                *day
            }
        }
    }
}

/// A day is "complete" this many hours after its local midnight has
/// passed (D+1 02:00): headroom for `knowledged`'s end-of-session write to
/// land before the flush-evidence probe is even consulted.
const COMPLETION_HOURS_AFTER_MIDNIGHT: i64 = 2;
/// Days after D at whose local midnight a day is digested even without
/// flush evidence — the valve for a Mac that simply was not used again.
const FALLBACK_DAYS: i64 = 3;

/// Evaluate the digest gates for every candidate day after `after_day`,
/// in order, stopping at the first day that must be held. Blocking: runs
/// against an open connection, so call it from `spawn_blocking` (as
/// [`digest_once`] does) or from a one-shot CLI.
///
/// The four gates, per day D (local day ordinal, `tz` = modal
/// `ZSECONDSFROMGMT` of its rows):
///
/// 1. **covered** — `coverage_start('/app/usage') ≤ D 00:00`: the store
///    still holds a row *created* before D began, so the rolling purge
///    cannot have eaten D's early sessions. An uncovered day can never
///    become covered, so it is skipped for good. Days whose local midnight
///    precedes the raw lane's first-run window start
///    (`backfill_window_start`, pinned in the cursor at the raw lane's
///    first tick) are skipped the same way: the raw lane never shipped
///    their sessions. The comparison is in the day's own zone, so a Mac
///    that changed zones since is not off by a day.
/// 2. **completed** — `now ≥ D+1 02:00` *and* a `/app/usage` row started
///    on or after D+1 00:00 (FIFO flush evidence); failing that, released
///    anyway at `now ≥ D+3 00:00` with a warning, so a Mac that was shut
///    for a long weekend still gets its last day summarized.
/// 3. **display closure** — `display_on_secs` is included only when
///    `/display/isBacklit` is not disabled, its coverage reaches D, and its
///    `MAX(ZENDDATE) ≥ D+1 00:00` (the span across midnight has closed).
///    Enabled-and-covered but not yet closed holds the *whole* day rather
///    than emitting an undercount — until the D+3 valve, which releases
///    the day *without* the figure: a display left on across midnight
///    ("prevent sleep" on a desktop) never closes its span, and holding
///    on it would wedge every later digest behind the purge.
/// 4. **≥ 1 app row after suppression** — checked by the caller when the
///    builder returns nothing; such a day is final and skipped.
pub(crate) fn plan_digest_days(
    conn: &rusqlite::Connection,
    knowledge: &crate::config::KnowledgeConfig,
    after_day: Option<i64>,
    backfill_window_start: Option<f64>,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> Result<Vec<DayPlan>> {
    use reader::local_midnight_cocoa_secs as midnight;

    let now_cocoa = (now.timestamp() - reader::COCOA_EPOCH_OFFSET_MS / 1000) as f64;
    let display_enabled = !knowledge
        .disabled_streams
        .iter()
        .any(|s| s == facts::STREAM_DISPLAY);
    let short_below = i64::try_from(knowledge.min_session_secs).unwrap_or(i64::MAX);

    let Some(app_coverage) = reader::coverage_start(conn, facts::STREAM_APP_USAGE)? else {
        return Ok(Vec::new());
    };
    let display_coverage = if display_enabled {
        reader::coverage_start(conn, facts::STREAM_DISPLAY)?
    } else {
        None
    };
    let display_max_end = if display_enabled {
        reader::max_end(conn, facts::STREAM_DISPLAY)?
    } else {
        None
    };
    let mut plans = Vec::new();
    for day in reader::local_days_with_rows(conn, facts::STREAM_APP_USAGE, after_day)? {
        let Some(tz) = reader::modal_tz_offset(conn, facts::STREAM_APP_USAGE, day)? else {
            plans.push(DayPlan::Skip {
                day,
                reason: "no zone offset on its rows",
            });
            continue;
        };
        let day_start = midnight(day, tz);
        let next_midnight = midnight(day + 1, tz);

        // 1. covered
        if app_coverage > day_start {
            plans.push(DayPlan::Skip {
                day,
                reason: "before the store's coverage (rolling purge)",
            });
            continue;
        }
        // A day that began before the raw window is not a whole day the
        // raw lane shipped: skip it (its digest would summarize sessions
        // the server never saw).
        if backfill_window_start.is_some_and(|window_start| day_start < window_start) {
            plans.push(DayPlan::Skip {
                day,
                reason: "before the first-run backfill window",
            });
            continue;
        }

        // 2. completed
        let completion_point = next_midnight + (COMPLETION_HOURS_AFTER_MIDNIGHT * 3_600) as f64;
        let past_fallback = now_cocoa >= midnight(day + FALLBACK_DAYS, tz);
        let flushed = now_cocoa >= completion_point
            && reader::has_local_row_on_or_after(conn, facts::STREAM_APP_USAGE, next_midnight)?;
        if !flushed && !past_fallback {
            plans.push(DayPlan::Hold {
                day,
                reason: if now_cocoa < completion_point {
                    "day not complete yet (before D+1 02:00)"
                } else {
                    "waiting for flush evidence (no later /app/usage row yet)"
                },
            });
            break;
        }

        // 3. display closure — the D+3 valve applies here too, and a day
        // it releases with an open display timeline ships *without* the
        // figure rather than with the known undercount (a re-send could
        // never correct it: the summary's source id is stable and
        // CI-Server drops a re-post as a duplicate).
        let display_reaches_day = display_coverage.is_some_and(|cov| cov <= day_start);
        let display_closed = display_max_end.is_some_and(|end| end >= next_midnight);
        let display_open = display_reaches_day && !display_closed;
        if display_open && !past_fallback {
            plans.push(DayPlan::Hold {
                day,
                reason: "display timeline has not closed past midnight yet",
            });
            break;
        }

        let usage = reader::read_daily_usage(conn, day, short_below)?;
        let display_on_secs = if display_reaches_day && display_closed {
            usage.display_on_secs
        } else {
            None
        };
        plans.push(DayPlan::Emit {
            day,
            tz,
            usage,
            display_on_secs,
            // Released by the valve rather than by the evidence the gates
            // normally wait for — either kind is worth the warning.
            fallback: past_fallback && (!flushed || display_open),
        });
    }
    Ok(plans)
}

/// Emit the daily digests that have become final since the last pass and
/// advance `cursor.last_digest_day` — **only after** each day's batch is
/// acknowledged, so a failed POST leaves the day to be rebuilt and re-sent
/// next tick (its source ids are stable, so a partial ack costs only
/// `duplicate`s).
///
/// `now` is injected: production passes `chrono::Local::now()` from the
/// one call site in [`drain_once`]; tests pass literal instants in a fixed
/// offset, so every gate below is host-zone independent.
pub(crate) async fn digest_once(
    db_path: &Path,
    config: &AppConfig,
    device_id: Uuid,
    cursor: &mut Cursor,
    cursor_path: &Path,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> Result<DigestOutcome> {
    let mut outcome = DigestOutcome::default();
    if !config.knowledge.daily_digest {
        return Ok(outcome);
    }
    let streams = facts::effective_streams(&config.knowledge);
    if !streams.iter().any(|s| s == facts::STREAM_APP_USAGE) {
        return Ok(outcome);
    }

    let after_day = cursor.last_digest_day.map(reader::date_to_day_ordinal);
    let backfill_window_start = cursor.backfill_window_start;
    let knowledge = config.knowledge.clone();
    let path = db_path.to_path_buf();
    let (store_uuid, plans) = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = reader::open_readonly(&path)?;
        let store_uuid = reader::read_store_uuid(&conn)?;
        let plans = plan_digest_days(&conn, &knowledge, after_day, backfill_window_start, now)?;
        Ok((store_uuid, plans))
    })
    .await
    .context("joining knowledgeC.db digest thread")??;

    let ctx = FactContext {
        config: &config.knowledge,
        device_id,
        store_uuid: store_uuid.as_deref(),
        web_effective: streams.iter().any(|s| s == facts::STREAM_WEB_USAGE),
    };
    let observed_at = now.with_timezone(&chrono::Utc);

    for plan in plans {
        let day = plan.day();
        let Some(date) = reader::day_ordinal_to_date(day) else {
            // Unreachable for any real store (chrono covers ±262 000 years);
            // hold rather than advance so the cursor never loses its date.
            log::warn!("knowledge: digest day ordinal {day} is not a date; holding");
            break;
        };
        match plan {
            DayPlan::Skip { reason, .. } => {
                log::info!("knowledge: no digest for {date}: {reason}");
                advance_digest_day(cursor, cursor_path, day)?;
                outcome.days_skipped += 1;
            }
            DayPlan::Hold { reason, .. } => {
                log::debug!("knowledge: holding digest for {date}: {reason}");
                outcome.held = Some(date);
                break;
            }
            DayPlan::Emit {
                tz,
                usage,
                display_on_secs,
                fallback,
                ..
            } => {
                if fallback {
                    log::warn!(
                        "knowledge: digesting {date} past the D+{FALLBACK_DAYS} valve without the evidence the gates wait for; a late-written session may be missing, and display time is omitted if its span never closed"
                    );
                }
                let digest = facts::DailyDigest {
                    date,
                    day,
                    tz_offset_secs: tz,
                    usage: &usage,
                    display_on_secs,
                    observed_at,
                };
                let events = facts::daily_digest_events(&digest, &ctx);
                if events.is_empty() {
                    log::info!(
                        "knowledge: no digest for {date}: no app activity after suppression"
                    );
                    advance_digest_day(cursor, cursor_path, day)?;
                    outcome.days_skipped += 1;
                    continue;
                }

                let mut failed = false;
                for chunk in events.chunks(BATCH_SIZE) {
                    match server_ingest::post_capture_events(config, chunk).await {
                        Ok(ack) => {
                            outcome.ingested += ack.ingested as usize;
                            outcome.duplicate += ack.duplicate as usize;
                        }
                        Err(err) => {
                            // Transient: rebuilt and re-posted next tick.
                            // Permanent: a code-level shape problem — logged
                            // loudly once, then retried every
                            // MAX_BACKOFF_TICKS ticks so neither the log nor
                            // the wire is hammered; `last_digest_day` never
                            // moves past an unacked day (parking a digest
                            // would silently drop a whole day's summary, and
                            // a rebuilt day is what ships once the shape is
                            // fixed).
                            let level = if err.permanent {
                                log::Level::Error
                            } else {
                                log::Level::Warn
                            };
                            log::log!(
                                level,
                                "knowledge: digest batch for {date} ({} event(s)) failed{}: {}",
                                chunk.len(),
                                if err.permanent {
                                    format!(
                                        " permanently; next attempt in {MAX_BACKOFF_TICKS} ticks"
                                    )
                                } else {
                                    String::new()
                                },
                                redact_urls(&err.to_string())
                            );
                            if err.permanent {
                                cursor.digest_backoff_ticks = MAX_BACKOFF_TICKS;
                                save_cursor(cursor_path, cursor)?;
                            }
                            failed = true;
                            break;
                        }
                    }
                }
                if failed {
                    outcome.held = Some(date);
                    break;
                }
                advance_digest_day(cursor, cursor_path, day)?;
                outcome.days_emitted += 1;
                log::info!(
                    "knowledge: digest for {date}: {} event(s) ({} app(s)){}",
                    events.len(),
                    events.len() - 1,
                    display_on_secs
                        .map(|on| format!(
                            ", display on {}",
                            facts::humanize_duration(on.max(0) as u64)
                        ))
                        .unwrap_or_default()
                );
            }
        }
    }
    Ok(outcome)
}

/// Move `last_digest_day` to `day` and persist it.
fn advance_digest_day(cursor: &mut Cursor, cursor_path: &Path, day: i64) -> Result<()> {
    cursor.last_digest_day = reader::day_ordinal_to_date(day);
    save_cursor(cursor_path, cursor)
}

/// Read one page after `after_pk` on a blocking thread, opening the store
/// fresh so no connection lives across the POSTs that follow.
async fn read_page(
    db_path: &Path,
    after_pk: i64,
    streams: &[String],
    include_synced: bool,
    page_limit: u32,
) -> Result<EventPage> {
    let path = db_path.to_path_buf();
    let streams = streams.to_vec();
    tokio::task::spawn_blocking(move || {
        let conn = reader::open_readonly(&path)?;
        reader::read_events(&conn, after_pk, &streams, include_synced, page_limit)
    })
    .await
    .context("joining knowledgeC.db read thread")?
}

/// Append one rejected event to the park file as a single JSON line.
fn park(
    rejected_path: &Path,
    pk: i64,
    stream: &str,
    event: &CaptureEvent,
    reason: &str,
    now_ms: i64,
) -> Result<()> {
    if let Some(parent) = rejected_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating park dir {}", parent.display()))?;
    }
    let record = ParkRecord {
        pk,
        source_id: &event.source_id,
        kind: &event.kind,
        stream,
        reason: reason.to_owned(),
        rejected_at: server_ingest::wire_ts(now_ms).unwrap_or_default(),
        event,
    };
    let mut line = serde_json::to_vec(&record).context("serializing park record")?;
    line.push(b'\n');
    // A previous append may have died part-way (ENOSPC, EIO, power loss
    // before the data reached the disk) and left a torn fragment with no
    // newline; the cursor did not move past that event, so it comes back
    // here next tick. Start this record on its own line so replay skips
    // the fragment as one unreadable line instead of losing this record
    // with it.
    if !ends_with_newline(rejected_path)
        .with_context(|| format!("inspecting park file {}", rejected_path.display()))?
    {
        line.insert(0, b'\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rejected_path)
        .with_context(|| format!("opening park file {}", rejected_path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("appending to park file {}", rejected_path.display()))?;
    // The append licenses the cursor to step past the event: it has to be
    // on the disk, not in the page cache, before that happens.
    file.sync_data()
        .with_context(|| format!("syncing park file {}", rejected_path.display()))?;
    Ok(())
}

/// True if the park file is absent, empty, or ends with `\n`.
fn ends_with_newline(path: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err),
    };
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// `IngestError`'s message names the endpoint it posted to. The park file
/// keeps the full reason (it is the operator's own disk); the log line gets
/// the same text with every `http(s)://…` token replaced — including one
/// wrapped in punctuation, because reqwest's own transport errors end in
/// `for url (https://…)` and that form is exactly what an outage logs.
fn redact_urls(message: &str) -> String {
    message
        .split(' ')
        .map(|token| {
            let Some(start) = ["https://", "http://"]
                .iter()
                .filter_map(|scheme| token.find(scheme))
                .min()
            else {
                return token.to_owned();
            };
            // Cut at the closing paren when present, otherwise to the end
            // of the token.
            let end = token[start..].find(')').map_or(token.len(), |i| start + i);
            format!("{}<server>{}", &token[..start], &token[end..])
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Human-readable summary of one row, for `--check-knowledge` and
/// `--import-knowledge-once`: shape only — stream, pk, the app or device
/// the row is about, and its duration. Never a web domain (a `/app/webUsage`
/// row prints as its browser, a web-centre notification as `web`) and never
/// a URL, so a terminal transcript of the probe is safe to paste anywhere.
/// A bundle the shipping path suppresses (`facts::is_suppressed_bundle`:
/// password managers and the user's own exclusions) prints as
/// `<suppressed>` for the same reason — the probe reads the raw reader
/// output, before that policy runs, and must not be the one place it leaks.
pub fn describe(row: &KnowledgeRow, config: &crate::config::KnowledgeConfig) -> String {
    let redact = |bundle: &str| -> String {
        if facts::is_suppressed_bundle(bundle, config) {
            "<suppressed>".to_owned()
        } else {
            bundle.to_owned()
        }
    };
    let subject = match row.stream.as_str() {
        facts::STREAM_APP_USAGE | facts::STREAM_MEDIA_USAGE => row
            .value_string
            .as_deref()
            .map(redact)
            .unwrap_or_else(|| "?".to_owned()),
        facts::STREAM_NOTIFICATION => match row.notification_bundle_id.as_deref() {
            Some(b) if b.starts_with("_WEB_CENTER_:") => "web".to_owned(),
            Some(b) if b.starts_with("_SYSTEM_CENTER_:") => "system".to_owned(),
            Some(b) => redact(b),
            None => "?".to_owned(),
        },
        facts::STREAM_BLUETOOTH => format!(
            "{} ({})",
            row.bt_name.as_deref().unwrap_or("?"),
            if row.value_integer == Some(1) {
                "connected"
            } else {
                "disconnected"
            }
        ),
        facts::STREAM_DISPLAY => if row.value_integer == Some(1) {
            "on"
        } else {
            "off"
        }
        .to_owned(),
        facts::STREAM_WEB_USAGE => row
            .value_string
            .as_deref()
            .map(redact)
            .unwrap_or_else(|| "com.apple.Safari".to_owned()),
        facts::STREAM_INTENTS => row
            .source_bundle_id
            .as_deref()
            .map(redact)
            .unwrap_or_else(|| "?".to_owned()),
        _ => "?".to_owned(),
    };
    format!(
        "stream={} pk={} subject={subject} duration={} origin={}",
        row.stream,
        row.pk,
        facts::humanize_duration(row.duration_secs()),
        if row.synced { "synced" } else { "local" }
    )
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "ci-capture-knowledge-cursor-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    /// The cursor has to survive the file, not just the process. A serde
    /// regression here would be SILENT: the lane would restart from the
    /// first-run floor on every launch and CI-Server's `source_id` dedup
    /// would hide it, so nothing would look wrong except the traffic — and
    /// a lost `stalled_head` would let a poison event wedge the lane past
    /// every restart.
    #[test]
    fn a_full_cursor_round_trips_through_the_file() {
        let path = tmp();
        let _ = std::fs::remove_file(&path);

        let saved = Cursor {
            store_uuid: Some("8C2F1A3E-0000-4000-8000-000000000001".to_owned()),
            last_pk: Some(31_337),
            last_digest_day: chrono::NaiveDate::from_ymd_opt(2026, 9, 14),
            backfill_window_start: Some(807_321_600.5),
            backoff_ticks: 3,
            backoff_level: 3,
            digest_backoff_ticks: 17,
            stalled_head: Some(("knowledge:ABC".to_owned(), 1_789_000_000_000)),
        };
        save_cursor(&path, &saved).expect("save");

        let loaded = load_cursor(&path);
        assert_eq!(loaded, saved, "cursor did not survive the file");
        assert_eq!(
            loaded.backoff_level, 3,
            "the doubling level survives the countdown"
        );
        assert_eq!(loaded.backfill_window_start, Some(807_321_600.5));

        let _ = std::fs::remove_file(&path);
    }

    /// A cursor written by an older build that knew fewer fields still
    /// loads — every field defaults — so adding state to the lane never
    /// restarts existing installs from the floor.
    #[test]
    fn a_cursor_with_missing_fields_loads_with_defaults() {
        let path = tmp();
        std::fs::write(&path, br#"{"last_pk": 42}"#).expect("write");

        let loaded = load_cursor(&path);
        assert_eq!(loaded.last_pk, Some(42));
        assert_eq!(loaded.store_uuid, None);
        assert_eq!(loaded.backoff_ticks, 0);
        assert_eq!(loaded.backoff_level, 0);
        assert_eq!(loaded.digest_backoff_ticks, 0);
        assert_eq!(loaded.backfill_window_start, None);
        assert_eq!(loaded.stalled_head, None);
        assert_eq!(loaded.last_digest_day, None);

        let _ = std::fs::remove_file(&path);
    }

    /// Garbage on disk restarts the lane rather than killing it — the same
    /// reasoning `config.rs` documents for a malformed config.
    #[test]
    fn an_unreadable_cursor_file_falls_back_to_the_first_run() {
        let path = tmp();
        std::fs::write(&path, b"{not json at all").expect("write");
        assert_eq!(load_cursor(&path), Cursor::default());

        let _ = std::fs::remove_file(&path);
        // A file that was never written is the same "start from the top" case.
        assert_eq!(load_cursor(&path), Cursor::default());
    }

    /// The log line for a rejection must not carry the endpoint it was
    /// posted to; the park file keeps the full text. reqwest's transport
    /// errors wrap the URL as `for url (https://…)` — the form an outage
    /// logs most — so a token that merely *contains* a scheme is redacted
    /// too, keeping its punctuation.
    #[test]
    fn redact_urls_replaces_every_http_token() {
        assert_eq!(
            redact_urls(
                "event ingest to https://memory.example.com/v1/events failed with HTTP 400"
            ),
            "event ingest to <server> failed with HTTP 400"
        );
        assert_eq!(
            redact_urls(
                "sending event ingest request to https://memory.example.com/v1/events: error sending request for url (https://memory.example.com/v1/events) (transient)"
            ),
            "sending event ingest request to <server> error sending request for url (<server>) (transient)"
        );
        assert_eq!(redact_urls("no url here"), "no url here");
    }

    /// Windows has `USERPROFILE` and no `HOME`; the path must resolve
    /// there too so `db_access_status` reaches the cfg-gated probe. On
    /// this host the assertion is only that the resolver does not depend
    /// on anything beyond a home directory.
    #[test]
    fn db_path_resolves_without_home_env() {
        assert!(db_path().is_ok());
    }

    /// Off macOS there is no Knowledge store and no TCC, and the lane must
    /// say so (`NotRequired`) rather than show Full Disk Access copy for a
    /// permission that does not exist there.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn db_access_status_is_not_required_off_macos() {
        assert_eq!(db_access_status(), PermissionStatus::NotRequired);
    }

    /// A park append that died part-way (ENOSPC, EIO) leaves a fragment
    /// with no newline, and the event comes back next tick because the
    /// cursor never moved past it. The retry must start on a fresh line so
    /// replay skips one unreadable fragment instead of losing the record
    /// that was glued onto it.
    #[test]
    fn park_starts_a_new_line_after_a_torn_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let rejected = dir.path().join("knowledge-rejected.jsonl");
        std::fs::write(&rejected, br#"{"pk":1,"source_id":"knowledge:tor"#).unwrap();

        let event = CaptureEvent {
            source_system: "knowledge",
            source_id: "knowledge:U2".to_owned(),
            kind: "knowledge_app_usage".to_owned(),
            occurred_at: "2026-09-01T00:00:00.000Z".to_owned(),
            ended_at: None,
            observed_at: Some("2026-09-01T00:05:00.000Z".to_owned()),
            title: Some("Used Alpha for 5 min".to_owned()),
            description: None,
            payload: serde_json::json!({}),
            capture_tick_id: None,
        };
        park(
            &rejected,
            2,
            facts::STREAM_APP_USAGE,
            &event,
            "HTTP 400",
            1_788_220_800_000,
        )
        .unwrap();

        let text = std::fs::read_to_string(&rejected).unwrap();
        assert_eq!(text.matches('\n').count(), 2, "{text:?}");
        assert!(text.ends_with('\n'));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            serde_json::from_str::<serde_json::Value>(lines[0]).is_err(),
            "the fragment is its own unreadable line"
        );
        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(record["pk"], 2);
        assert_eq!(record["source_id"], "knowledge:U2");

        // A well-terminated file gains exactly one line per park.
        park(
            &rejected,
            3,
            facts::STREAM_APP_USAGE,
            &event,
            "HTTP 400",
            1_788_220_800_000,
        )
        .unwrap();
        let text = std::fs::read_to_string(&rejected).unwrap();
        assert_eq!(text.matches('\n').count(), 3, "{text:?}");
    }

    fn base_row() -> KnowledgeRow {
        KnowledgeRow {
            pk: 7,
            uuid: None,
            stream: facts::STREAM_NOTIFICATION.to_owned(),
            value_string: None,
            value_integer: None,
            start_unix_ms: 1_788_220_800_000,
            end_unix_ms: 1_788_220_800_000,
            created_unix_ms: None,
            tz_offset_secs: Some(-25_200),
            source_bundle_id: None,
            synced: false,
            origin_device_model: None,
            web_domain: Some("secret-host.example".to_owned()),
            usage_type: None,
            usage_trusted: None,
            notification_bundle_id: Some("_WEB_CENTER_:secret-host.example".to_owned()),
            media_title: None,
            media_artist: None,
            media_album: None,
            media_type: None,
            intent_class: None,
            intent_verb: None,
            intent_direction: None,
            intent_donated_by_siri: None,
            bt_name: None,
            bt_device_type: None,
            bt_is_apple_audio: None,
            bt_is_user_wearing: None,
        }
    }

    /// `describe` is what `--check-knowledge` prints, so it is the one place
    /// a domain could reach a terminal transcript. Web-centre notifications
    /// and Safari rows must print without their host.
    #[test]
    fn describe_never_prints_a_web_host() {
        let config = crate::config::KnowledgeConfig::default();
        let notification = describe(&base_row(), &config);
        assert!(!notification.contains("secret-host"), "{notification}");
        assert!(notification.contains("subject=web"), "{notification}");

        let web = KnowledgeRow {
            stream: facts::STREAM_WEB_USAGE.to_owned(),
            value_string: Some("com.apple.Safari".to_owned()),
            end_unix_ms: 1_788_220_800_000 + 65_000,
            ..base_row()
        };
        let line = describe(&web, &config);
        assert!(!line.contains("secret-host"), "{line}");
        assert!(line.contains("subject=com.apple.Safari"), "{line}");
        assert!(line.contains("duration=1 min"), "{line}");
    }

    /// The probe reads raw reader output, before the shipping path's
    /// suppression runs, so a password manager's bundle would otherwise
    /// reach the transcript the wire never carries. Same rule as the
    /// wire: code-side list and the user's own exclusions.
    #[test]
    fn describe_redacts_sensitive_bundles() {
        let config = crate::config::KnowledgeConfig::default();
        let row = KnowledgeRow {
            stream: facts::STREAM_APP_USAGE.to_owned(),
            value_string: Some("com.1password.1password".to_owned()),
            ..base_row()
        };
        let line = describe(&row, &config);
        assert!(!line.contains("1password"), "{line}");
        assert!(line.contains("subject=<suppressed>"), "{line}");

        let notif = KnowledgeRow {
            stream: facts::STREAM_NOTIFICATION.to_owned(),
            notification_bundle_id: Some("com.apple.Passwords".to_owned()),
            ..base_row()
        };
        assert!(!describe(&notif, &config).contains("Passwords"));

        let excluded = crate::config::KnowledgeConfig {
            excluded_bundle_ids: vec!["com.example.Private".to_owned()],
            ..config
        };
        let intent = KnowledgeRow {
            stream: facts::STREAM_INTENTS.to_owned(),
            source_bundle_id: Some("com.example.Private".to_owned()),
            ..base_row()
        };
        let line = describe(&intent, &excluded);
        assert!(!line.contains("Private"), "{line}");
        assert!(line.contains("subject=<suppressed>"), "{line}");
    }
}

#[cfg(test)]
mod digest_tests {
    //! The four digest gates and the cursor rule, against the e2e fixture
    //! store and mock server, with `now` injected in a fixed offset
    //! (UTC−7) and every date a literal — nothing here depends on the
    //! zone or clock of the machine running the tests, except the last
    //! test, which drives the real `drain_once` hook and so uses the real
    //! clock (its fixture days are all safely in the past).

    use std::time::Duration;

    use chrono::{DateTime, FixedOffset};
    use uuid::Uuid;

    use super::e2e::{Lane, lane_config, source_ids, spawn_mock_server, status_rule};
    use super::facts::{STREAM_APP_USAGE, STREAM_DISPLAY};
    use super::reader::{self, FixtureRow};
    use super::{Cursor, DigestOutcome, digest_once, drain_once, load_cursor};

    /// 2026-09-14 as days since 2001-01-01.
    const D: i64 = 9_387;
    const TZ_PDT: i64 = -25_200;

    fn date(day: i64) -> chrono::NaiveDate {
        reader::day_ordinal_to_date(day).unwrap()
    }

    /// Cocoa seconds `hours` after local midnight of `day` in PDT.
    fn at(day: i64, hours: f64) -> f64 {
        reader::local_midnight_cocoa_secs(day, TZ_PDT) + hours * 3_600.0
    }

    /// `now` as an instant `hours` after local midnight of `day`, carried
    /// in the UTC−7 offset.
    fn now_at(day: i64, hours: f64) -> DateTime<FixedOffset> {
        let unix = at(day, hours) as i64 + reader::COCOA_EPOCH_OFFSET_MS / 1000;
        DateTime::from_timestamp(unix, 0)
            .unwrap()
            .with_timezone(&FixedOffset::west_opt(7 * 3_600).unwrap())
    }

    /// An `/app/usage` session of `bundle` on `day`, `start_h` hours after
    /// local midnight, `minutes` long.
    fn session(pk: i64, day: i64, start_h: f64, minutes: f64, bundle: &str) -> FixtureRow {
        let start = at(day, start_h);
        FixtureRow::new(STREAM_APP_USAGE, start, start + minutes * 60.0)
            .pk(pk)
            .uuid(&format!("U{pk}"))
            .value_string(bundle)
    }

    fn display_on(pk: i64, start: f64, end: f64) -> FixtureRow {
        FixtureRow::new(STREAM_DISPLAY, start, end)
            .pk(pk)
            .value_integer(1)
    }

    /// Rows that make day `D` covered (a session on D−1, created before
    /// D 00:00) and give it some activity.
    fn covered_day_d() -> Vec<FixtureRow> {
        vec![
            session(1, D - 1, 20.0, 10.0, "com.example.Alpha"),
            session(2, D, 9.0, 60.0, "com.example.Alpha"),
            session(3, D, 14.0, 30.0, "com.example.Beta"),
        ]
    }

    /// The production hook with the e2e sizes and the real clock.
    async fn drain(
        lane: &Lane,
        config: &crate::config::AppConfig,
        cursor: &mut Cursor,
    ) -> super::PollOutcome {
        drain_once(
            &lane.db_path,
            config,
            Uuid::nil(),
            cursor,
            &lane.cursor_path,
            &lane.rejected_path,
            10,
            10,
            Duration::from_secs(30),
        )
        .await
        .unwrap()
    }

    async fn digest(
        lane: &Lane,
        config: &crate::config::AppConfig,
        cursor: &mut Cursor,
        now: DateTime<FixedOffset>,
    ) -> DigestOutcome {
        digest_once(
            &lane.db_path,
            config,
            Uuid::nil(),
            cursor,
            &lane.cursor_path,
            now,
        )
        .await
        .unwrap()
    }

    fn kinds(body: &str) -> Vec<String> {
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        json["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The store's rolling purge has eaten the start of its oldest day:
    /// `coverage_start` sits *inside* day X, so X's early sessions may be
    /// gone and a digest of it would read as a quiet day. X is skipped for
    /// good (the cursor steps past it), X+1 is emitted, and X+2 — whose
    /// flush evidence has not arrived — is held.
    #[tokio::test(flavor = "multi_thread")]
    async fn coverage_skips_the_purge_truncated_oldest_day() {
        let x = D - 2;
        let lane = Lane::new(&[
            session(1, x, 9.0, 60.0, "com.example.Alpha"),
            session(2, x + 1, 9.0, 60.0, "com.example.Alpha"),
            session(3, x + 1, 11.0, 30.0, "com.example.Beta"),
            session(4, x + 2, 9.0, 60.0, "com.example.Alpha"),
        ]);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();

        let outcome = digest(&lane, &config, &mut cursor, now_at(x + 4, 12.0)).await;

        assert_eq!(outcome.days_skipped, 1, "X is before coverage");
        assert_eq!(outcome.days_emitted, 1, "X+1");
        assert_eq!(outcome.held, Some(date(x + 2)));
        assert_eq!(outcome.ingested, 3, "two apps + summary");
        assert_eq!(cursor.last_digest_day, Some(date(x + 1)));
        assert_eq!(
            load_cursor(&lane.cursor_path).last_digest_day,
            Some(date(x + 1)),
            "persisted"
        );

        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].0, "/v1/events");
            assert_eq!(
                source_ids(&requests[0].2),
                vec![
                    // The lane runs with a nil identity here, so the fixture
                    // store's UUID is the per-machine scope.
                    format!("knowledge:daily:STORE-A:{}:com.example.Alpha", date(x + 1)),
                    format!("knowledge:daily:STORE-A:{}:com.example.Beta", date(x + 1)),
                    format!("knowledge:daily:STORE-A:{}:summary", date(x + 1)),
                ]
            );
        }

        // Next tick, nothing new: X+2 is still held and nothing re-posts.
        let again = digest(&lane, &config, &mut cursor, now_at(x + 4, 13.0)).await;
        assert_eq!(again.days_emitted, 0);
        assert_eq!(again.days_skipped, 0);
        assert_eq!(again.held, Some(date(x + 2)));
        assert_eq!(recorded.lock().unwrap().len(), 1);
    }

    /// `knowledged` writes `/app/usage` up to eleven hours late. Being past
    /// D+1 02:00 is not enough: until a session that began on D+1 has been
    /// written, D's tail may still be in the daemon's buffer, so D is held
    /// — and released the moment that later row appears.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_flush_evidence_gate_holds_a_day_then_releases_it() {
        let lane = Lane::new(&covered_day_d());
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();

        // Before D+1 02:00: not complete at all.
        let early = digest(&lane, &config, &mut cursor, now_at(D + 1, 1.0)).await;
        assert_eq!(early.held, Some(date(D)));
        assert_eq!(early.days_skipped, 1, "D−1 is before coverage");
        assert_eq!(cursor.last_digest_day, Some(date(D - 1)));

        // After D+1 02:00 but no row on D+1 yet: still held.
        let waiting = digest(&lane, &config, &mut cursor, now_at(D + 1, 3.0)).await;
        assert_eq!(waiting.held, Some(date(D)));
        assert_eq!(waiting.days_emitted, 0);
        assert!(
            recorded.lock().unwrap().is_empty(),
            "nothing posted while held"
        );

        // A session on D+1 lands: D's tail is proven flushed.
        {
            let conn = rusqlite::Connection::open(&lane.db_path).unwrap();
            reader::insert_row(&conn, &session(4, D + 1, 0.5, 5.0, "com.example.Alpha"));
        }
        let released = digest(&lane, &config, &mut cursor, now_at(D + 1, 3.0)).await;
        assert_eq!(released.days_emitted, 1);
        assert_eq!(
            released.held,
            Some(date(D + 1)),
            "D+1 itself is not complete"
        );
        assert_eq!(cursor.last_digest_day, Some(date(D)));
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            kinds(&requests[0].2),
            vec![
                "knowledge_daily_app_usage",
                "knowledge_daily_app_usage",
                "knowledge_daily_summary"
            ]
        );
    }

    /// A session that runs past midnight into a day nothing else touches:
    /// the tail is that day's whole activity and must be digested, not
    /// stepped over for good when the next day with a session of its own
    /// is emitted (the cursor only ever moves forward).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_day_whose_only_activity_is_a_midnight_crossing_tail_is_digested() {
        let lane = Lane::new(&[
            session(1, D - 1, 20.0, 10.0, "com.example.Alpha"),
            // 22:00 on D → 02:00 on D+1.
            session(2, D, 22.0, 240.0, "com.example.Alpha"),
            // Flush evidence for D+1, and D+2's own candidate.
            session(3, D + 2, 9.0, 60.0, "com.example.Alpha"),
        ]);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();

        let outcome = digest(&lane, &config, &mut cursor, now_at(D + 3, 12.0)).await;

        assert_eq!(outcome.days_skipped, 1, "D−1 is before coverage");
        assert_eq!(outcome.days_emitted, 2, "D and its tail day D+1");
        assert_eq!(outcome.held, Some(date(D + 2)));
        assert_eq!(cursor.last_digest_day, Some(date(D + 1)));
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            source_ids(&requests[1].2),
            vec![
                format!("knowledge:daily:STORE-A:{}:com.example.Alpha", date(D + 1)),
                format!("knowledge:daily:STORE-A:{}:summary", date(D + 1)),
            ]
        );
        let body: serde_json::Value = serde_json::from_str(&requests[1].2).unwrap();
        assert_eq!(
            body["events"][0]["payload"]["total_secs"], 7_200,
            "00:00–02:00 clipped tail"
        );
    }

    /// A Mac shut down for a long weekend never produces flush evidence
    /// for its last day. At D+3 00:00 the day is released anyway (with a
    /// warning) rather than waiting forever — and if the display timeline
    /// has not closed past midnight either, the day ships *without*
    /// `display_on_secs` rather than with the known undercount, because a
    /// re-send could never correct it (stable source id, server-side
    /// dedup).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_d_plus_3_fallback_releases_a_day_without_flush_evidence() {
        let mut rows = covered_day_d();
        rows.push(display_on(10, at(D - 1, 20.0), at(D - 1, 21.0))); // coverage reaches D
        rows.push(display_on(11, at(D, 10.0), at(D, 18.0))); // timeline ends 18:00 on D
        let lane = Lane::new(&rows);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();

        let just_before = digest(&lane, &config, &mut cursor, now_at(D + 2, 23.99)).await;
        assert_eq!(just_before.held, Some(date(D)));
        assert_eq!(just_before.days_emitted, 0);

        let at_fallback = digest(&lane, &config, &mut cursor, now_at(D + 3, 0.0)).await;
        assert_eq!(at_fallback.days_emitted, 1);
        assert_eq!(at_fallback.held, None);
        assert_eq!(cursor.last_digest_day, Some(date(D)));
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let json: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        let summary = &json["events"][2];
        assert_eq!(summary["kind"], "knowledge_daily_summary");
        assert!(
            summary["payload"].get("display_on_secs").is_none(),
            "an open display timeline ships no figure: {summary}"
        );
    }

    /// A display left on across D's midnight (a desktop with "prevent
    /// display sleep") never closes its span, so its timeline never reaches
    /// D+1 00:00 while it stays on. With flush evidence present the day
    /// would otherwise be held without bound — and every later day behind
    /// it, until the purge turned them into skips. The D+3 valve releases
    /// the display hold too.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_d_plus_3_valve_also_releases_a_display_closure_hold() {
        let mut rows = covered_day_d();
        rows.push(session(4, D + 1, 9.0, 5.0, "com.example.Alpha")); // flush evidence
        rows.push(display_on(10, at(D - 1, 20.0), at(D - 1, 21.0))); // coverage reaches D
        rows.push(display_on(11, at(D, 10.0), at(D, 18.0))); // never closes past D+1 00:00
        let lane = Lane::new(&rows);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();

        let held = digest(&lane, &config, &mut cursor, now_at(D + 2, 23.99)).await;
        assert_eq!(held.held, Some(date(D)));
        assert_eq!(held.days_emitted, 0);
        assert!(recorded.lock().unwrap().is_empty());

        let released = digest(&lane, &config, &mut cursor, now_at(D + 3, 0.0)).await;
        assert_eq!(released.days_emitted, 1);
        assert_eq!(cursor.last_digest_day, Some(date(D)));
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let json: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        assert!(
            json["events"][2]["payload"]
                .get("display_on_secs")
                .is_none(),
            "{}",
            json["events"][2]
        );
    }

    /// Pins the documented approximation on a DST-transition day: the
    /// day's midnight instants are taken in its *modal* zone. Shaped like
    /// a fall-back day in US Pacific — a few sessions before the 02:00
    /// switch still carry UTC−7, the bulk of the day carries UTC−8 — so
    /// `occurred_at` is D 00:00 PST (08:00Z) although the day really began
    /// at 00:00 PDT (07:00Z), and `first_activity` (07:30Z, from the
    /// per-row clipping) precedes it by up to an hour. `ended_at` and the
    /// totals are exact; the wire is stable, so any change here is a
    /// deliberate one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dst_transition_day_files_its_midnights_in_the_modal_zone() {
        const TZ_PST: i64 = -28_800;
        let pdt_midnight = reader::local_midnight_cocoa_secs(D, TZ_PDT); // 07:00Z
        let pst_midnight = reader::local_midnight_cocoa_secs(D, TZ_PST); // 08:00Z
        let rows = vec![
            // Coverage: created before either midnight of D.
            session(1, D - 1, 20.0, 10.0, "com.example.Alpha"),
            // 00:30–01:00 PDT, before the switch: still UTC−7.
            FixtureRow::new(
                STREAM_APP_USAGE,
                pdt_midnight + 1_800.0,
                pdt_midnight + 3_600.0,
            )
            .pk(2)
            .uuid("U2")
            .value_string("com.example.Alpha"),
            // The rest of the day in UTC−8: 10:00–11:00 and 14:00–14:30 PST.
            FixtureRow::new(
                STREAM_APP_USAGE,
                pst_midnight + 36_000.0,
                pst_midnight + 39_600.0,
            )
            .pk(3)
            .uuid("U3")
            .value_string("com.example.Alpha")
            .tz_offset_secs(Some(TZ_PST)),
            FixtureRow::new(
                STREAM_APP_USAGE,
                pst_midnight + 50_400.0,
                pst_midnight + 52_200.0,
            )
            .pk(4)
            .uuid("U4")
            .value_string("com.example.Beta")
            .tz_offset_secs(Some(TZ_PST)),
            // Flush evidence on D+1, in PST.
            FixtureRow::new(
                STREAM_APP_USAGE,
                pst_midnight + 86_400.0 + 32_400.0,
                pst_midnight + 86_400.0 + 32_700.0,
            )
            .pk(5)
            .uuid("U5")
            .value_string("com.example.Alpha")
            .tz_offset_secs(Some(TZ_PST)),
        ];
        let lane = Lane::new(&rows);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();

        let outcome = digest(&lane, &config, &mut cursor, now_at(D + 1, 12.0)).await;
        assert_eq!(outcome.days_emitted, 1, "{outcome:?}");
        assert_eq!(cursor.last_digest_day, Some(date(D)));

        let requests = recorded.lock().unwrap();
        let json: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        let summary = &json["events"][2];
        assert_eq!(summary["kind"], "knowledge_daily_summary");
        assert_eq!(summary["payload"]["tz_offset_secs"], TZ_PST, "modal zone");
        assert_eq!(summary["occurred_at"], "2026-09-14T08:00:00.000Z");
        assert_eq!(summary["ended_at"], "2026-09-15T08:00:00.000Z");
        assert_eq!(
            summary["payload"]["first_activity"], "2026-09-14T07:30:00.000Z",
            "the pre-switch session is in D and precedes the modal midnight"
        );
        assert_eq!(
            summary["payload"]["total_secs"],
            1_800 + 3_600 + 1_800,
            "totals are per-row and unaffected"
        );
    }

    /// The display "on" span that straddles midnight is written only when
    /// it ends. While `/display/isBacklit` is enabled and its timeline has
    /// not closed past D+1 00:00, D is held rather than summarized with an
    /// undercounted `display_on_secs`; once the span closes the day ships
    /// with the clipped figure. Disabling the stream removes both the hold
    /// and the figure.
    #[tokio::test(flavor = "multi_thread")]
    async fn display_closure_holds_the_day_until_the_midnight_span_closes() {
        let mut rows = covered_day_d();
        rows.push(session(4, D + 1, 9.0, 5.0, "com.example.Alpha")); // flush evidence
        rows.push(display_on(10, at(D - 1, 20.0), at(D - 1, 21.0))); // display coverage reaches D
        rows.push(display_on(11, at(D, 10.0), at(D, 18.0))); // 8 h inside D; timeline ends 18:00
        let lane = Lane::new(&rows);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();
        let now = now_at(D + 1, 12.0);

        let held = digest(&lane, &config, &mut cursor, now).await;
        assert_eq!(held.held, Some(date(D)));
        assert_eq!(held.days_emitted, 0);
        assert!(recorded.lock().unwrap().is_empty());

        // The span across midnight closes: 23:00 → 01:00 next day.
        {
            let conn = rusqlite::Connection::open(&lane.db_path).unwrap();
            reader::insert_row(&conn, &display_on(12, at(D, 23.0), at(D + 1, 1.0)));
        }
        let released = digest(&lane, &config, &mut cursor, now).await;
        assert_eq!(released.days_emitted, 1);
        assert_eq!(cursor.last_digest_day, Some(date(D)));
        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let json: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
            let summary = &json["events"][2];
            assert_eq!(summary["kind"], "knowledge_daily_summary");
            assert_eq!(
                summary["payload"]["display_on_secs"],
                8 * 3_600 + 3_600,
                "8 h + the hour before midnight, clipped"
            );
        }

        // Same store, display disabled: no hold, no figure.
        let (origin, recorded) = spawn_mock_server(vec![]);
        let mut config = lane_config(&origin);
        config.knowledge.disabled_streams = vec![STREAM_DISPLAY.to_owned()];
        let mut cursor = Cursor::default();
        {
            let conn = rusqlite::Connection::open(&lane.db_path).unwrap();
            conn.execute("DELETE FROM ZOBJECT WHERE Z_PK = 12", [])
                .unwrap();
        }
        let outcome = digest(&lane, &config, &mut cursor, now).await;
        assert_eq!(outcome.days_emitted, 1);
        let requests = recorded.lock().unwrap();
        let json: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        assert!(
            json["events"][2]["payload"]
                .get("display_on_secs")
                .is_none(),
            "{}",
            json["events"][2]
        );
    }

    /// A store without any display rows (another macOS, or the stream
    /// never observed) must not wedge every digest behind gate 3.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_with_no_display_rows_does_not_hold_on_display_closure() {
        let mut rows = covered_day_d();
        rows.push(session(4, D + 1, 9.0, 5.0, "com.example.Alpha"));
        let lane = Lane::new(&rows);
        let (origin, _recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();
        let outcome = digest(&lane, &config, &mut cursor, now_at(D + 1, 12.0)).await;
        assert_eq!(outcome.days_emitted, 1);
        assert_eq!(cursor.last_digest_day, Some(date(D)));
    }

    /// The cursor rule: `last_digest_day` moves only once the day's batch
    /// is acknowledged. A 503 leaves it where it was and the day is rebuilt
    /// and re-posted next tick with the same source ids.
    #[tokio::test(flavor = "multi_thread")]
    async fn last_digest_day_advances_only_after_the_batch_is_acked() {
        let mut rows = covered_day_d();
        rows.push(session(4, D + 1, 9.0, 5.0, "com.example.Alpha"));
        let lane = Lane::new(&rows);
        let (origin, recorded) =
            spawn_mock_server(vec![status_rule("knowledge_daily_summary", 503, false)]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();
        let now = now_at(D + 1, 12.0);

        let failed = digest(&lane, &config, &mut cursor, now).await;
        assert_eq!(failed.days_emitted, 0);
        assert_eq!(failed.held, Some(date(D)));
        assert_eq!(
            cursor.last_digest_day,
            Some(date(D - 1)),
            "only the uncovered day was stepped over"
        );

        let retried = digest(&lane, &config, &mut cursor, now).await;
        assert_eq!(retried.days_emitted, 1);
        assert_eq!(cursor.last_digest_day, Some(date(D)));
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 2, "one failed, one acked");
        assert_eq!(
            source_ids(&requests[0].2),
            source_ids(&requests[1].2),
            "the rebuilt day carries the same ids, so a partial ack is only a duplicate"
        );
    }

    /// A permanently rejected digest batch is a shape bug: the day is held
    /// (never parked — that would drop a whole day's summary) but the next
    /// attempt waits `MAX_BACKOFF_TICKS` ticks instead of re-posting and
    /// error-logging every tick, and the raw lane keeps running meanwhile.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_permanently_rejected_digest_holds_the_day_and_backs_off() {
        let mut rows = covered_day_d();
        rows.push(session(4, D + 1, 9.0, 5.0, "com.example.Alpha"));
        let lane = Lane::new(&rows);
        let (origin, recorded) =
            spawn_mock_server(vec![status_rule("knowledge_daily_summary", 400, true)]);
        let config = lane_config(&origin);
        // The raw rows are already acked, so `drain_once` below has only
        // the digest stage to run.
        let mut cursor = Cursor {
            store_uuid: Some("STORE-A".to_owned()),
            last_pk: Some(4),
            ..Cursor::default()
        };

        let outcome = digest(&lane, &config, &mut cursor, now_at(D + 1, 12.0)).await;
        assert_eq!(outcome.days_emitted, 0);
        assert_eq!(outcome.held, Some(date(D)));
        assert_eq!(cursor.last_digest_day, Some(date(D - 1)));
        assert_eq!(cursor.digest_backoff_ticks, super::MAX_BACKOFF_TICKS);
        assert_eq!(
            load_cursor(&lane.cursor_path).digest_backoff_ticks,
            super::MAX_BACKOFF_TICKS,
            "persisted"
        );
        assert_eq!(recorded.lock().unwrap().len(), 1);

        // The production hook counts the window down without posting.
        let raw = drain(&lane, &config, &mut cursor).await;
        assert_eq!(raw.ingested, 0);
        assert_eq!(recorded.lock().unwrap().len(), 1, "no second POST");
        assert_eq!(cursor.digest_backoff_ticks, super::MAX_BACKOFF_TICKS - 1);
        assert_eq!(cursor.last_digest_day, Some(date(D - 1)), "still held");
    }

    /// On a first run the digest honours `backfill_days` exactly as the raw
    /// lane does: days before the first whole day inside the window are
    /// skipped, because their sessions were never shipped either. The
    /// window is the one the raw lane pinned in the cursor at its first
    /// tick — not recomputed from `now` — so a digest pass that first runs
    /// days later (backfill spans ticks, the server was down) still floors
    /// at the same day instead of stepping past one whose sessions shipped.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_run_backfill_floor_skips_days_before_the_window() {
        let lane = Lane::new(&[
            session(1, D - 2, 9.0, 60.0, "com.example.Alpha"),
            session(2, D - 1, 9.0, 60.0, "com.example.Alpha"),
            session(3, D, 9.0, 60.0, "com.example.Alpha"),
            session(4, D + 1, 9.0, 60.0, "com.example.Alpha"),
        ]);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let mut config = lane_config(&origin);
        // The raw window started at D noon (a 2-day window taken at D+2
        // noon), so the first whole day inside it is D+1.
        let window_start = Some(reader::local_midnight_cocoa_secs(D, TZ_PDT) + 12.0 * 3_600.0);
        let mut cursor = Cursor {
            backfill_window_start: window_start,
            ..Cursor::default()
        };

        let outcome = digest(&lane, &config, &mut cursor, now_at(D + 2, 12.0)).await;
        assert_eq!(
            outcome.days_skipped, 3,
            "D−2 (coverage), D−1 and D (window)"
        );
        assert_eq!(outcome.days_emitted, 0);
        assert_eq!(outcome.held, Some(date(D + 1)), "no flush evidence yet");
        assert_eq!(cursor.last_digest_day, Some(date(D)));
        assert!(recorded.lock().unwrap().is_empty());

        // The regression: the same window digested a day later (the raw
        // backfill or an outage delayed the first pass past a midnight)
        // must still stop at D+1, never floor it away.
        let mut late = Cursor {
            backfill_window_start: window_start,
            ..Cursor::default()
        };
        let outcome = digest(&lane, &config, &mut late, now_at(D + 3, 12.0)).await;
        assert_eq!(outcome.days_skipped, 3, "the floor did not drift with now");
        assert_eq!(outcome.held, Some(date(D + 1)));
        assert_eq!(late.last_digest_day, Some(date(D)));

        // A config with the digest switched off does nothing at all.
        config.knowledge.daily_digest = false;
        let off = digest(&lane, &config, &mut cursor, now_at(D + 5, 12.0)).await;
        assert_eq!(off.days_emitted, 0);
        assert_eq!(off.held, None);
    }

    /// The raw lane's first tick is what pins the window: `drain_once`
    /// writes `backfill_window_start` into the cursor alongside the raw
    /// floor, once, and a later store reset leaves a started digest stage
    /// alone. (`backfill_days = 0` — the e2e default — means no floor.)
    #[tokio::test(flavor = "multi_thread")]
    async fn the_raw_lanes_first_tick_pins_the_digest_backfill_window() {
        let lane = Lane::new(&[]);
        let (origin, _recorded) = spawn_mock_server(vec![]);
        let mut config = lane_config(&origin);
        config.knowledge.backfill_days = 30;
        let mut cursor = Cursor::default();
        let before = chrono::Utc::now();

        drain(&lane, &config, &mut cursor).await;

        let window_start = cursor
            .backfill_window_start
            .expect("pinned at the first tick");
        let expected = (before.timestamp_millis() - reader::COCOA_EPOCH_OFFSET_MS) as f64 / 1000.0
            - 30.0 * 86_400.0;
        assert!(
            (window_start - expected).abs() < 5.0,
            "{window_start} vs {expected}"
        );
        assert_eq!(
            load_cursor(&lane.cursor_path).backfill_window_start,
            Some(window_start),
            "persisted"
        );

        // A store reset after digests have shipped does not re-floor them.
        cursor.last_digest_day = Some(date(D));
        cursor.store_uuid = Some("STORE-B".to_owned());
        cursor.backfill_window_start = Some(1.0);
        drain(&lane, &config, &mut cursor).await;
        assert_eq!(cursor.store_uuid, Some("STORE-A".to_owned()), "reset ran");
        assert_eq!(cursor.backfill_window_start, Some(1.0));
    }

    /// The production hook: `drain_once` ships the raw rows first and only
    /// then digests, so a day's summary never lands before its sessions.
    /// This one uses the real clock through the hook's `Local::now()`; the
    /// fixture days are 2026-09-01/02, safely final on any later day.
    #[tokio::test(flavor = "multi_thread")]
    async fn drain_once_digests_after_the_raw_rows_are_acked() {
        // 2026-09-01 is day 9 374.
        let sep_01 = 9_374;
        let lane = Lane::new(&[
            session(1, sep_01 - 1, 20.0, 10.0, "com.example.Alpha"), // coverage for 09-01
            session(2, sep_01, 9.0, 60.0, "com.example.Alpha"),
            session(3, sep_01, 15.0, 30.0, "com.example.Beta"),
            session(4, sep_01 + 1, 9.0, 5.0, "com.example.Alpha"), // flush evidence for 09-01
        ]);
        let (origin, recorded) = spawn_mock_server(vec![]);
        let config = lane_config(&origin);
        let mut cursor = Cursor::default();

        let outcome = drain_once(
            &lane.db_path,
            &config,
            Uuid::nil(),
            &mut cursor,
            &lane.cursor_path,
            &lane.rejected_path,
            10,
            10,
            Duration::from_secs(30),
        )
        .await
        .unwrap();

        assert_eq!(outcome.ingested, 4, "raw rows");
        assert_eq!(cursor.last_pk, Some(4));
        // 08-31 uncovered → skipped; 09-01 flushed → emitted; 09-02 has no
        // later row but is long past its D+3 → emitted by the fallback.
        assert_eq!(cursor.last_digest_day, Some(date(sep_01 + 1)));

        {
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 3, "{requests:?}");
            assert!(
                kinds(&requests[0].2)
                    .iter()
                    .all(|k| k == "knowledge_app_usage"),
                "raw rows first"
            );
            assert_eq!(
                kinds(&requests[1].2),
                vec![
                    "knowledge_daily_app_usage",
                    "knowledge_daily_app_usage",
                    "knowledge_daily_summary"
                ]
            );
            assert_eq!(
                kinds(&requests[2].2),
                vec!["knowledge_daily_app_usage", "knowledge_daily_summary"]
            );
            assert!(
                source_ids(&requests[1].2)[2].ends_with("2026-09-01:summary"),
                "{:?}",
                source_ids(&requests[1].2)
            );
        }

        // A second tick with nothing new posts nothing.
        let again = drain_once(
            &lane.db_path,
            &config,
            Uuid::nil(),
            &mut cursor,
            &lane.cursor_path,
            &lane.rejected_path,
            10,
            10,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert_eq!(again.ingested, 0);
        assert_eq!(recorded.lock().unwrap().len(), 3);
    }
}
