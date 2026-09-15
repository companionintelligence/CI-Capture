//! Reads `knowledgeC.db` — CoreDuet's activity store — directly, read-only.
//!
//! The store is a Core Data SQLite file: every event is one `ZOBJECT` row
//! tagged with a `ZSTREAMNAME` (`/app/usage`, `/bluetooth/isConnected`, …),
//! a start/end pair, and a pointer into `ZSTRUCTUREDMETADATA`, a 239-column
//! wide table whose column set drifts with every macOS release. Nothing
//! here is documented by Apple; the shapes below were measured on a live
//! store (macOS 27.0, ~30k rows) and are recorded as doc comments so the
//! next person does not have to re-measure them.
//!
//! Three findings drive the design:
//!
//! - **The cursor is `Z_PK`, not a date.** Rows are written when a session
//!   *ends* — `ZCREATIONDATE ≥ ZENDDATE` on 100% of rows, up to 11 h late
//!   for `/app/usage` — so a start-date cursor skipped 5.8% of rows on the
//!   reference store. `Z_PK` is monotonic and never reused within a store
//!   file; `Z_METADATA.Z_UUID` names the file so a replaced store (fresh
//!   install, migration) resets the cursor instead of silently skipping.
//! - **Timestamps are Cocoa seconds in a mixed storage class.** `ZSTARTDATE`
//!   and `ZENDDATE` are INTEGER for the in-scope streams while
//!   `ZCREATIONDATE` is REAL, and the sync-bookkeeping streams carry a
//!   year-0000 sentinel (`-63114076800`). Everything is read as
//!   `Option<f64>` and run through [`cocoa_secs_to_unix_ms`], which refuses
//!   anything that cannot be a real instant.
//! - **Only a named subset of metadata is ever selected.** The SELECT is
//!   built from a `PRAGMA table_info` probe so a column that this macOS does
//!   not have becomes `NULL` rather than a failed query — and the columns
//!   that would carry full page URLs, Bluetooth MAC addresses, notification
//!   identifiers, contact identifiers, or serialized intents are not in the
//!   wanted list at all, so they never enter process memory.

use anyhow::{Context, Result};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params_from_iter};

/// Milliseconds from the Unix epoch to Apple's Cocoa epoch
/// (2001-01-01T00:00:00Z). Same constant as `imessage::reader`; duplicated
/// rather than shared because the two lanes must stay independently
/// compilable (the imessage one is private to its module).
pub const COCOA_EPOCH_OFFSET_MS: i64 = 978_307_200_000;

/// Upper bound for a plausible Cocoa-seconds value: 4e9 s after 2001 is the
/// year 2127. Real rows sit around 8.1e8 (2026); the sync-bookkeeping
/// streams also carry `-63114076800` (year 0000), which the `< 0` guard
/// catches. Anything past this is a corrupt or hostile value, not a date.
const MAX_COCOA_SECS: f64 = 4.0e9;

/// Cocoa seconds (INTEGER or REAL in the store) → Unix milliseconds, or
/// `None` when the value cannot be an instant at all: NaN/∞, negative
/// (which is where the year-0000 sentinel lands), or beyond
/// [`MAX_COCOA_SECS`]. The fractional part is truncated to the millisecond,
/// matching how the imessage lane treats nanosecond dates.
///
/// `None` rather than a clamped value for the same reason the imessage
/// reader gives: a row that gets re-dated to something plausible is
/// acknowledged by the server and the cursor moves past it with nothing
/// logged; a row that is *dropped and counted* shows up in the poll outcome.
pub fn cocoa_secs_to_unix_ms(secs: f64) -> Option<i64> {
    if !secs.is_finite() || !(0.0..=MAX_COCOA_SECS).contains(&secs) {
        return None;
    }
    // The range check above keeps the product well inside i64.
    let ms_since_2001 = (secs * 1000.0).floor() as i64;
    let unix_ms = COCOA_EPOCH_OFFSET_MS.checked_add(ms_since_2001)?;
    chrono::DateTime::from_timestamp_millis(unix_ms).map(|_| unix_ms)
}

/// One `ZOBJECT` row, joined with its source and the wanted subset of its
/// structured metadata. Timestamps are already Unix milliseconds — the
/// reader refuses to emit a row it could not convert, so every consumer can
/// treat `start_unix_ms ≤ end_unix_ms` as an invariant.
///
/// What is deliberately **not** here, and has no field a future edit could
/// quietly populate: the page URL behind a `/app/webUsage` row, the MAC
/// address behind a Bluetooth row, notification and contact identifiers,
/// the serialized intent blob, and the raw `ZSOURCE.ZDEVICEID` (only the
/// derived `synced` flag and the peer's model name leave the reader).
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeRow {
    /// `ZOBJECT.Z_PK` — the cursor unit.
    pub pk: i64,
    /// `ZOBJECT.ZUUID`; the stable identity behind `source_id`. Nullable in
    /// the schema (only `Z_PK` is `NOT NULL`), always set on real rows.
    pub uuid: Option<String>,
    /// `ZSTREAMNAME`, e.g. `/app/usage`.
    pub stream: String,
    /// Stream-dependent: the bundle id for `/app/usage`, `/app/mediaUsage`
    /// and `/app/webUsage` (always `com.apple.Safari` there), the literal
    /// `Receive` for `/notification/usage`, the intent *domain*
    /// (`Messages`, `Calls`, `Maps`, …) for `/app/intents`.
    pub value_string: Option<String>,
    /// Stream-dependent: `1`/`0` for `/display/isBacklit` (on/off) and
    /// `/bluetooth/isConnected` (connected/disconnected); an opaque hash
    /// elsewhere.
    pub value_integer: Option<i64>,
    pub start_unix_ms: i64,
    /// Never less than `start_unix_ms`. Equal to it on point events
    /// (`/notification/usage`, 92% of `/app/intents`).
    pub end_unix_ms: i64,
    /// `ZCREATIONDATE` — when knowledged wrote the row, which is at or after
    /// `end_unix_ms`. Becomes `observed_at` on the wire. `None` only if the
    /// column was NULL or unrepresentable; the row itself is still shipped.
    pub created_unix_ms: Option<i64>,
    /// `ZSECONDSFROMGMT` — the local UTC offset when the event happened.
    /// Used for day bucketing in digests only; wire timestamps are UTC.
    pub tz_offset_secs: Option<i64>,
    /// `ZSOURCE.ZBUNDLEID` — the donating app for `/app/intents`; NULL on
    /// the local activity streams.
    pub source_bundle_id: Option<String>,
    /// `ZSOURCE.ZDEVICEID IS NOT NULL` — the row was synced from another
    /// device (iPhone/iPad) rather than observed on this Mac.
    pub synced: bool,
    /// `ZSYNCPEER.ZMODEL` for a synced row, e.g. `iPhone18,2`. Often NULL
    /// even when `synced` — the peer table is not always populated.
    pub origin_device_model: Option<String>,

    // ── ZSTRUCTUREDMETADATA subset (every one `None` when this macOS lacks
    // the column, or the row has no metadata) ────────────────────────────
    /// `Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN` — the host only.
    pub web_domain: Option<String>,
    /// `Z_DKDIGITALHEALTHMETADATAKEY__USAGETYPE`.
    pub usage_type: Option<i64>,
    /// `Z_DKDIGITALHEALTHMETADATAKEY__ISUSAGETRUSTED` (0/1).
    pub usage_trusted: Option<i64>,
    /// `Z_DKNOTIFICATIONUSAGEMETADATAKEY__BUNDLEID` — a real bundle id, or
    /// the pseudo-bundles `_WEB_CENTER_:<host>` / `_SYSTEM_CENTER_:<daemon>`.
    pub notification_bundle_id: Option<String>,
    /// `Z_DKNOWPLAYINGMETADATAKEY__TITLE` — empty on the reference store.
    pub media_title: Option<String>,
    /// `Z_DKNOWPLAYINGMETADATAKEY__ARTIST`.
    pub media_artist: Option<String>,
    /// `Z_DKNOWPLAYINGMETADATAKEY__ALBUM`.
    pub media_album: Option<String>,
    /// `Z_DKNOWPLAYINGMETADATAKEY__MEDIATYPE`.
    pub media_type: Option<String>,
    /// `Z_DKINTENTMETADATAKEY__INTENTCLASS`, e.g. `INStartCallIntent`.
    pub intent_class: Option<String>,
    /// `Z_DKINTENTMETADATAKEY__INTENTVERB`, e.g. `StartCall`.
    pub intent_verb: Option<String>,
    /// `Z_DKINTENTMETADATAKEY__DIRECTION` (0/1/2).
    pub intent_direction: Option<i64>,
    /// `Z_DKINTENTMETADATAKEY__DONATEDBYSIRI` (0/1).
    pub intent_donated_by_siri: Option<i64>,
    /// `Z_DKBLUETOOTHMETADATAKEY__NAME` — the device's advertised name
    /// (`AirPods Pro`, `TOYOTA Prius`). Never the address.
    pub bt_name: Option<String>,
    /// `Z_DKBLUETOOTHMETADATAKEY__DEVICETYPE`.
    pub bt_device_type: Option<i64>,
    /// `Z_DKBLUETOOTHMETADATAKEY__ISAPPLEAUDIODEVICE` (0/1).
    pub bt_is_apple_audio: Option<i64>,
    /// `Z_DKBLUETOOTHMETADATAKEY__ISUSERWEARING` (0/1).
    pub bt_is_user_wearing: Option<i64>,
}

impl KnowledgeRow {
    /// Whole seconds between start and end (floored). Zero for point events.
    pub fn duration_secs(&self) -> u64 {
        u64::try_from((self.end_unix_ms - self.start_unix_ms) / 1000).unwrap_or(0)
    }
}

/// One page from [`read_events`], plus what reading it had to throw away.
#[derive(Debug, Default)]
pub struct EventPage {
    pub rows: Vec<KnowledgeRow>,
    /// How many rows the SQL returned, dropped ones included. The lane stops
    /// paging when this is below its page limit; comparing `rows.len()`
    /// would stop early on a page that lost rows to timestamp validation.
    pub rows_returned: usize,
    /// The highest `Z_PK` the SQL returned, dropped rows included — what the
    /// cursor advances to once everything ingestible on the page is acked.
    /// A page whose every row was dropped would otherwise be re-read
    /// forever; those rows can never become ingestible, so nothing that
    /// could have been retried is skipped.
    pub last_seen_pk: Option<i64>,
    /// Rows whose start or end could not be an instant, or whose end
    /// preceded its start. Counted (and logged with their pk) rather than
    /// quietly omitted.
    pub skipped_invalid_timestamp: usize,
}

/// Open the store read-only. Same shape as `imessage::reader::open_readonly`:
/// no create, no `-wal`/`-shm` sidecar written next to a file this process
/// does not own. Deliberately *not* `immutable=1` — that mode reads only the
/// checkpointed main file and lags the WAL, which on a live store (rows
/// land minutes apart) would make `max_pk` disagree with the cursor. A
/// `CANTOPEN` here is a transient error: the tick stops and retries later.
pub fn open_readonly(path: &std::path::Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", path.display()))
}

/// `Z_METADATA.Z_UUID` — identifies the store *file*. Core Data assigns it
/// when the file is created; a fresh file (reinstall, migration) gets a new
/// one, and with it a fresh `Z_PK` sequence that a stale cursor would skip
/// straight past.
pub fn read_store_uuid(conn: &Connection) -> Result<Option<String>> {
    conn.query_row("SELECT Z_UUID FROM Z_METADATA LIMIT 1", [], |row| {
        row.get::<_, Option<String>>(0)
    })
    .optional()
    .context("reading Z_METADATA.Z_UUID")
    .map(Option::flatten)
}

/// `MAX(Z_PK)` over the whole store, `None` when empty. The lane's
/// defensive check: a cursor above this can only come from a different
/// store file than the one now on disk.
pub fn max_pk(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row("SELECT MAX(Z_PK) FROM ZOBJECT", [], |row| {
        row.get::<_, Option<i64>>(0)
    })
    .context("reading MAX(Z_PK)")
}

/// The `Z_PK` a first crawl should start *after*, so that only rows that
/// started within the last `backfill_days` are shipped. `0` when
/// `backfill_days == 0` (everything the store holds) or the store is empty;
/// `MAX(Z_PK)` when nothing started inside the window (so the crawl begins
/// with the next row written).
///
/// Bounded by `ZSTARTDATE`, not `Z_PK` arithmetic, because retention is
/// per-stream and the pk density per day varies by an order of magnitude
/// across streams.
pub fn first_run_floor(conn: &Connection, backfill_days: u32) -> Result<i64> {
    let now_cocoa_secs =
        (chrono::Utc::now().timestamp_millis() - COCOA_EPOCH_OFFSET_MS) as f64 / 1000.0;
    first_run_floor_at(conn, backfill_days, now_cocoa_secs)
}

/// [`first_run_floor`] with the clock injected, so tests can pin the window
/// to literal fixture dates.
pub fn first_run_floor_at(
    conn: &Connection,
    backfill_days: u32,
    now_cocoa_secs: f64,
) -> Result<i64> {
    if backfill_days == 0 {
        return Ok(0);
    }
    let window_start = now_cocoa_secs - f64::from(backfill_days) * 86_400.0;
    let floor: Option<i64> = conn
        .query_row(
            "SELECT COALESCE(
                 (SELECT MIN(Z_PK) FROM ZOBJECT WHERE ZSTARTDATE >= ?),
                 (SELECT MAX(Z_PK) + 1 FROM ZOBJECT),
                 1
             ) - 1",
            [window_start],
            |row| row.get(0),
        )
        .context("computing first-run floor")?;
    Ok(floor.unwrap_or(0))
}

// ── Digest support ──────────────────────────────────────────────────────────
//
// Everything below reads **local rows only** (`s.ZDEVICEID IS NULL`): a
// digest is "what this Mac did on day D", and a synced iPhone session must
// not inflate it. Days are bucketed in the row's *own* zone —
// `(ZSTARTDATE + ZSECONDSFROMGMT) / 86400` is the number of local days
// since 2001-01-01 — so a laptop that crossed time zones mid-week still
// files each session under the day its owner experienced, and the host
// zone of the process building the digest never enters the SQL.

/// Whole days since 2001-01-01 (the Cocoa epoch date) for a local day.
/// Cocoa seconds offset by `ZSECONDSFROMGMT` divide by 86 400 to exactly
/// this value, which is why the digest's day ordinal is an `i64` rather
/// than a `NaiveDate` until it reaches the wire.
pub fn date_to_day_ordinal(date: chrono::NaiveDate) -> i64 {
    date.signed_duration_since(cocoa_epoch_date()).num_days()
}

/// Inverse of [`date_to_day_ordinal`]; `None` only for an ordinal chrono
/// cannot represent (nothing a real store holds).
pub fn day_ordinal_to_date(day: i64) -> Option<chrono::NaiveDate> {
    cocoa_epoch_date().checked_add_signed(chrono::Duration::days(day))
}

fn cocoa_epoch_date() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2001, 1, 1).expect("2001-01-01 is a date")
}

/// Cocoa seconds of local midnight that *begins* day `day` in a zone
/// `tz_offset_secs` east of UTC: `day * 86400 − tz`. The single formula
/// every digest boundary is derived from (D 00:00, D+1 00:00, D+1 02:00,
/// D+3 00:00), so a sign mistake would show up in every test at once.
pub fn local_midnight_cocoa_secs(day: i64, tz_offset_secs: i64) -> f64 {
    (day * 86_400 - tz_offset_secs) as f64
}

/// `MIN(ZCREATIONDATE)` over the local rows of `stream` — the instant the
/// store's rolling purge has retreated to. A day whose midnight is at or
/// after this instant still has every row it ever had; a day that began
/// before it may have lost its early sessions to the purge and must never
/// be digested (a partial day would read as a quiet one). `None` when the
/// stream has no local rows at all.
pub fn coverage_start(conn: &Connection, stream: &str) -> Result<Option<f64>> {
    conn.query_row(
        "SELECT MIN(o.ZCREATIONDATE)
         FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
         WHERE o.ZSTREAMNAME = ? AND s.ZDEVICEID IS NULL AND o.ZCREATIONDATE > 0",
        [stream],
        |row| row.get::<_, Option<f64>>(0),
    )
    .with_context(|| format!("reading coverage start of {stream}"))
}

/// Whether a local row of `stream` *started* at or after `cocoa_secs`.
/// The digest's flush evidence: `knowledged` writes `/app/usage` rows in
/// session-end order (FIFO), so a session that began on D+1 — and hence
/// ended on D+1 — proves every session that ended on D has already been
/// written, however late the daemon's batch landed (11 h on the reference
/// host).
pub fn has_local_row_on_or_after(conn: &Connection, stream: &str, cocoa_secs: f64) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
             WHERE o.ZSTREAMNAME = ? AND s.ZDEVICEID IS NULL AND o.ZSTARTDATE >= ?
         )",
        rusqlite::params![stream, cocoa_secs],
        |row| row.get::<_, i64>(0),
    )
    .map(|exists| exists != 0)
    .with_context(|| format!("probing for a {stream} row on or after a local day boundary"))
}

/// `MAX(ZENDDATE)` over the local rows of `stream`, `None` when there are
/// none. For `/display/isBacklit` this is how far the display timeline has
/// *closed*: a row is only written when its span ends, so until this
/// reaches D+1 00:00 the "on" span that straddles D's midnight is still
/// open and D's `display_on_secs` would be short by exactly that span.
pub fn max_end(conn: &Connection, stream: &str) -> Result<Option<f64>> {
    conn.query_row(
        "SELECT MAX(o.ZENDDATE)
         FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
         WHERE o.ZSTREAMNAME = ? AND s.ZDEVICEID IS NULL AND o.ZENDDATE > 0",
        [stream],
        |row| row.get::<_, Option<f64>>(0),
    )
    .with_context(|| format!("reading max end of {stream}"))
}

/// `ZSECONDSFROMGMT` of the newest row of `stream`, `None` on an empty
/// stream. The one-shot import uses it to derive "now, in the zone this Mac
/// is currently in" from the store itself rather than from the host clock's
/// zone, keeping every caller below the lane's `chrono::Local` boundary.
pub fn newest_tz_offset(conn: &Connection, stream: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT o.ZSECONDSFROMGMT FROM ZOBJECT o
         WHERE o.ZSTREAMNAME = ? AND o.ZSECONDSFROMGMT IS NOT NULL
         ORDER BY o.Z_PK DESC LIMIT 1",
        [stream],
        |row| row.get::<_, Option<i64>>(0),
    )
    .optional()
    .map(Option::flatten)
    .with_context(|| format!("reading the newest {stream} offset"))
}

/// The distinct local days (ordinals per [`date_to_day_ordinal`]) that
/// have at least one local, non-zero-length row of `stream` that started
/// on, or ran past midnight into, that day — ascending, restricted to days
/// after `after_day`. These are the digest's candidate days: a day with no
/// session at all has nothing to summarize and no zone to bucket it in, so
/// it is never a candidate and the digest cursor steps over it when the
/// next day with rows is emitted.
///
/// The end day of a midnight-crossing session must be nominated too:
/// [`read_daily_usage`] selects by overlap and clips, so that day *has*
/// activity (the session's tail), and a day nominated only by its own
/// starts would leave a tail-only day — a late night followed by a day
/// off — skipped for good once the cursor moves past it. The second branch
/// uses a strict `>` so a session ending exactly at midnight does not
/// nominate the next day, matching `read_daily_usage`'s own bound. Known
/// residual: a single session longer than 24 h would nominate its start
/// and end days but not the whole days between; `/app/usage` sessions of
/// that length do not occur (`knowledged` closes them on switch/sleep).
pub fn local_days_with_rows(
    conn: &Connection,
    stream: &str,
    after_day: Option<i64>,
) -> Result<Vec<i64>> {
    let mut stmt = conn
        .prepare(
            "SELECT local_day FROM (
                 SELECT CAST((o.ZSTARTDATE + o.ZSECONDSFROMGMT) / 86400 AS INTEGER) AS local_day
                 FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
                 WHERE o.ZSTREAMNAME = ? AND s.ZDEVICEID IS NULL
                   AND o.ZSTARTDATE > 0 AND o.ZSECONDSFROMGMT IS NOT NULL
                   AND o.ZENDDATE > o.ZSTARTDATE
                 UNION
                 SELECT CAST((o.ZENDDATE + o.ZSECONDSFROMGMT) / 86400 AS INTEGER)
                 FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
                 WHERE o.ZSTREAMNAME = ? AND s.ZDEVICEID IS NULL
                   AND o.ZSTARTDATE > 0 AND o.ZSECONDSFROMGMT IS NOT NULL
                   AND o.ZENDDATE > (CAST((o.ZSTARTDATE + o.ZSECONDSFROMGMT) / 86400 AS INTEGER) + 1) * 86400
                                    - o.ZSECONDSFROMGMT
             )
             WHERE local_day > ?
             ORDER BY local_day ASC",
        )
        .context("preparing local-day query")?;
    let days = stmt
        .query_map(
            rusqlite::params![stream, stream, after_day.unwrap_or(i64::MIN)],
            |row| row.get::<_, i64>(0),
        )
        .context("running local-day query")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collecting local days")?;
    Ok(days)
}

/// The most common `ZSECONDSFROMGMT` among the local `/app/usage`-style
/// rows of `stream` that overlapped local day `day` — the zone the digest
/// files that day under. Ties break toward the smaller offset so the
/// choice is deterministic. `None` when the day has no rows.
///
/// Overlap, not start day, and with the same per-row bounds as
/// [`read_daily_usage`]: a day whose only activity is the tail of a
/// midnight-crossing session has rows to total but none that started on
/// it, and voting by start day alone would report "no zone" for a day
/// [`local_days_with_rows`] just nominated.
pub fn modal_tz_offset(conn: &Connection, stream: &str, day: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT o.ZSECONDSFROMGMT
         FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
         WHERE o.ZSTREAMNAME = ? AND s.ZDEVICEID IS NULL
           AND o.ZSTARTDATE > 0 AND o.ZSECONDSFROMGMT IS NOT NULL
           AND o.ZENDDATE > o.ZSTARTDATE
           AND o.ZSTARTDATE < (? + 1) * 86400 - o.ZSECONDSFROMGMT
           AND o.ZENDDATE > ? * 86400 - o.ZSECONDSFROMGMT
         GROUP BY o.ZSECONDSFROMGMT
         ORDER BY COUNT(*) DESC, o.ZSECONDSFROMGMT ASC
         LIMIT 1",
        rusqlite::params![stream, day, day],
        |row| row.get::<_, Option<i64>>(0),
    )
    .optional()
    .map(Option::flatten)
    .with_context(|| format!("reading the modal offset of {stream} on day {day}"))
}

/// One app's share of a local day, from [`read_daily_usage`].
#[derive(Debug, Clone, PartialEq)]
pub struct DailyAppUsage {
    /// `ZVALUESTRING` of the `/app/usage` rows.
    pub bundle_id: String,
    /// Sessions that overlapped the day at all.
    pub session_count: u64,
    /// Seconds inside the day, each session clipped to the day's bounds —
    /// a session that ran across midnight contributes only its part on
    /// this side of it, so no day can total more than 86 400 s.
    pub total_secs: i64,
    /// Sessions whose *unclipped* length was below the threshold the
    /// caller passed (the raw lane's `min_session_secs`). They are inside
    /// `total_secs` and `session_count`; this count makes that legible.
    pub short_sessions: u64,
    /// Cocoa seconds of the earliest (clipped) start on the day.
    pub first_start: f64,
    /// Cocoa seconds of the latest (clipped) end on the day.
    pub last_end: f64,
}

/// One local day of `/app/usage`, plus the display-on total for the same
/// day. Apps are ordered by `total_secs` descending (ties by bundle id) so
/// a rank is just a position.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DailyUsage {
    pub apps: Vec<DailyAppUsage>,
    /// Clipped sum of the day's `/display/isBacklit` "on" rows; `None`
    /// when no on-row touched the day. Display rows never overlap, so a
    /// plain sum is exact.
    pub display_on_secs: Option<i64>,
}

/// Per-app totals for local day `day`, sessions **clipped to the day**.
///
/// The day's bounds are computed per row from that row's own
/// `ZSECONDSFROMGMT` (`day*86400 − tz` … `(day+1)*86400 − tz`), so the
/// question asked of every row is "how much of you fell inside the local
/// day *you* were recorded in?" — which is the only bucketing that stays
/// right across a zone change. `short_below_secs` is the raw lane's
/// session floor: sessions under it are still summed (digests count every
/// non-zero session) and reported in `short_sessions`.
///
/// All placeholders are anonymous and bound in textual order, as
/// [`read_events`] does; the `debug_assert` pins the count.
pub fn read_daily_usage(conn: &Connection, day: i64, short_below_secs: i64) -> Result<DailyUsage> {
    const USAGE_SQL: &str = "
        SELECT o.ZVALUESTRING AS bundle_id,
               COUNT(*) AS session_count,
               SUM(MIN(o.ZENDDATE, (? + 1) * 86400 - o.ZSECONDSFROMGMT)
                   - MAX(o.ZSTARTDATE, ? * 86400 - o.ZSECONDSFROMGMT)) AS total_secs,
               SUM(o.ZENDDATE - o.ZSTARTDATE < ?) AS short_sessions,
               MIN(MAX(o.ZSTARTDATE, ? * 86400 - o.ZSECONDSFROMGMT)) AS first_start,
               MAX(MIN(o.ZENDDATE, (? + 1) * 86400 - o.ZSECONDSFROMGMT)) AS last_end
        FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
        WHERE o.ZSTREAMNAME = '/app/usage' AND s.ZDEVICEID IS NULL
          AND o.ZENDDATE > o.ZSTARTDATE AND o.ZSTARTDATE > 0
          AND o.ZSECONDSFROMGMT IS NOT NULL
          AND o.ZVALUESTRING IS NOT NULL AND o.ZVALUESTRING != ''
          AND o.ZSTARTDATE < (? + 1) * 86400 - o.ZSECONDSFROMGMT
          AND o.ZENDDATE > ? * 86400 - o.ZSECONDSFROMGMT
        GROUP BY o.ZVALUESTRING
        ORDER BY total_secs DESC, o.ZVALUESTRING ASC";
    const DISPLAY_SQL: &str = "
        SELECT SUM(MIN(o.ZENDDATE, (? + 1) * 86400 - o.ZSECONDSFROMGMT)
                   - MAX(o.ZSTARTDATE, ? * 86400 - o.ZSECONDSFROMGMT)) AS on_secs
        FROM ZOBJECT o LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
        WHERE o.ZSTREAMNAME = '/display/isBacklit' AND s.ZDEVICEID IS NULL
          AND o.ZVALUEINTEGER = 1
          AND o.ZENDDATE > o.ZSTARTDATE AND o.ZSTARTDATE > 0
          AND o.ZSECONDSFROMGMT IS NOT NULL
          AND o.ZSTARTDATE < (? + 1) * 86400 - o.ZSECONDSFROMGMT
          AND o.ZENDDATE > ? * 86400 - o.ZSECONDSFROMGMT";

    let mut stmt = conn
        .prepare(USAGE_SQL)
        .context("preparing daily usage query")?;
    let values: Vec<Value> = vec![
        Value::Integer(day),
        Value::Integer(day),
        Value::Integer(short_below_secs),
        Value::Integer(day),
        Value::Integer(day),
        Value::Integer(day),
        Value::Integer(day),
    ];
    debug_assert_eq!(
        stmt.parameter_count(),
        values.len(),
        "daily usage SQL placeholder count drifted from its bindings"
    );
    let apps = stmt
        .query_map(params_from_iter(values), |row| {
            Ok(DailyAppUsage {
                bundle_id: get_string(row, "bundle_id")?.unwrap_or_default(),
                session_count: get_i64(row, "session_count")?.unwrap_or(0).max(0) as u64,
                total_secs: get_f64(row, "total_secs")?.unwrap_or(0.0).floor() as i64,
                short_sessions: get_i64(row, "short_sessions")?.unwrap_or(0).max(0) as u64,
                first_start: get_f64(row, "first_start")?.unwrap_or(0.0),
                last_end: get_f64(row, "last_end")?.unwrap_or(0.0),
            })
        })
        .context("running daily usage query")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("reading daily usage rows")?;

    let display_on_secs: Option<f64> = conn
        .query_row(DISPLAY_SQL, rusqlite::params![day, day, day, day], |row| {
            get_f64(row, "on_secs")
        })
        .context("running daily display query")?;

    Ok(DailyUsage {
        apps,
        display_on_secs: display_on_secs.map(|secs| secs.floor() as i64),
    })
}

/// The `ZSTRUCTUREDMETADATA` columns this lane reads, as
/// `(column, alias)`. **This list is the privacy boundary**: a column not
/// named here is never part of any SELECT. Explicitly absent, and to stay
/// that way: `…DIGITALHEALTHMETADATAKEY__WEBPAGEURL` (full URLs, including
/// search queries), `…BLUETOOTHMETADATAKEY__ADDRESS` (MAC),
/// `…NOTIFICATIONUSAGEMETADATAKEY__IDENTIFIER`,
/// `…RELATEDCONTACTIDENTIFIERS`, `…SERIALIZEDINTERACTION`.
const WANTED_METADATA: &[(&str, &str)] = &[
    ("Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN", "web_domain"),
    ("Z_DKDIGITALHEALTHMETADATAKEY__USAGETYPE", "usage_type"),
    (
        "Z_DKDIGITALHEALTHMETADATAKEY__ISUSAGETRUSTED",
        "usage_trusted",
    ),
    (
        "Z_DKNOTIFICATIONUSAGEMETADATAKEY__BUNDLEID",
        "notification_bundle_id",
    ),
    ("Z_DKNOWPLAYINGMETADATAKEY__TITLE", "media_title"),
    ("Z_DKNOWPLAYINGMETADATAKEY__ARTIST", "media_artist"),
    ("Z_DKNOWPLAYINGMETADATAKEY__ALBUM", "media_album"),
    ("Z_DKNOWPLAYINGMETADATAKEY__MEDIATYPE", "media_type"),
    ("Z_DKINTENTMETADATAKEY__INTENTCLASS", "intent_class"),
    ("Z_DKINTENTMETADATAKEY__INTENTVERB", "intent_verb"),
    ("Z_DKINTENTMETADATAKEY__DIRECTION", "intent_direction"),
    (
        "Z_DKINTENTMETADATAKEY__DONATEDBYSIRI",
        "intent_donated_by_siri",
    ),
    ("Z_DKBLUETOOTHMETADATAKEY__NAME", "bt_name"),
    ("Z_DKBLUETOOTHMETADATAKEY__DEVICETYPE", "bt_device_type"),
    (
        "Z_DKBLUETOOTHMETADATAKEY__ISAPPLEAUDIODEVICE",
        "bt_is_apple_audio",
    ),
    (
        "Z_DKBLUETOOTHMETADATAKEY__ISUSERWEARING",
        "bt_is_user_wearing",
    ),
];

/// The names `ZSTRUCTUREDMETADATA` actually has on this host. Empty when
/// the table itself is missing. Re-probed on every [`read_events`] call:
/// one `PRAGMA` per page is noise next to the page query, and it keeps the
/// reader stateless so a drain can open a fresh connection per tick.
fn metadata_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("PRAGMA table_info('ZSTRUCTUREDMETADATA')")
        .context("probing ZSTRUCTUREDMETADATA columns")?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("reading ZSTRUCTUREDMETADATA column names")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collecting ZSTRUCTUREDMETADATA column names")?;
    Ok(names)
}

/// The `SELECT` for one page, with every wanted metadata column that this
/// host has selected under its alias and every missing one substituted by
/// `NULL AS <alias>` — so the row mapper reads the same alias list on every
/// macOS and only the *values* differ. The metadata join is dropped
/// entirely when the table is absent, so the statement still prepares.
///
/// The peer model is a correlated scalar subquery rather than a join:
/// `ZSYNCPEER` is unique only on `ZUUID` (index `Z_SyncPeer_UNIQUE_uuid`),
/// never on `ZDEVICEID`, so a device that re-paired can own two live peer
/// rows and a `LEFT JOIN` on it would emit every synced object twice —
/// doubled counters, a halved effective page, and a poison row parked once
/// per copy. The subquery pins the row count to `ZOBJECT` no matter what
/// the peer table holds, and prefers the most recently seen peer. The two
/// remaining joins are on integer primary keys and cannot fan out.
fn build_events_sql(present: &[String], stream_count: usize) -> String {
    let have_table = !present.is_empty();
    let metadata_select = WANTED_METADATA
        .iter()
        .map(|(col, alias)| {
            if present.iter().any(|p| p == col) {
                format!("m.{col} AS {alias}")
            } else {
                format!("NULL AS {alias}")
            }
        })
        .collect::<Vec<_>>()
        .join(",\n                ");
    let metadata_join = if have_table {
        "LEFT JOIN ZSTRUCTUREDMETADATA m ON m.Z_PK = o.ZSTRUCTUREDMETADATA"
    } else {
        ""
    };
    let stream_placeholders = std::iter::repeat_n("?", stream_count)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "SELECT o.Z_PK AS pk,
                o.ZUUID AS uuid,
                o.ZSTREAMNAME AS stream,
                o.ZVALUESTRING AS value_string,
                o.ZVALUEINTEGER AS value_integer,
                o.ZSTARTDATE AS start_secs,
                o.ZENDDATE AS end_secs,
                o.ZCREATIONDATE AS created_secs,
                o.ZSECONDSFROMGMT AS tz_offset_secs,
                s.ZBUNDLEID AS source_bundle_id,
                (s.ZDEVICEID IS NOT NULL) AS synced,
                (SELECT p.ZMODEL FROM ZSYNCPEER p WHERE p.ZDEVICEID = s.ZDEVICEID
                  ORDER BY p.ZLASTSEENDATE DESC, p.Z_PK DESC LIMIT 1) AS origin_model,
                {metadata_select}
         FROM ZOBJECT o
         LEFT JOIN ZSOURCE s ON s.Z_PK = o.ZSOURCE
         {metadata_join}
         WHERE o.Z_PK > ?
           AND o.ZSTREAMNAME IN ({stream_placeholders})
           AND (? OR s.ZDEVICEID IS NULL)
         ORDER BY o.Z_PK ASC
         LIMIT ?"
    )
}

/// Up to `limit` rows with `Z_PK > after_pk` in the named streams, oldest
/// pk first; local rows only unless `include_synced`. Empty `streams`
/// returns an empty page without touching the database (an `IN ()` is a
/// SQL syntax error, and "nothing enabled" is a legitimate configuration).
///
/// Placeholders are all anonymous and bound **in textual order** via
/// `params_from_iter` — rusqlite forbids mixing `?N` with `?` in one
/// statement, and the stream list is variable-length, so numbering is
/// not an option. The `debug_assert` pins the count so an edit to the SQL
/// that adds a placeholder cannot silently shift every later binding.
pub fn read_events(
    conn: &Connection,
    after_pk: i64,
    streams: &[String],
    include_synced: bool,
    limit: u32,
) -> Result<EventPage> {
    if streams.is_empty() {
        return Ok(EventPage::default());
    }

    let present = metadata_columns(conn)?;
    let sql = build_events_sql(&present, streams.len());
    let mut stmt = conn
        .prepare(&sql)
        .context("preparing knowledge event query")?;

    let mut values: Vec<Value> = Vec::with_capacity(streams.len() + 3);
    values.push(Value::Integer(after_pk));
    values.extend(streams.iter().map(|s| Value::Text(s.clone())));
    values.push(Value::Integer(i64::from(include_synced)));
    values.push(Value::Integer(i64::from(limit)));
    debug_assert_eq!(
        stmt.parameter_count(),
        values.len(),
        "knowledge event SQL placeholder count drifted from its bindings"
    );

    let rows = stmt
        .query_map(params_from_iter(values), |row| {
            let pk: i64 = row.get("pk")?;
            Ok((pk, map_row(row, pk)?))
        })
        .context("running knowledge event query")?;

    let mut page = EventPage::default();
    for row in rows {
        let (pk, row) = row.context("reading knowledge event rows")?;
        page.rows_returned += 1;
        page.last_seen_pk = Some(pk);
        match row {
            Some(row) => page.rows.push(row),
            None => page.skipped_invalid_timestamp += 1,
        }
    }
    Ok(page)
}

/// One SQL row → `KnowledgeRow`, or `None` when its timestamps disqualify
/// it. Every column is read through the lenient accessors below: the
/// schema declares almost nothing `NOT NULL`, and a metadata column's
/// storage class is Apple's to change, so a strict typed `get` would fail
/// the whole page on the one row that differs.
fn map_row(row: &Row<'_>, pk: i64) -> rusqlite::Result<Option<KnowledgeRow>> {
    let start_secs = get_f64(row, "start_secs")?;
    let end_secs = get_f64(row, "end_secs")?;
    let (Some(start_unix_ms), Some(end_unix_ms)) = (
        start_secs.and_then(cocoa_secs_to_unix_ms),
        end_secs.and_then(cocoa_secs_to_unix_ms),
    ) else {
        log::debug!("knowledge: skipping pk={pk}, start/end is not a representable timestamp");
        return Ok(None);
    };
    if end_unix_ms < start_unix_ms {
        log::debug!("knowledge: skipping pk={pk}, ZENDDATE precedes ZSTARTDATE");
        return Ok(None);
    }

    Ok(Some(KnowledgeRow {
        pk,
        uuid: get_string(row, "uuid")?,
        stream: get_string(row, "stream")?.unwrap_or_default(),
        value_string: get_string(row, "value_string")?,
        value_integer: get_i64(row, "value_integer")?,
        start_unix_ms,
        end_unix_ms,
        created_unix_ms: get_f64(row, "created_secs")?.and_then(cocoa_secs_to_unix_ms),
        tz_offset_secs: get_i64(row, "tz_offset_secs")?,
        source_bundle_id: get_string(row, "source_bundle_id")?,
        synced: get_i64(row, "synced")?.unwrap_or(0) != 0,
        origin_device_model: get_string(row, "origin_model")?,
        web_domain: get_string(row, "web_domain")?,
        usage_type: get_i64(row, "usage_type")?,
        usage_trusted: get_i64(row, "usage_trusted")?,
        notification_bundle_id: get_string(row, "notification_bundle_id")?,
        media_title: get_string(row, "media_title")?,
        media_artist: get_string(row, "media_artist")?,
        media_album: get_string(row, "media_album")?,
        media_type: get_string(row, "media_type")?,
        intent_class: get_string(row, "intent_class")?,
        intent_verb: get_string(row, "intent_verb")?,
        intent_direction: get_i64(row, "intent_direction")?,
        intent_donated_by_siri: get_i64(row, "intent_donated_by_siri")?,
        bt_name: get_string(row, "bt_name")?,
        bt_device_type: get_i64(row, "bt_device_type")?,
        bt_is_apple_audio: get_i64(row, "bt_is_apple_audio")?,
        bt_is_user_wearing: get_i64(row, "bt_is_user_wearing")?,
    }))
}

/// Lenient numeric read: INTEGER and REAL both convert; TEXT is parsed;
/// NULL, BLOB, or unparsable text is `None`.
fn get_f64(row: &Row<'_>, name: &str) -> rusqlite::Result<Option<f64>> {
    Ok(match row.get_ref(name)? {
        ValueRef::Integer(i) => Some(i as f64),
        ValueRef::Real(f) => Some(f),
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse().ok()),
        ValueRef::Null | ValueRef::Blob(_) => None,
    })
}

/// Lenient integer read: REAL is truncated, TEXT parsed; NULL/BLOB `None`.
fn get_i64(row: &Row<'_>, name: &str) -> rusqlite::Result<Option<i64>> {
    Ok(match row.get_ref(name)? {
        ValueRef::Integer(i) => Some(i),
        ValueRef::Real(f) if f.is_finite() => Some(f as i64),
        ValueRef::Real(_) => None,
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse().ok()),
        ValueRef::Null | ValueRef::Blob(_) => None,
    })
}

/// Lenient text read: numbers are formatted; NULL/BLOB `None`. Empty
/// strings stay `Some("")` — the fact mapper decides what "no value"
/// means per stream.
fn get_string(row: &Row<'_>, name: &str) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(name)? {
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Null | ValueRef::Blob(_) => None,
    })
}

// ── Test fixture (shared with `e2e`) ─────────────────────────────────────────

/// A `knowledgeC.db`-shaped schema for tests: the tables [`read_events`]
/// touches, with the real column lists for the small ones and only the
/// wanted subset of `ZSTRUCTUREDMETADATA` (the real table has 239). **No
/// `NOT NULL` anywhere** — the real schema has none beyond the implicit
/// primary keys, and a fixture stricter than production would hide the
/// "a NULL in an unexpected column fails the page" class of bug.
///
/// Sample values from the reference store, for realistic fixtures:
///
/// | stream | ZVALUESTRING | ZVALUEINTEGER | ZSTARTDATE | ZENDDATE | ZCREATIONDATE | ZSECONDSFROMGMT |
/// |---|---|---|---|---|---|---|
/// | `/app/usage` | `com.google.Chrome` | hash | `810099494` | `810099500` | `810099501.179779` | `-25200` |
/// | `/app/webUsage` | `com.apple.Safari` | hash | `809048800` | `809048802` | `809048802.705598` | `-25200` |
/// | `/app/mediaUsage` | `com.apple.Photos` | hash | `808852201` | `808852206` | `808852206.916551` | `-25200` |
/// | `/display/isBacklit` | NULL | `1` | `808721504` | `808721564` | `808721566.657898` | `-25200` |
/// | `/bluetooth/isConnected` | NULL | `1` | `808696816` | `808697807` | `808697808.105977` | `-25200` |
/// | `/app/intents` | `Calls` | hash | `808685546` | `808685546` | `808685586.361287` | `-25200` |
/// | `/notification/usage` | `Receive` | hash | `808683682` | `808683682` | `808683682.98102` | `-25200` |
///
/// `ZSTARTDATE`/`ZENDDATE` are integral (stored INTEGER); `ZCREATIONDATE`
/// is fractional (REAL). The `TIMESTAMP` declared type has NUMERIC affinity,
/// so binding an integral `f64` stores INTEGER and a fractional one REAL —
/// the fixture reproduces the real storage-class mix without special cases.
#[cfg(test)]
pub(crate) const FIXTURE_SCHEMA: &str = "
    CREATE TABLE ZOBJECT (
        Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER, ZUUIDHASH INTEGER,
        ZEVENT INTEGER, ZSOURCE INTEGER, ZCATEGORYTYPE INTEGER, ZINTEGERVALUE INTEGER,
        ZCOMPATIBILITYVERSION INTEGER, ZENDDAYOFWEEK INTEGER, ZENDSECONDOFDAY INTEGER,
        ZHASCUSTOMMETADATA INTEGER, ZHASSTRUCTUREDMETADATA INTEGER, ZSECONDSFROMGMT INTEGER,
        ZSHOULDSYNC INTEGER, ZSTARTDAYOFWEEK INTEGER, ZSTARTSECONDOFDAY INTEGER,
        ZVALUECLASS INTEGER, ZVALUEINTEGER INTEGER, ZVALUETYPECODE INTEGER,
        ZSTRUCTUREDMETADATA INTEGER, ZVALUE INTEGER, Z9_VALUE INTEGER, ZIDENTIFIERTYPE INTEGER,
        ZQUANTITYTYPE INTEGER, ZCREATIONDATE TIMESTAMP, ZLOCALCREATIONDATE TIMESTAMP,
        ZCONFIDENCE FLOAT, ZENDDATE TIMESTAMP, ZSTARTDATE TIMESTAMP, ZVALUEDOUBLE FLOAT,
        ZDOUBLEVALUE FLOAT, ZUUID VARCHAR, ZSTREAMNAME VARCHAR, ZVALUESTRING VARCHAR,
        ZSTRING VARCHAR, ZMETADATA BLOB
    );
    CREATE TABLE ZSOURCE (
        Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER, ZUSERID INTEGER,
        ZBUNDLEID VARCHAR, ZDEVICEID VARCHAR, ZGROUPID VARCHAR, ZINTENTID VARCHAR,
        ZITEMID VARCHAR, ZSOURCEID VARCHAR
    );
    CREATE TABLE ZSYNCPEER (
        Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER, ZLASTSEENDATE TIMESTAMP,
        ZCLOUDID VARCHAR, ZDEVICEID VARCHAR, ZMODEL VARCHAR, ZRAPPORTID VARCHAR,
        ZVERSION VARCHAR, ZUUID BLOB
    );
    CREATE TABLE ZSTRUCTUREDMETADATA (
        Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER,
        Z_DKBLUETOOTHMETADATAKEY__DEVICETYPE INTEGER,
        Z_DKBLUETOOTHMETADATAKEY__ISAPPLEAUDIODEVICE INTEGER,
        Z_DKBLUETOOTHMETADATAKEY__ISUSERWEARING INTEGER,
        Z_DKDIGITALHEALTHMETADATAKEY__ISUSAGETRUSTED INTEGER,
        Z_DKDIGITALHEALTHMETADATAKEY__USAGETYPE INTEGER,
        Z_DKINTENTMETADATAKEY__DIRECTION INTEGER,
        Z_DKINTENTMETADATAKEY__DONATEDBYSIRI INTEGER,
        Z_DKBLUETOOTHMETADATAKEY__NAME VARCHAR,
        Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN VARCHAR,
        Z_DKINTENTMETADATAKEY__INTENTCLASS VARCHAR,
        Z_DKINTENTMETADATAKEY__INTENTVERB VARCHAR,
        Z_DKNOTIFICATIONUSAGEMETADATAKEY__BUNDLEID VARCHAR,
        Z_DKNOWPLAYINGMETADATAKEY__ALBUM VARCHAR,
        Z_DKNOWPLAYINGMETADATAKEY__ARTIST VARCHAR,
        Z_DKNOWPLAYINGMETADATAKEY__MEDIATYPE VARCHAR,
        Z_DKNOWPLAYINGMETADATAKEY__TITLE VARCHAR
    );
    CREATE TABLE Z_METADATA (Z_VERSION INTEGER PRIMARY KEY, Z_UUID VARCHAR(255), Z_PLIST BLOB);
";

/// The wanted `ZSTRUCTUREDMETADATA` values for one fixture row. Every field
/// optional so a test names only what it is about.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct FixtureMetadata {
    pub web_domain: Option<String>,
    pub usage_type: Option<i64>,
    pub usage_trusted: Option<i64>,
    pub notification_bundle_id: Option<String>,
    pub media_title: Option<String>,
    pub media_artist: Option<String>,
    pub media_album: Option<String>,
    pub media_type: Option<String>,
    pub intent_class: Option<String>,
    pub intent_verb: Option<String>,
    pub intent_direction: Option<i64>,
    pub intent_donated_by_siri: Option<i64>,
    pub bt_name: Option<String>,
    pub bt_device_type: Option<i64>,
    pub bt_is_apple_audio: Option<i64>,
    pub bt_is_user_wearing: Option<i64>,
}

/// One fixture `ZOBJECT` row for [`insert_row`]. Build with
/// [`FixtureRow::new`] and override what the test is about; the defaults
/// describe a local, unsynced row with a fractional `ZCREATIONDATE` just
/// after its end and the reference host's `-25200` offset.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct FixtureRow {
    /// `None` lets SQLite assign the next `Z_PK`.
    pub pk: Option<i64>,
    /// `None` stores NULL (a real possibility — only `Z_PK` is `NOT NULL`).
    pub uuid: Option<String>,
    pub stream: String,
    pub value_string: Option<String>,
    pub value_integer: Option<i64>,
    /// Cocoa seconds. Integral values are stored INTEGER, fractional REAL.
    pub start: f64,
    pub end: f64,
    /// `None` stores NULL.
    pub created: Option<f64>,
    pub tz_offset_secs: Option<i64>,
    /// `ZSOURCE.ZBUNDLEID` — a `ZSOURCE` row is created when this or
    /// `synced_device` is set.
    pub source_bundle_id: Option<String>,
    /// `(ZDEVICEID, ZSYNCPEER.ZMODEL)` for a row synced from another device.
    pub synced_device: Option<(String, Option<String>)>,
    /// When set, a `ZSTRUCTUREDMETADATA` row is created and linked.
    pub metadata: Option<FixtureMetadata>,
}

#[cfg(test)]
impl FixtureRow {
    pub(crate) fn new(stream: &str, start: f64, end: f64) -> Self {
        Self {
            pk: None,
            uuid: None,
            stream: stream.to_owned(),
            value_string: None,
            value_integer: None,
            start,
            end,
            created: Some(end + 1.5),
            tz_offset_secs: Some(-25_200),
            source_bundle_id: None,
            synced_device: None,
            metadata: None,
        }
    }

    pub(crate) fn pk(mut self, pk: i64) -> Self {
        self.pk = Some(pk);
        self
    }

    pub(crate) fn uuid(mut self, uuid: &str) -> Self {
        self.uuid = Some(uuid.to_owned());
        self
    }

    pub(crate) fn value_string(mut self, value: &str) -> Self {
        self.value_string = Some(value.to_owned());
        self
    }

    pub(crate) fn value_integer(mut self, value: i64) -> Self {
        self.value_integer = Some(value);
        self
    }

    pub(crate) fn created(mut self, created: Option<f64>) -> Self {
        self.created = created;
        self
    }

    pub(crate) fn tz_offset_secs(mut self, tz: Option<i64>) -> Self {
        self.tz_offset_secs = tz;
        self
    }

    pub(crate) fn source_bundle_id(mut self, bundle_id: &str) -> Self {
        self.source_bundle_id = Some(bundle_id.to_owned());
        self
    }

    pub(crate) fn synced_from(mut self, device_id: &str, model: Option<&str>) -> Self {
        self.synced_device = Some((device_id.to_owned(), model.map(str::to_owned)));
        self
    }

    pub(crate) fn metadata(mut self, metadata: FixtureMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Insert a `ZSYNCPEER` row unconditionally. The real table has no
/// uniqueness on `ZDEVICEID` (only `Z_SyncPeer_UNIQUE_uuid`), so a device
/// that re-paired can own two live rows; use this to build a store with two
/// peers for one device id and prove the reader never fans out on them.
#[cfg(test)]
pub(crate) fn insert_sync_peer(conn: &Connection, device_id: &str, model: Option<&str>) {
    conn.execute(
        "INSERT INTO ZSYNCPEER (ZDEVICEID, ZMODEL) VALUES (?1, ?2)",
        rusqlite::params![device_id, model],
    )
    .unwrap();
}

/// Insert one row (plus its `ZSOURCE` / `ZSYNCPEER` / `ZSTRUCTUREDMETADATA`
/// companions as needed) and return its `Z_PK`. A `ZSYNCPEER` row is
/// created once per device id, mirroring the store's usual shape; see
/// [`insert_sync_peer`] for the duplicate case.
#[cfg(test)]
pub(crate) fn insert_row(conn: &Connection, row: &FixtureRow) -> i64 {
    use rusqlite::params;

    let source_pk: Option<i64> = if row.source_bundle_id.is_some() || row.synced_device.is_some() {
        let device_id = row.synced_device.as_ref().map(|(id, _)| id.clone());
        conn.execute(
            "INSERT INTO ZSOURCE (ZBUNDLEID, ZDEVICEID) VALUES (?1, ?2)",
            params![row.source_bundle_id, device_id],
        )
        .unwrap();
        Some(conn.last_insert_rowid())
    } else {
        None
    };

    if let Some((device_id, model)) = &row.synced_device {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ZSYNCPEER WHERE ZDEVICEID = ?1",
                params![device_id],
                |r| r.get(0),
            )
            .unwrap();
        if exists == 0 {
            insert_sync_peer(conn, device_id, model.as_deref());
        }
    }

    let metadata_pk: Option<i64> = row.metadata.as_ref().map(|m| {
        conn.execute(
            "INSERT INTO ZSTRUCTUREDMETADATA (
                Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN,
                Z_DKDIGITALHEALTHMETADATAKEY__USAGETYPE,
                Z_DKDIGITALHEALTHMETADATAKEY__ISUSAGETRUSTED,
                Z_DKNOTIFICATIONUSAGEMETADATAKEY__BUNDLEID,
                Z_DKNOWPLAYINGMETADATAKEY__TITLE,
                Z_DKNOWPLAYINGMETADATAKEY__ARTIST,
                Z_DKNOWPLAYINGMETADATAKEY__ALBUM,
                Z_DKNOWPLAYINGMETADATAKEY__MEDIATYPE,
                Z_DKINTENTMETADATAKEY__INTENTCLASS,
                Z_DKINTENTMETADATAKEY__INTENTVERB,
                Z_DKINTENTMETADATAKEY__DIRECTION,
                Z_DKINTENTMETADATAKEY__DONATEDBYSIRI,
                Z_DKBLUETOOTHMETADATAKEY__NAME,
                Z_DKBLUETOOTHMETADATAKEY__DEVICETYPE,
                Z_DKBLUETOOTHMETADATAKEY__ISAPPLEAUDIODEVICE,
                Z_DKBLUETOOTHMETADATAKEY__ISUSERWEARING
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                m.web_domain,
                m.usage_type,
                m.usage_trusted,
                m.notification_bundle_id,
                m.media_title,
                m.media_artist,
                m.media_album,
                m.media_type,
                m.intent_class,
                m.intent_verb,
                m.intent_direction,
                m.intent_donated_by_siri,
                m.bt_name,
                m.bt_device_type,
                m.bt_is_apple_audio,
                m.bt_is_user_wearing,
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    });

    conn.execute(
        "INSERT INTO ZOBJECT (
            Z_PK, ZUUID, ZSTREAMNAME, ZVALUESTRING, ZVALUEINTEGER,
            ZSTARTDATE, ZENDDATE, ZCREATIONDATE, ZSECONDSFROMGMT,
            ZSOURCE, ZSTRUCTUREDMETADATA
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            row.pk,
            row.uuid,
            row.stream,
            row.value_string,
            row.value_integer,
            row.start,
            row.end,
            row.created,
            row.tz_offset_secs,
            source_pk,
            metadata_pk,
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// Write the store's identity, as Core Data does on file creation.
#[cfg(test)]
pub(crate) fn insert_store_uuid(conn: &Connection, uuid: &str) {
    conn.execute(
        "INSERT INTO Z_METADATA (Z_VERSION, Z_UUID) VALUES (1, ?1)",
        [uuid],
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn
    }

    fn streams(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pks(page: &EventPage) -> Vec<i64> {
        page.rows.iter().map(|r| r.pk).collect()
    }

    /// 2026-09-15T00:00:00Z in Cocoa seconds, computed by hand (not through
    /// this module) so the conversion tests cannot pass by construction.
    const SEP_15_2026_COCOA: f64 = 811_123_200.0;
    const SEP_15_2026_UNIX_MS: i64 = 1_789_430_400_000;
    /// The year-0000 value the sync-bookkeeping streams really carry.
    const YEAR_0000_SENTINEL: f64 = -63_114_076_800.0;

    #[test]
    fn cocoa_seconds_convert_to_the_known_unix_millisecond() {
        assert_eq!(
            cocoa_secs_to_unix_ms(SEP_15_2026_COCOA),
            Some(SEP_15_2026_UNIX_MS)
        );
        // A REAL creation date keeps its milliseconds and drops the rest.
        assert_eq!(
            cocoa_secs_to_unix_ms(SEP_15_2026_COCOA + 1.179_779),
            Some(SEP_15_2026_UNIX_MS + 1_179)
        );
    }

    /// The sync-bookkeeping streams store `-63114076800` (0001-01-01 in
    /// Cocoa terms) as a "never" marker. Converted naively it is a valid
    /// chrono instant in the year 0, which the server would have accepted
    /// and indexed as a two-thousand-year-old fact.
    #[test]
    fn the_year_0000_sentinel_and_other_impossible_values_convert_to_none() {
        assert_eq!(cocoa_secs_to_unix_ms(YEAR_0000_SENTINEL), None);
        assert_eq!(cocoa_secs_to_unix_ms(-1.0), None);
        assert_eq!(cocoa_secs_to_unix_ms(f64::NAN), None);
        assert_eq!(cocoa_secs_to_unix_ms(f64::INFINITY), None);
        assert_eq!(cocoa_secs_to_unix_ms(5.0e9), None);
        assert_eq!(cocoa_secs_to_unix_ms(0.0), Some(COCOA_EPOCH_OFFSET_MS));
    }

    /// The real store mixes storage classes: `ZSTARTDATE`/`ZENDDATE` are
    /// INTEGER, `ZCREATIONDATE` is REAL, and one sync stream even mixes
    /// INTEGER starts with REAL ends. A typed `get::<i64>` on the REAL
    /// column (or vice versa) fails the whole page; the reader must take
    /// both, and must drop — not crash on, not re-date — the sentinel row
    /// and an `end < start` row while still counting them and advancing
    /// `last_seen_pk` past them.
    #[test]
    fn integer_and_real_timestamps_both_read_while_sentinels_and_inverted_spans_are_dropped_and_counted()
     {
        let conn = fixture_db();
        // pk 1: integral start/end (INTEGER), fractional created (REAL).
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", SEP_15_2026_COCOA, SEP_15_2026_COCOA + 60.0)
                .pk(1)
                .created(Some(SEP_15_2026_COCOA + 61.179_779)),
        );
        // pk 2: fractional start/end (REAL).
        insert_row(
            &conn,
            &FixtureRow::new(
                "/app/usage",
                SEP_15_2026_COCOA + 100.5,
                SEP_15_2026_COCOA + 130.25,
            )
            .pk(2),
        );
        // pk 3: the sentinel start.
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", YEAR_0000_SENTINEL, SEP_15_2026_COCOA + 200.0).pk(3),
        );
        // pk 4: end before start.
        insert_row(
            &conn,
            &FixtureRow::new(
                "/app/usage",
                SEP_15_2026_COCOA + 300.0,
                SEP_15_2026_COCOA + 299.0,
            )
            .pk(4),
        );
        // pk 5: NULL created is fine; NULL end is not.
        insert_row(
            &conn,
            &FixtureRow::new(
                "/app/usage",
                SEP_15_2026_COCOA + 400.0,
                SEP_15_2026_COCOA + 460.0,
            )
            .pk(5)
            .created(None),
        );
        conn.execute("UPDATE ZOBJECT SET ZENDDATE = NULL WHERE Z_PK = 4", [])
            .unwrap();

        // Prove the fixture reproduces the storage-class mix before relying on it.
        let classes: Vec<(String, String, String)> = {
            let mut stmt = conn
                .prepare("SELECT typeof(ZSTARTDATE), typeof(ZENDDATE), typeof(ZCREATIONDATE) FROM ZOBJECT WHERE Z_PK IN (1, 2) ORDER BY Z_PK")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            classes,
            vec![
                ("integer".into(), "integer".into(), "real".into()),
                ("real".into(), "real".into(), "real".into()),
            ]
        );

        let page = read_events(&conn, 0, &streams(&["/app/usage"]), false, 100).unwrap();
        assert_eq!(pks(&page), [1, 2, 5]);
        assert_eq!(page.rows_returned, 5);
        assert_eq!(page.skipped_invalid_timestamp, 2);
        assert_eq!(
            page.last_seen_pk,
            Some(5),
            "dropped rows must still move the cursor"
        );

        let first = &page.rows[0];
        assert_eq!(first.start_unix_ms, SEP_15_2026_UNIX_MS);
        assert_eq!(first.end_unix_ms, SEP_15_2026_UNIX_MS + 60_000);
        assert_eq!(first.created_unix_ms, Some(SEP_15_2026_UNIX_MS + 61_179));
        assert_eq!(first.duration_secs(), 60);
        let second = &page.rows[1];
        assert_eq!(second.start_unix_ms, SEP_15_2026_UNIX_MS + 100_500);
        assert_eq!(second.end_unix_ms, SEP_15_2026_UNIX_MS + 130_250);
        assert_eq!(second.duration_secs(), 29, "floored, not rounded");
        assert_eq!(page.rows[2].created_unix_ms, None);
    }

    /// `ZSTRUCTUREDMETADATA` has 239 columns on macOS 27 and a different set
    /// on every release. A fixture whose metadata table lacks one of the
    /// wanted columns must still produce a page — with that field `None` —
    /// rather than a "no such column" error that takes the whole lane down
    /// on the next macOS.
    #[test]
    fn a_metadata_table_missing_a_wanted_column_reads_as_none_instead_of_failing() {
        let conn = fixture_db();
        insert_row(
            &conn,
            &FixtureRow::new(
                "/bluetooth/isConnected",
                SEP_15_2026_COCOA,
                SEP_15_2026_COCOA + 900.0,
            )
            .value_integer(1)
            .metadata(FixtureMetadata {
                bt_name: Some("AirPods Pro".into()),
                bt_device_type: Some(20),
                bt_is_apple_audio: Some(1),
                bt_is_user_wearing: Some(1),
                media_type: Some("audio".into()),
                ..Default::default()
            }),
        );
        // Now make this "macOS" one that never had two of the wanted columns.
        conn.execute_batch(
            "ALTER TABLE ZSTRUCTUREDMETADATA DROP COLUMN Z_DKBLUETOOTHMETADATAKEY__ISUSERWEARING;
             ALTER TABLE ZSTRUCTUREDMETADATA DROP COLUMN Z_DKNOWPLAYINGMETADATAKEY__MEDIATYPE;",
        )
        .unwrap();

        let page = read_events(&conn, 0, &streams(&["/bluetooth/isConnected"]), false, 10).unwrap();
        assert_eq!(page.rows.len(), 1);
        let row = &page.rows[0];
        assert_eq!(row.bt_name.as_deref(), Some("AirPods Pro"));
        assert_eq!(row.bt_device_type, Some(20));
        assert_eq!(row.bt_is_apple_audio, Some(1));
        assert_eq!(row.bt_is_user_wearing, None, "absent column reads as None");
        assert_eq!(row.media_type, None, "absent column reads as None");
    }

    /// The whole metadata table can be absent (a hypothetical stripped
    /// store); the join is dropped and every metadata field is `None`.
    #[test]
    fn a_store_without_the_metadata_table_still_reads_object_rows() {
        let conn = fixture_db();
        conn.execute_batch("DROP TABLE ZSTRUCTUREDMETADATA;")
            .unwrap();
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", SEP_15_2026_COCOA, SEP_15_2026_COCOA + 60.0)
                .value_string("com.google.Chrome"),
        );
        let page = read_events(&conn, 0, &streams(&["/app/usage"]), false, 10).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(
            page.rows[0].value_string.as_deref(),
            Some("com.google.Chrome")
        );
        assert_eq!(page.rows[0].web_domain, None);
    }

    fn seed_one_of_each(conn: &Connection) {
        let t = SEP_15_2026_COCOA;
        insert_row(
            conn,
            &FixtureRow::new("/app/usage", t, t + 60.0)
                .pk(1)
                .value_string("com.google.Chrome"),
        );
        insert_row(
            conn,
            &FixtureRow::new("/app/mediaUsage", t + 100.0, t + 160.0)
                .pk(2)
                .value_string("com.apple.Photos"),
        );
        insert_row(
            conn,
            &FixtureRow::new("/notification/usage", t + 200.0, t + 200.0)
                .pk(3)
                .value_string("Receive"),
        );
        insert_row(
            conn,
            &FixtureRow::new("/bluetooth/isConnected", t + 300.0, t + 900.0)
                .pk(4)
                .value_integer(1),
        );
        insert_row(
            conn,
            &FixtureRow::new("/display/isBacklit", t + 1000.0, t + 1600.0)
                .pk(5)
                .value_integer(1),
        );
        insert_row(
            conn,
            &FixtureRow::new("/app/webUsage", t + 2000.0, t + 2060.0)
                .pk(6)
                .value_string("com.apple.Safari"),
        );
        insert_row(
            conn,
            &FixtureRow::new("/app/intents", t + 3000.0, t + 3000.0)
                .pk(7)
                .value_string("Calls")
                .source_bundle_id("com.apple.InCallService")
                .synced_from("DEVICE-A", Some("iPhone18,2")),
        );
        // A sync-bookkeeping row that no stream list should ever match.
        insert_row(
            conn,
            &FixtureRow::new("/knowledge-sync-addition-window/X", t, t).pk(8),
        );
    }

    #[test]
    fn read_events_with_one_stream_returns_only_that_stream() {
        let conn = fixture_db();
        seed_one_of_each(&conn);
        let page =
            read_events(&conn, 0, &streams(&["/bluetooth/isConnected"]), false, 100).unwrap();
        assert_eq!(pks(&page), [4]);
        assert_eq!(page.rows[0].stream, "/bluetooth/isConnected");
        assert_eq!(page.rows[0].value_integer, Some(1));
        assert!(!page.rows[0].synced);
        assert_eq!(page.rows_returned, 1);
        assert_eq!(page.last_seen_pk, Some(4));
    }

    #[test]
    fn read_events_with_the_default_streams_returns_exactly_the_default_set() {
        let conn = fixture_db();
        seed_one_of_each(&conn);
        let defaults = streams(super::super::facts::DEFAULT_STREAMS);
        let page = read_events(&conn, 0, &defaults, false, 100).unwrap();
        assert_eq!(pks(&page), [1, 2, 3, 4]);
        assert_eq!(
            page.rows[0].value_string.as_deref(),
            Some("com.google.Chrome")
        );
        assert_eq!(page.rows[2].value_string.as_deref(), Some("Receive"));
    }

    /// `/app/intents` is 100% synced from iPhone/iPad on the reference host.
    /// With `include_synced == false` the stream can be enabled and still
    /// yield nothing; flipping the flag makes the same rows appear, tagged
    /// with the peer's model and — critically — without the raw device id.
    #[test]
    fn include_synced_false_hides_synced_rows_and_true_reveals_them_with_the_peer_model() {
        let conn = fixture_db();
        seed_one_of_each(&conn);
        let intents = streams(&["/app/intents"]);

        let hidden = read_events(&conn, 0, &intents, false, 100).unwrap();
        assert!(hidden.rows.is_empty());
        assert_eq!(hidden.rows_returned, 0);
        assert_eq!(hidden.last_seen_pk, None);

        let shown = read_events(&conn, 0, &intents, true, 100).unwrap();
        assert_eq!(pks(&shown), [7]);
        let row = &shown.rows[0];
        assert!(row.synced);
        assert_eq!(row.origin_device_model.as_deref(), Some("iPhone18,2"));
        assert_eq!(
            row.source_bundle_id.as_deref(),
            Some("com.apple.InCallService")
        );
        assert_eq!(row.value_string.as_deref(), Some("Calls"));

        // Local rows are unaffected by the flag.
        let both = read_events(
            &conn,
            0,
            &streams(&["/app/usage", "/app/intents"]),
            true,
            100,
        )
        .unwrap();
        assert_eq!(pks(&both), [1, 7]);
    }

    /// `ZSYNCPEER` has no uniqueness on `ZDEVICEID` (only on `ZUUID`), so a
    /// device that re-paired owns two live peer rows. That must not double
    /// every synced row in the page — the cursor, the counters and the
    /// parking ledger all assume one row per `Z_PK` — and the model must
    /// come from the peer seen most recently, not whichever row the planner
    /// happened to visit first.
    #[test]
    fn a_duplicate_sync_peer_does_not_fan_out_synced_rows() {
        let conn = fixture_db();
        seed_one_of_each(&conn); // pk 7 synced from DEVICE-A / iPhone18,2
        insert_sync_peer(&conn, "DEVICE-A", Some("iPhone17,1"));
        conn.execute(
            "UPDATE ZSYNCPEER SET ZLASTSEENDATE = 1.0 WHERE ZMODEL = 'iPhone18,2'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE ZSYNCPEER SET ZLASTSEENDATE = 2.0 WHERE ZMODEL = 'iPhone17,1'",
            [],
        )
        .unwrap();

        let page = read_events(&conn, 0, &streams(&["/app/intents"]), true, 100).unwrap();
        assert_eq!(pks(&page), [7]);
        assert_eq!(page.rows_returned, 1);
        assert_eq!(page.last_seen_pk, Some(7));
        assert_eq!(
            page.rows[0].origin_device_model.as_deref(),
            Some("iPhone17,1"),
            "the most recently seen peer names the device"
        );
    }

    /// Paging: a `limit` below the match count returns the oldest pks and
    /// reports the last one as `last_seen_pk`; feeding that back yields the
    /// remainder, with no overlap and nothing skipped.
    #[test]
    fn a_limit_below_the_match_count_pages_by_last_seen_pk_without_overlap() {
        let conn = fixture_db();
        seed_one_of_each(&conn);
        let defaults = streams(super::super::facts::DEFAULT_STREAMS);

        let first = read_events(&conn, 0, &defaults, false, 3).unwrap();
        assert_eq!(pks(&first), [1, 2, 3]);
        assert_eq!(first.rows_returned, 3);
        assert_eq!(first.last_seen_pk, Some(3));

        let second = read_events(&conn, first.last_seen_pk.unwrap(), &defaults, false, 3).unwrap();
        assert_eq!(pks(&second), [4]);
        assert_eq!(
            second.rows_returned, 1,
            "below the limit: the lane stops paging here"
        );
        assert_eq!(second.last_seen_pk, Some(4));

        let third = read_events(&conn, 4, &defaults, false, 3).unwrap();
        assert!(third.rows.is_empty());
        assert_eq!(third.last_seen_pk, None);
    }

    /// An empty stream list is a legitimate configuration (everything
    /// disabled), not a SQL error — and it must not touch the database at
    /// all, so it works on a store the process cannot even query.
    #[test]
    fn an_empty_stream_list_returns_an_empty_page_without_querying() {
        let conn = Connection::open_in_memory().unwrap(); // no schema at all
        let page = read_events(&conn, 0, &[], true, 100).unwrap();
        assert!(page.rows.is_empty());
        assert_eq!(page.rows_returned, 0);
        assert_eq!(page.last_seen_pk, None);
        assert_eq!(page.skipped_invalid_timestamp, 0);
    }

    /// `ZSECONDSFROMGMT` is the only column the digests bucket days by, and
    /// only `Z_PK` is `NOT NULL` in the real schema — so a NULL offset must
    /// read as `None` (never default to UTC, which would silently shift a
    /// row into the wrong local day) and a non-reference offset must come
    /// back untouched rather than being normalised to the host's zone.
    #[test]
    fn the_utc_offset_reads_back_verbatim_and_a_null_offset_is_none() {
        let conn = fixture_db();
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", SEP_15_2026_COCOA, SEP_15_2026_COCOA + 60.0)
                .pk(1)
                .tz_offset_secs(Some(3_600)),
        );
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", SEP_15_2026_COCOA, SEP_15_2026_COCOA + 60.0)
                .pk(2)
                .tz_offset_secs(None),
        );
        let page = read_events(&conn, 0, &streams(&["/app/usage"]), false, 10).unwrap();
        assert_eq!(pks(&page), [1, 2]);
        assert_eq!(page.rows[0].tz_offset_secs, Some(3_600));
        assert_eq!(page.rows[1].tz_offset_secs, None);
    }

    #[test]
    fn store_uuid_and_max_pk_read_back_what_the_fixture_wrote() {
        let conn = fixture_db();
        assert_eq!(read_store_uuid(&conn).unwrap(), None);
        assert_eq!(max_pk(&conn).unwrap(), None);
        insert_store_uuid(&conn, "D0F8C2A8-4F00-4514-AA07-746DAB1E27F2");
        seed_one_of_each(&conn);
        assert_eq!(
            read_store_uuid(&conn).unwrap().as_deref(),
            Some("D0F8C2A8-4F00-4514-AA07-746DAB1E27F2")
        );
        assert_eq!(max_pk(&conn).unwrap(), Some(8));
    }

    #[test]
    fn first_run_floor_on_an_empty_store_is_zero() {
        let conn = fixture_db();
        assert_eq!(first_run_floor_at(&conn, 30, SEP_15_2026_COCOA).unwrap(), 0);
        assert_eq!(first_run_floor(&conn, 30).unwrap(), 0);
    }

    /// `backfill_days = 0` means "everything the store holds": the floor is
    /// 0 regardless of how old the rows are, and the database is not even
    /// consulted.
    #[test]
    fn first_run_floor_with_zero_backfill_days_is_zero_regardless_of_rows() {
        let conn = fixture_db();
        insert_row(
            &conn,
            &FixtureRow::new(
                "/app/usage",
                SEP_15_2026_COCOA - 100.0 * 86_400.0,
                SEP_15_2026_COCOA - 100.0 * 86_400.0 + 60.0,
            )
            .pk(10),
        );
        assert_eq!(first_run_floor_at(&conn, 0, SEP_15_2026_COCOA).unwrap(), 0);
    }

    /// The window is bounded by `ZSTARTDATE`, so rows older than
    /// `backfill_days` are skipped by pk (floor = pk before the first in-window
    /// row), and when *nothing* started inside the window the floor is
    /// `MAX(Z_PK)` — the crawl starts with the next row written, not from 0.
    #[test]
    fn first_run_floor_lands_just_before_the_first_row_inside_the_window() {
        let conn = fixture_db();
        let day = 86_400.0;
        let now = SEP_15_2026_COCOA;
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", now - 40.0 * day, now - 40.0 * day + 60.0).pk(10),
        );
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", now - 31.0 * day, now - 31.0 * day + 60.0).pk(11),
        );
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", now - 29.0 * day, now - 29.0 * day + 60.0).pk(12),
        );
        insert_row(
            &conn,
            &FixtureRow::new("/app/usage", now - 1.0 * day, now - 1.0 * day + 60.0).pk(13),
        );

        assert_eq!(first_run_floor_at(&conn, 30, now).unwrap(), 11);
        assert_eq!(first_run_floor_at(&conn, 365, now).unwrap(), 9);
        // Nothing in the last 12 hours → resume after the newest row.
        assert_eq!(
            first_run_floor_at(&conn, 1, now + 12.0 * 3600.0).unwrap(),
            13
        );
    }

    /// The row struct has no field for anything the wanted-column list
    /// excludes. This is a compile-time property, but the SELECT is built
    /// from a string, so pin the text itself: none of the excluded column
    /// names may appear in the generated SQL even when the host has them.
    #[test]
    fn the_generated_select_never_names_an_excluded_column() {
        let present: Vec<String> = WANTED_METADATA
            .iter()
            .map(|(c, _)| (*c).to_owned())
            .chain(
                [
                    "Z_DKDIGITALHEALTHMETADATAKEY__WEBPAGEURL",
                    "Z_DKBLUETOOTHMETADATAKEY__ADDRESS",
                    "Z_DKNOTIFICATIONUSAGEMETADATAKEY__IDENTIFIER",
                    "Z_DKINTENTMETADATAKEY__SERIALIZEDINTERACTION",
                    "Z_DKINTENTMETADATAKEY__RELATEDCONTACTIDENTIFIERS",
                ]
                .into_iter()
                .map(str::to_owned),
            )
            .collect();
        let sql = build_events_sql(&present, 2);
        for forbidden in [
            "WEBPAGEURL",
            "__ADDRESS",
            "__IDENTIFIER",
            "SERIALIZEDINTERACTION",
            "RELATEDCONTACT",
            "ZDEVICEID AS",
        ] {
            assert!(!sql.contains(forbidden), "SQL selects {forbidden}:\n{sql}");
        }
        assert!(sql.contains("m.Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN AS web_domain"));
        assert!(sql.contains("IN (?,?)"));
    }

    // ── digest support ───────────────────────────────────────────────────

    /// 2026-09-14 as days since 2001-01-01, computed by hand (25 years =
    /// 9 131 days incl. six leap days, + 256 days into 2026).
    const DAY_2026_09_14: i64 = 9_387;
    /// The reference host's zone: UTC−7 (PDT).
    const TZ_PDT: i64 = -25_200;

    /// Cocoa seconds `hours` after local midnight of `day` in PDT.
    fn at(day: i64, hours: f64) -> f64 {
        local_midnight_cocoa_secs(day, TZ_PDT) + hours * 3_600.0
    }

    fn usage_row(pk: i64, bundle: &str, start: f64, end: f64) -> FixtureRow {
        FixtureRow::new("/app/usage", start, end)
            .pk(pk)
            .uuid(&format!("U{pk}"))
            .value_string(bundle)
    }

    fn display_on(pk: i64, start: f64, end: f64) -> FixtureRow {
        FixtureRow::new("/display/isBacklit", start, end)
            .pk(pk)
            .value_integer(1)
    }

    /// The day ordinal is the digest's unit; an off-by-one here would file
    /// every digest under the wrong date and the wrong midnight, so both
    /// directions and the midnight instant are pinned to literal values.
    #[test]
    fn day_ordinals_round_trip_and_local_midnight_lands_on_the_known_utc_instant() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 9, 14).unwrap();
        assert_eq!(date_to_day_ordinal(date), DAY_2026_09_14);
        assert_eq!(day_ordinal_to_date(DAY_2026_09_14), Some(date));
        assert_eq!(date_to_day_ordinal(cocoa_epoch_date()), 0);

        // Midnight 2026-09-14 in PDT is 07:00Z.
        let midnight = local_midnight_cocoa_secs(DAY_2026_09_14, TZ_PDT);
        let unix_ms = cocoa_secs_to_unix_ms(midnight).unwrap();
        assert_eq!(
            crate::server_ingest::wire_ts(unix_ms).as_deref(),
            Some("2026-09-14T07:00:00.000Z")
        );
        // A zone east of UTC moves the instant earlier.
        assert_eq!(
            local_midnight_cocoa_secs(DAY_2026_09_14, 3_600),
            midnight - 28_800.0
        );
    }

    /// The display is "on" from 23:52 the night before to 00:05 the night
    /// after: a 24 h 13 min row that overlaps the whole day. A plain SUM
    /// would report a 24.13 h day; the clipped SUM must report exactly
    /// 86 400 s. The same clipping applies to an app session that crosses
    /// midnight — only its part inside the day counts, on each side.
    #[test]
    fn totals_are_clipped_to_the_day_so_a_midnight_spanning_row_cannot_exceed_24_hours() {
        let conn = fixture_db();
        let d = DAY_2026_09_14;
        insert_row(
            &conn,
            &display_on(1, at(d, -8.0 / 60.0), at(d + 1, 5.0 / 60.0)),
        );
        // Chrome 23:50 → 00:20 next day: 10 min on D, 20 min on D+1.
        insert_row(
            &conn,
            &usage_row(
                2,
                "com.google.Chrome",
                at(d, 23.0 + 50.0 / 60.0),
                at(d + 1, 20.0 / 60.0),
            ),
        );
        // Discord entirely inside D.
        insert_row(
            &conn,
            &usage_row(3, "com.hnc.Discord", at(d, 9.0), at(d, 10.0)),
        );
        // An on-row entirely inside D+1 must not leak into D.
        insert_row(&conn, &display_on(4, at(d + 1, 9.0), at(d + 1, 10.0)));

        let day = read_daily_usage(&conn, d, 30).unwrap();
        assert_eq!(day.display_on_secs, Some(86_400), "24.0 h, not 24.13 h");
        let chrome = day
            .apps
            .iter()
            .find(|a| a.bundle_id == "com.google.Chrome")
            .unwrap();
        assert_eq!(chrome.total_secs, 600);
        assert_eq!(chrome.session_count, 1);
        assert_eq!(chrome.first_start, at(d, 23.0 + 50.0 / 60.0));
        assert_eq!(chrome.last_end, at(d + 1, 0.0), "clipped to midnight");
        // Ordered by total: Discord (3600) before Chrome (600).
        assert_eq!(day.apps[0].bundle_id, "com.hnc.Discord");
        assert_eq!(day.apps[0].first_start, at(d, 9.0));
        assert_eq!(day.apps[0].last_end, at(d, 10.0));

        let next = read_daily_usage(&conn, d + 1, 30).unwrap();
        assert_eq!(next.display_on_secs, Some(300 + 3_600));
        assert_eq!(next.apps.len(), 1);
        assert_eq!(next.apps[0].bundle_id, "com.google.Chrome");
        assert_eq!(next.apps[0].total_secs, 1_200);
        assert_eq!(
            next.apps[0].first_start,
            at(d + 1, 0.0),
            "clipped to midnight"
        );

        // A day nothing touched: no apps, and no display figure at all
        // (None, not Some(0)) so the caller can tell "off" from "unknown".
        let empty = read_daily_usage(&conn, d + 2, 30).unwrap();
        assert!(empty.apps.is_empty());
        assert_eq!(empty.display_on_secs, None);

        // D+1 has no session of its own, only Chrome's tail: still a
        // candidate day, filed in the tail's zone — otherwise the 1 200 s
        // above would never be digested.
        assert_eq!(
            local_days_with_rows(&conn, "/app/usage", None).unwrap(),
            vec![d, d + 1]
        );
        assert_eq!(
            modal_tz_offset(&conn, "/app/usage", d + 1).unwrap(),
            Some(TZ_PDT)
        );
        // A session that ends exactly at midnight belongs to its start
        // day alone, as read_daily_usage's strict bound already says.
        insert_row(
            &conn,
            &usage_row(5, "com.apple.Safari", at(d + 3, 23.0), at(d + 4, 0.0)),
        );
        assert_eq!(
            local_days_with_rows(&conn, "/app/usage", Some(d + 1)).unwrap(),
            vec![d + 3]
        );
        assert_eq!(modal_tz_offset(&conn, "/app/usage", d + 4).unwrap(), None);
    }

    /// Digest totals count every non-zero session, however short — the raw
    /// lane's floor drops 73% of `/app/usage` rows as Cmd-Tab blips, and a
    /// digest that dropped them too would undercount a day by hours. The
    /// `short_sessions` figure is what keeps that legible.
    #[test]
    fn short_sessions_are_counted_while_total_secs_still_includes_them() {
        let conn = fixture_db();
        let d = DAY_2026_09_14;
        insert_row(
            &conn,
            &usage_row(1, "com.apple.Notes", at(d, 9.0), at(d, 9.0) + 10.0),
        );
        insert_row(
            &conn,
            &usage_row(2, "com.apple.Notes", at(d, 10.0), at(d, 10.0) + 20.0),
        );
        insert_row(
            &conn,
            &usage_row(3, "com.apple.Notes", at(d, 11.0), at(d, 11.0) + 600.0),
        );
        // Exactly the threshold is not short (the raw floor ships it).
        insert_row(
            &conn,
            &usage_row(4, "com.apple.Notes", at(d, 12.0), at(d, 12.0) + 30.0),
        );
        // Zero-length rows are not sessions at all.
        insert_row(
            &conn,
            &usage_row(5, "com.apple.Notes", at(d, 13.0), at(d, 13.0)),
        );
        // A synced row never counts in a digest.
        insert_row(
            &conn,
            &usage_row(6, "com.apple.Notes", at(d, 14.0), at(d, 15.0))
                .synced_from("DEVICE-A", Some("iPhone18,2")),
        );

        let day = read_daily_usage(&conn, d, 30).unwrap();
        assert_eq!(day.apps.len(), 1);
        let notes = &day.apps[0];
        assert_eq!(notes.session_count, 4);
        assert_eq!(notes.short_sessions, 2);
        assert_eq!(notes.total_secs, 10 + 20 + 600 + 30);
        assert_eq!(notes.first_start, at(d, 9.0));
        assert_eq!(notes.last_end, at(d, 12.0) + 30.0);

        // `min_session_secs = 0` → nothing is short.
        assert_eq!(
            read_daily_usage(&conn, d, 0).unwrap().apps[0].short_sessions,
            0
        );
    }

    /// The gate probes: candidate days come from local `/app/usage` rows
    /// only and are bucketed in each row's own zone; coverage is the
    /// earliest *creation*; flush evidence is a row that *started* on or
    /// after the boundary; `max_end` is the latest close. Synced rows are
    /// invisible to all of them.
    #[test]
    fn digest_probes_see_local_rows_only_and_bucket_days_in_the_rows_own_zone() {
        let conn = fixture_db();
        let d = DAY_2026_09_14;
        assert_eq!(coverage_start(&conn, "/app/usage").unwrap(), None);
        assert_eq!(max_end(&conn, "/display/isBacklit").unwrap(), None);
        assert!(
            local_days_with_rows(&conn, "/app/usage", None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(modal_tz_offset(&conn, "/app/usage", d).unwrap(), None);
        assert_eq!(newest_tz_offset(&conn, "/app/usage").unwrap(), None);

        insert_row(
            &conn,
            &usage_row(1, "a", at(d, 9.0), at(d, 10.0)).created(Some(at(d, 10.5))),
        );
        insert_row(&conn, &usage_row(2, "a", at(d + 1, 9.0), at(d + 1, 10.0)));
        // 2026-09-15 20:00 in UTC+2 is 2026-09-16 05:00 in PDT terms — but
        // the row's *own* zone says 09-15, and that is where it is filed.
        insert_row(
            &conn,
            &usage_row(
                3,
                "a",
                at(d + 1, 20.0) - 9.0 * 3_600.0,
                at(d + 1, 21.0) - 9.0 * 3_600.0,
            )
            .tz_offset_secs(Some(7_200)),
        );
        // Synced rows: a much older creation, a much later start and end.
        insert_row(
            &conn,
            &usage_row(4, "a", at(d + 5, 9.0), at(d + 5, 10.0))
                .created(Some(at(d - 20, 0.0)))
                .synced_from("DEVICE-A", None),
        );
        insert_row(&conn, &display_on(5, at(d, 8.0), at(d, 9.0)));
        insert_row(
            &conn,
            &display_on(6, at(d + 9, 8.0), at(d + 9, 9.0)).synced_from("DEVICE-A", None),
        );

        assert_eq!(
            coverage_start(&conn, "/app/usage").unwrap(),
            Some(at(d, 10.5))
        );
        assert_eq!(
            local_days_with_rows(&conn, "/app/usage", None).unwrap(),
            vec![d, d + 1]
        );
        assert_eq!(
            local_days_with_rows(&conn, "/app/usage", Some(d)).unwrap(),
            vec![d + 1]
        );
        assert_eq!(
            modal_tz_offset(&conn, "/app/usage", d).unwrap(),
            Some(TZ_PDT)
        );
        // Day d+1 has one PDT row and one UTC+2 row: the tie breaks low.
        assert_eq!(
            modal_tz_offset(&conn, "/app/usage", d + 1).unwrap(),
            Some(TZ_PDT)
        );
        insert_row(
            &conn,
            &usage_row(7, "a", at(d + 1, 11.0), at(d + 1, 12.0)).tz_offset_secs(Some(7_200)),
        );
        assert_eq!(
            modal_tz_offset(&conn, "/app/usage", d + 1).unwrap(),
            Some(7_200)
        );
        assert_eq!(newest_tz_offset(&conn, "/app/usage").unwrap(), Some(7_200));

        assert!(has_local_row_on_or_after(&conn, "/app/usage", at(d + 1, 0.0)).unwrap());
        assert!(has_local_row_on_or_after(&conn, "/app/usage", at(d + 1, 11.0)).unwrap());
        assert!(
            !has_local_row_on_or_after(&conn, "/app/usage", at(d + 2, 0.0)).unwrap(),
            "the synced row on d+5 must not count as evidence"
        );
        assert_eq!(
            max_end(&conn, "/display/isBacklit").unwrap(),
            Some(at(d, 9.0))
        );
    }
}
