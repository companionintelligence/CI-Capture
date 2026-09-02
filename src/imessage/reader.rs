//! Reads `~/Library/Messages/chat.db` directly — the local counterpart to
//! the `bluebubbles` lane's network client, for the case where nothing runs
//! a BlueBubbles server at all: this Mac's own iMessage/SMS history.
//!
//! The schema, join shape, and Cocoa-epoch handling deliberately mirror
//! CI-Server's own `datasources/imessage.ts` (already run against real
//! multi-hundred-thousand-row corpora) rather than being derived fresh —
//! this is a SUBSET of that importer's fidelity, not a rewrite of its
//! design. Left out for this first pass, all noted as gaps in the
//! `imessage` module doc:
//! - `attributedBody` decoding (recent macOS leaves `text` NULL and stores
//!   the body only in this NSKeyedArchiver-encoded binary column — see the
//!   TS importer's own comment on `decodeAttributedBody`)
//! - full chat participant lists (group-chat membership) — only the chat's
//!   identifier/display name are resolved here, not who else is in it
//! - attachments, reactions, and reply threading

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};

/// Milliseconds from the Unix epoch to Apple's Cocoa epoch (2001-01-01T00:00:00Z).
const COCOA_EPOCH_OFFSET_MS: i64 = 978_307_200_000;

/// Above this, a raw `message.date` can only be nanoseconds — Cocoa *seconds*
/// stay under ~1e10 for centuries. Same threshold as the TS importer's
/// `parseCocoaDate`.
const NANOSECOND_THRESHOLD: i64 = 100_000_000_000;

/// Default page size for [`read_messages`] — matches `bluebubbles::client::PAGE_LIMIT`.
pub const DEFAULT_LIMIT: u32 = 1000;

/// Where a poll stopped, in chat.db's **own** units — deliberately not a Unix
/// millisecond timestamp, and deliberately compound.
///
/// Both halves are load-bearing:
/// - `date_raw` is `message.date` verbatim because macOS ≥10.13 stores it in
///   nanoseconds. Storing the cursor as Unix ms and converting back floors
///   away up to 999,999ns of the very row it came from, so `date > cursor`
///   lands *below* that row and re-matches it on every subsequent poll,
///   forever. On a real 433,398-row chat.db, 96.9% of rows are not
///   millisecond-aligned.
/// - `rowid` makes the comparison total. `message.date` is not unique — a real
///   chat.db has runs of up to 56 rows sharing one timestamp — so a date-only
///   cursor combined with `LIMIT` permanently skips the remainder of whatever
///   run the page boundary happens to fall inside.
///
/// Fixing either half alone just trades one bug for the other, which is why
/// they are one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImessageCursor {
    /// `message.date` exactly as chat.db stores it, no unit conversion applied.
    pub date_raw: i64,
    /// `message.ROWID` of the same row.
    pub rowid: i64,
}

/// `message.date` → Unix milliseconds, or `None` when the value cannot be a
/// real timestamp at all.
///
/// The unit is decided **per row**, matching the TS importer's
/// `parseCocoaDate`, rather than once per database from a sampled row as an
/// earlier version did. A whole chat.db really is consistent in practice, but
/// `WHERE date IS NOT NULL` does not exclude `date = 0`, and one such row
/// landing on the sample re-typed every other row in the file: a nanosecond
/// history read as seconds lands in the year 79,000,000.
///
/// `None` rather than a best-effort value because the caller's only other
/// option was `server_ingest`'s `unwrap_or_else(Utc::now)`, which stamped an
/// unreadable row with the current time, got it acknowledged, and advanced the
/// cursor past every real message behind it — with nothing logged.
fn cocoa_raw_to_unix_ms(raw: i64) -> Option<i64> {
    let ms_since_2001 = if raw > NANOSECOND_THRESHOLD {
        raw / 1_000_000
    } else {
        // Checked: a corrupt or hostile `date` (say `i64::MIN`) otherwise
        // panics on overflow in debug builds and takes the whole poll with it.
        raw.checked_mul(1000)?
    };
    let unix_ms = COCOA_EPOCH_OFFSET_MS.checked_add(ms_since_2001)?;
    // Must survive the `DateTime::from_timestamp_millis` in `server_ingest` —
    // anything it would reject is rejected here instead, where the row can be
    // named in a log line rather than silently re-dated.
    chrono::DateTime::from_timestamp_millis(unix_ms).map(|_| unix_ms)
}

/// One `message` row, joined with its sender handle and (when resolvable)
/// its chat. Deliberately not `crate::bluebubbles::client::BlueBubblesMessage`
/// — same *kind* of data, but a different provenance (local file vs. network
/// API) that `server_ingest::ingest_imessage_message` tags distinctly, same
/// as `ingest_vizier_event`/`ingest_file_event` stay separate despite some
/// structural overlap.
#[derive(Debug, Clone, PartialEq)]
pub struct ImessageRow {
    pub rowid: i64,
    pub guid: Option<String>,
    pub text: Option<String>,
    pub is_from_me: bool,
    pub service: Option<String>,
    /// The other party's handle (phone/email) — the sender on inbound, the
    /// per-row recipient hint on outbound when chat.db carries one. `None`
    /// on an outbound message where only the chat (not this column) names
    /// the recipient — chat.db never puts the owner's own handle here.
    pub handle: Option<String>,
    /// `message.date` verbatim — kept alongside the converted timestamp
    /// because only the raw value can serve as a cursor (see [`ImessageCursor`]).
    pub date_raw: i64,
    pub date_created_unix_ms: i64,
    pub chat_identifier: Option<String>,
    pub chat_display_name: Option<String>,
    pub attachment_count: i64,
    /// Everyone in this message's chat, as handle addresses. Empty for a chat
    /// the join resolves to nothing. On the corpus this was measured against,
    /// 106,880 messages sit in 513 group chats — resolving only the chat and
    /// not its members left every one of those without a membership, and left
    /// outbound messages with no recipient edges at all.
    pub participants: Vec<String>,
    /// `message.associated_message_type`. Non-zero means this row is a tapback
    /// on another message, not a message of its own — 34,332 of them in the
    /// reference corpus, 33,079 carrying generated text that quotes the
    /// original, so ingesting them as ordinary messages fills the corpus with
    /// near-duplicates of messages it already has.
    pub associated_message_type: i64,
    /// The message this one reacts to or replies under, when it does.
    pub associated_message_guid: Option<String>,
    pub thread_originator_guid: Option<String>,
    /// Set when the sender edited the message after sending. Without it the
    /// current wording is stored as though it were the original.
    pub date_edited_unix_ms: Option<i64>,
    /// Set when the sender unsent the message. Its `text` is then a body the
    /// sender explicitly withdrew.
    pub date_retracted_unix_ms: Option<i64>,
}

impl ImessageRow {
    /// True when this row is a tapback rather than a message.
    pub fn is_reaction(&self) -> bool {
        self.associated_message_type != 0
    }
}

impl ImessageRow {
    /// Where a poll that has ingested this row should resume.
    pub fn cursor(&self) -> ImessageCursor {
        ImessageCursor {
            date_raw: self.date_raw,
            rowid: self.rowid,
        }
    }
}

/// One page of rows, plus what reading it had to throw away.
#[derive(Debug, Default)]
pub struct MessagePage {
    pub rows: Vec<ImessageRow>,
    /// Rows whose `message.date` is not a representable timestamp. Counted and
    /// surfaced (each one is also logged with its rowid) rather than quietly
    /// omitted, because a page that silently shrinks is indistinguishable from
    /// one that had nothing in it.
    pub skipped_invalid_timestamp: usize,
    /// The last row the query returned, dropped ones included. The caller
    /// normally advances its cursor off an *ingested* row, so that a failure
    /// is retried rather than skipped — but a page whose every row was dropped
    /// leaves it nothing to advance with, and re-reading returns the same rows
    /// forever. This is the way past that: those rows can never become
    /// ingestible, so there is nothing being skipped that could have been
    /// retried.
    pub last_seen: Option<ImessageCursor>,
}

/// Open `chat.db` read-only. Doesn't create the file, doesn't write a
/// `-wal`/`-shm` sidecar next to a database this process doesn't own.
pub fn open_readonly(path: &std::path::Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", path.display()))
}

/// Up to `limit` messages strictly after `after`, in `(date, ROWID)` order (or
/// everything, oldest first, when `after` is `None`). Mirrors
/// `bluebubbles::poll_once`'s contract so both lanes' `run` loops read the
/// same way.
pub fn read_messages(
    conn: &Connection,
    after: Option<ImessageCursor>,
    limit: u32,
) -> Result<MessagePage> {
    let (after_date, after_rowid) = match after {
        Some(cursor) => (Some(cursor.date_raw), Some(cursor.rowid)),
        None => (None, None),
    };

    // chat.db's schema drifts across macOS releases (columns added/removed);
    // `attributedBody`/reactions/etc. are exactly that kind of column, which
    // is why the TS importer probes `PRAGMA table_info` first. This subset
    // sticks to columns present since chat.db's schema stabilized (10.12+),
    // so no probing is needed yet — add it here if that stops being true.
    //
    // The chat is resolved by a per-row subquery rather than by pre-loading
    // the whole `chat_message_join ⋈ chat` product into a map: that scanned
    // 388,115 rows on a real database on every poll to label at most `limit`
    // of them, and swallowed its own errors to boot. A message can technically
    // join more than one chat row; the lowest-ROWID join wins, which is both
    // deterministic and the same "first one found" answer the map gave — good
    // enough for "which conversation is this," not a claim about exhaustive
    // membership (see the module doc's participants gap).
    //
    // The `(date, ROWID) > (?1, ?2)` row-value comparison needs SQLite ≥3.15;
    // this crate bundles its own (3.4x), so it is not a host-version gamble.
    let mut stmt = conn
        .prepare(
            "SELECT
                message.ROWID AS rowid,
                message.guid AS guid,
                message.text AS text,
                message.attributedBody AS attributed_body,
                message.is_from_me AS is_from_me,
                message.service AS service,
                message.date AS date_raw,
                message.associated_message_type AS associated_message_type,
                message.associated_message_guid AS associated_message_guid,
                message.thread_originator_guid AS thread_originator_guid,
                message.date_edited AS date_edited,
                message.date_retracted AS date_retracted,
                handle.id AS handle,
                chat.chat_identifier AS chat_identifier,
                chat.display_name AS chat_display_name,
                (SELECT COUNT(*) FROM message_attachment_join maj WHERE maj.message_id = message.ROWID)
                    AS attachment_count,
                -- Chat membership, as one delimited string rather than a second
                -- query per row: the lane pages 1000 messages at a time, and a
                -- per-row round trip is what made the old chat lookup a full
                -- scan. US (0x1F) because a handle address can contain a comma.
                (SELECT group_concat(h2.id, char(31))
                   FROM chat_handle_join chj
                   JOIN handle h2 ON h2.ROWID = chj.handle_id
                  WHERE chj.chat_id = chat.ROWID) AS participants
             FROM message
             LEFT JOIN handle ON handle.ROWID = message.handle_id
             LEFT JOIN chat ON chat.ROWID = (
                 SELECT cmj.chat_id FROM chat_message_join cmj
                 WHERE cmj.message_id = message.ROWID
                 ORDER BY cmj.ROWID ASC LIMIT 1
             )
             WHERE message.date IS NOT NULL
               AND (?1 IS NULL OR (message.date, message.ROWID) > (?1, ?2))
             ORDER BY message.date ASC, message.ROWID ASC
             LIMIT ?3",
        )
        .context("preparing message query")?;

    let rows = stmt
        .query_map(params![after_date, after_rowid, limit], |row| {
            let rowid: i64 = row.get("rowid")?;
            let date_raw: i64 = row.get("date_raw")?;
            // Paired with every row, kept or not, so `last_seen` can be filled
            // in even for one that never becomes an `ImessageRow`.
            let seen = ImessageCursor { date_raw, rowid };
            let Some(date_created_unix_ms) = cocoa_raw_to_unix_ms(date_raw) else {
                log::warn!(
                    "imessage: skipping rowid={rowid}, message.date={date_raw} is not a representable timestamp"
                );
                return Ok((seen, None));
            };

            // `text` first, `attributedBody` only as a fallback: where both
            // exist they agree (CI-Server's decoder reproduced `text`
            // byte-for-byte across 40,000 such messages), so preferring the
            // plain column keeps the cheap path cheap and the decode for the
            // 40,961 rows that actually need it.
            let text: Option<String> = row.get("text")?;
            let text = match text {
                Some(t) if !t.trim().is_empty() => Some(t),
                _ => {
                    let blob: Option<Vec<u8>> = row.get("attributed_body")?;
                    super::attributed_body::decode_attributed_body(blob.as_deref())
                }
            };

            let participants = row
                .get::<_, Option<String>>("participants")?
                .map(|joined| {
                    joined
                        .split('\u{1F}')
                        .filter(|p| !p.trim().is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            Ok((seen, Some(ImessageRow {
                rowid,
                guid: row.get("guid")?,
                text,
                // Nullable in the real schema despite always being 0/1 in
                // practice; a strict `i64` get failed the *entire* page on the
                // one row that wasn't.
                is_from_me: row.get::<_, Option<i64>>("is_from_me")?.unwrap_or(0) == 1,
                service: row.get("service")?,
                handle: row.get("handle")?,
                date_raw,
                date_created_unix_ms,
                chat_identifier: row.get("chat_identifier")?,
                chat_display_name: row.get("chat_display_name")?,
                attachment_count: row.get("attachment_count")?,
                participants,
                // Nullable in the real schema, and 0 is "not a reaction".
                associated_message_type: row
                    .get::<_, Option<i64>>("associated_message_type")?
                    .unwrap_or(0),
                associated_message_guid: row.get("associated_message_guid")?,
                thread_originator_guid: row.get("thread_originator_guid")?,
                // Same Cocoa-epoch units as `date`, and 0 means "never edited"
                // rather than "edited at the epoch" — so it must not become
                // Some(2001-01-01).
                date_edited_unix_ms: row
                    .get::<_, Option<i64>>("date_edited")?
                    .filter(|&d| d != 0)
                    .and_then(cocoa_raw_to_unix_ms),
                date_retracted_unix_ms: row
                    .get::<_, Option<i64>>("date_retracted")?
                    .filter(|&d| d != 0)
                    .and_then(cocoa_raw_to_unix_ms),
            })))
        })
        .context("running message query")?;

    let mut page = MessagePage::default();
    for row in rows {
        let (seen, row) = row.context("reading message rows")?;
        page.last_seen = Some(seen);
        match row {
            Some(row) => page.rows.push(row),
            None => page.skipped_invalid_timestamp += 1,
        }
    }
    Ok(page)
}

/// A `chat.db`-shaped schema for tests — the tables `read_messages` touches,
/// with enough of the real column list around them (`message` alone has ~100
/// columns on macOS 15) that a query relying on column *position*, on a
/// column being NOT NULL, or on `date` being the last thing in the row would
/// fail here rather than only against a real database. Shared with `e2e` so
/// both exercise the same layout.
#[cfg(test)]
pub(crate) const FIXTURE_SCHEMA: &str = "
    CREATE TABLE handle (
        ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
        id TEXT NOT NULL,
        country TEXT,
        service TEXT NOT NULL,
        uncanonicalized_id TEXT
    );
    CREATE TABLE chat (
        ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
        guid TEXT UNIQUE NOT NULL,
        style INTEGER,
        state INTEGER,
        account_id TEXT,
        chat_identifier TEXT,
        service_name TEXT,
        room_name TEXT,
        display_name TEXT
    );
    CREATE TABLE chat_handle_join (
        chat_id INTEGER REFERENCES chat (ROWID) ON DELETE CASCADE,
        handle_id INTEGER REFERENCES handle (ROWID) ON DELETE CASCADE,
        UNIQUE (chat_id, handle_id)
    );
    CREATE TABLE chat_message_join (
        chat_id INTEGER REFERENCES chat (ROWID) ON DELETE CASCADE,
        message_id INTEGER REFERENCES message (ROWID) ON DELETE CASCADE,
        message_date INTEGER DEFAULT 0,
        PRIMARY KEY (chat_id, message_id)
    );
    CREATE TABLE attachment (
        ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
        guid TEXT UNIQUE NOT NULL,
        filename TEXT,
        mime_type TEXT,
        transfer_name TEXT
    );
    CREATE TABLE message_attachment_join (
        message_id INTEGER REFERENCES message (ROWID) ON DELETE CASCADE,
        attachment_id INTEGER REFERENCES attachment (ROWID) ON DELETE CASCADE
    );
    CREATE TABLE message (
        ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
        guid TEXT UNIQUE NOT NULL,
        text TEXT,
        replace INTEGER DEFAULT 0,
        service_center TEXT,
        handle_id INTEGER DEFAULT 0,
        subject TEXT,
        country TEXT,
        attributedBody BLOB,
        version INTEGER DEFAULT 0,
        type INTEGER DEFAULT 0,
        service TEXT,
        account TEXT,
        account_guid TEXT,
        error INTEGER DEFAULT 0,
        date INTEGER,
        date_read INTEGER,
        date_delivered INTEGER,
        is_delivered INTEGER DEFAULT 0,
        is_from_me INTEGER DEFAULT 0,
        cache_has_attachments INTEGER DEFAULT 0,
        cache_roomnames TEXT,
        item_type INTEGER DEFAULT 0,
        associated_message_guid TEXT,
        associated_message_type INTEGER DEFAULT 0,
        thread_originator_guid TEXT,
        date_edited INTEGER DEFAULT 0,
        date_retracted INTEGER DEFAULT 0
    );
    CREATE INDEX chat_message_join_idx_message_id_only ON chat_message_join (message_id);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn
    }

    /// `message.guid` is `UNIQUE NOT NULL` in the real schema, so every
    /// fixture row needs one even when the test is about something else.
    fn insert_message(conn: &Connection, rowid: i64, date_raw: i64) {
        conn.execute(
            "INSERT INTO message (ROWID, guid, is_from_me, date) VALUES (?1, ?2, 0, ?3)",
            params![rowid, format!("MSG-{rowid}"), date_raw],
        )
        .unwrap();
    }

    fn guids(page: &MessagePage) -> Vec<String> {
        page.rows
            .iter()
            .map(|r| r.guid.clone().unwrap())
            .collect::<Vec<_>>()
    }

    /// 2026-01-01T00:00:00Z in Cocoa units — computed independently (not via
    /// this module's own conversion functions) so the tests can't pass by
    /// construction. Nanoseconds is the default throughout: it is what every
    /// macOS since 10.13 actually writes, and a suite that only ever fixtured
    /// *seconds* is exactly why the millisecond-cursor bug stayed invisible.
    const JAN_1_2026_COCOA_SECONDS: i64 = 788_918_400;
    const JAN_1_2026_COCOA_NANOS: i64 = 788_918_400_000_000_000;
    const JAN_1_2026_UNIX_MS: i64 = 1_767_225_600_000;
    /// `date = 0` — Cocoa's own epoch, and a value real databases contain.
    const COCOA_EPOCH_UNIX_MS: i64 = 978_307_200_000;

    #[test]
    fn cocoa_seconds_convert_to_the_known_unix_timestamp() {
        assert_eq!(
            cocoa_raw_to_unix_ms(JAN_1_2026_COCOA_SECONDS),
            Some(JAN_1_2026_UNIX_MS)
        );
    }

    /// Replaces a test that called only the forward conversion, on an exact
    /// multiple of 1e6, and would therefore have passed with the nanosecond
    /// arm deleted entirely. The value here is the shape that actually broke
    /// the lane: 373,504ns past its millisecond, taken from the newest row of
    /// a real 433,398-row chat.db.
    #[test]
    fn a_sub_millisecond_nanosecond_date_truncates_to_its_millisecond() {
        assert_eq!(
            cocoa_raw_to_unix_ms(JAN_1_2026_COCOA_NANOS + 373_504),
            Some(JAN_1_2026_UNIX_MS)
        );
        // The same instant in the other unit reaches the same answer through
        // the other arm — the whole point of typing per row. Were the
        // nanosecond arm gone, the assertion above would overflow to `None`.
        assert_eq!(
            cocoa_raw_to_unix_ms(JAN_1_2026_COCOA_SECONDS),
            cocoa_raw_to_unix_ms(JAN_1_2026_COCOA_NANOS)
        );
    }

    #[test]
    fn a_date_that_cannot_be_a_timestamp_converts_to_none_rather_than_panicking() {
        assert_eq!(cocoa_raw_to_unix_ms(i64::MIN), None);
    }

    /// The regression test for the bug this lane shipped with: an idle Mac
    /// re-sent its newest message on every poll, forever, because the cursor
    /// round-tripped through Unix milliseconds.
    #[test]
    fn feeding_a_returned_cursor_back_in_never_returns_the_same_row_twice() {
        let conn = fixture_db();
        for (rowid, offset_ns) in [(1, 373_504), (2, 60_000_000_001), (3, 120_000_000_007)] {
            insert_message(&conn, rowid, JAN_1_2026_COCOA_NANOS + offset_ns);
        }

        let first = read_messages(&conn, None, 10).unwrap();
        assert_eq!(guids(&first), ["MSG-1", "MSG-2", "MSG-3"]);

        let cursor = first.rows.last().unwrap().cursor();
        let second = read_messages(&conn, Some(cursor), 10).unwrap();
        assert!(
            second.rows.is_empty(),
            "polling again with the cursor the first poll returned re-sent {:?}",
            guids(&second)
        );
    }

    /// The other half of the same fix: `message.date` is not unique, so a
    /// page boundary inside a run of identical timestamps used to skip the
    /// rest of that run permanently. Real runs reach 56 rows.
    #[test]
    fn a_run_of_tied_timestamps_pages_without_skipping_or_duplicating() {
        let conn = fixture_db();
        for rowid in 1..=5 {
            insert_message(&conn, rowid, JAN_1_2026_COCOA_NANOS);
        }

        let mut seen: Vec<String> = Vec::new();
        let mut cursor = None;
        // Page size 2 against 5 tied rows: every boundary lands mid-run.
        for _ in 0..10 {
            let page = read_messages(&conn, cursor, 2).unwrap();
            if page.rows.is_empty() {
                break;
            }
            cursor = Some(page.rows.last().unwrap().cursor());
            seen.extend(guids(&page));
        }

        assert_eq!(seen, ["MSG-1", "MSG-2", "MSG-3", "MSG-4", "MSG-5"]);
    }

    /// `WHERE date IS NOT NULL` does not exclude `date = 0`, and such a row
    /// arriving newest by ROWID used to type the entire database as seconds —
    /// sending every real (nanosecond) row to the year 79,000,000.
    #[test]
    fn the_date_unit_is_decided_per_row_not_from_one_sampled_row() {
        let conn = fixture_db();
        insert_message(&conn, 1, JAN_1_2026_COCOA_NANOS);
        insert_message(&conn, 2, 0);

        let page = read_messages(&conn, None, 10).unwrap();
        assert_eq!(guids(&page), ["MSG-2", "MSG-1"], "ascending by date");
        assert_eq!(page.rows[0].date_created_unix_ms, COCOA_EPOCH_UNIX_MS);
        assert_eq!(page.rows[1].date_created_unix_ms, JAN_1_2026_UNIX_MS);
    }

    #[test]
    fn an_unrepresentable_date_is_skipped_and_counted_not_stamped_with_now() {
        let conn = fixture_db();
        insert_message(&conn, 1, i64::MIN);
        insert_message(&conn, 2, JAN_1_2026_COCOA_NANOS);

        let page = read_messages(&conn, None, 10).unwrap();
        assert_eq!(guids(&page), ["MSG-2"]);
        assert_eq!(page.skipped_invalid_timestamp, 1);
    }

    /// A page with nothing left in it after the drops still reports where it
    /// got to, so the caller has something to move past. Without it, a whole
    /// page of unreadable rows would be re-read on every poll forever.
    #[test]
    fn an_entirely_dropped_page_still_reports_where_it_reached() {
        let conn = fixture_db();
        insert_message(&conn, 1, i64::MIN);
        insert_message(&conn, 2, i64::MIN + 1);

        let page = read_messages(&conn, None, 10).unwrap();
        assert!(page.rows.is_empty());
        assert_eq!(page.skipped_invalid_timestamp, 2);
        assert_eq!(
            page.last_seen,
            Some(ImessageCursor {
                date_raw: i64::MIN + 1,
                rowid: 2
            })
        );
    }

    #[test]
    fn an_empty_page_reaches_nothing_to_report() {
        let conn = fixture_db();
        assert_eq!(read_messages(&conn, None, 10).unwrap().last_seen, None);
    }

    #[test]
    fn a_null_is_from_me_reads_as_inbound_rather_than_failing_the_whole_page() {
        let conn = fixture_db();
        conn.execute(
            "INSERT INTO message (ROWID, guid, is_from_me, date) VALUES (1, 'MSG-1', NULL, ?1)",
            params![JAN_1_2026_COCOA_NANOS],
        )
        .unwrap();

        let page = read_messages(&conn, None, 10).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert!(!page.rows[0].is_from_me);
    }

    #[test]
    fn reads_an_inbound_message_with_its_chat_and_sender() {
        let conn = fixture_db();
        conn.execute_batch(
            "
            INSERT INTO handle (ROWID, id, service) VALUES (1, '+15551234567', 'iMessage');
            INSERT INTO chat (ROWID, guid, chat_identifier, display_name)
                VALUES (1, 'iMessage;-;+15551234567', '+15551234567', NULL);
            ",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, is_from_me, service, date, handle_id)
             VALUES (1, 'MSG-1', 'hello there', 0, 'iMessage', ?1, 1)",
            params![JAN_1_2026_COCOA_NANOS],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1)",
            [],
        )
        .unwrap();

        let page = read_messages(&conn, None, 10).unwrap();
        assert_eq!(page.rows.len(), 1);
        let row = &page.rows[0];
        assert_eq!(row.guid.as_deref(), Some("MSG-1"));
        assert_eq!(row.text.as_deref(), Some("hello there"));
        assert!(!row.is_from_me);
        assert_eq!(row.handle.as_deref(), Some("+15551234567"));
        assert_eq!(row.chat_identifier.as_deref(), Some("+15551234567"));
        assert_eq!(row.date_created_unix_ms, JAN_1_2026_UNIX_MS);
        assert_eq!(row.date_raw, JAN_1_2026_COCOA_NANOS);
        assert_eq!(row.attachment_count, 0);
    }

    /// The chat is resolved per row now, so each message must still get its
    /// own chat and only its own — the failure mode a shared lookup map would
    /// have hidden.
    #[test]
    fn each_message_resolves_its_own_chat() {
        let conn = fixture_db();
        conn.execute_batch(
            "
            INSERT INTO chat (ROWID, guid, chat_identifier, display_name)
                VALUES (1, 'chat-1', '+15550000001', 'Alice'),
                       (2, 'chat-2', '+15550000002', 'Bob');
            ",
        )
        .unwrap();
        insert_message(&conn, 1, JAN_1_2026_COCOA_NANOS);
        insert_message(&conn, 2, JAN_1_2026_COCOA_NANOS + 1);
        insert_message(&conn, 3, JAN_1_2026_COCOA_NANOS + 2);
        conn.execute_batch(
            "
            INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1), (2, 2);
            ",
        )
        .unwrap();

        let page = read_messages(&conn, None, 10).unwrap();
        assert_eq!(page.rows[0].chat_display_name.as_deref(), Some("Alice"));
        assert_eq!(page.rows[1].chat_display_name.as_deref(), Some("Bob"));
        assert_eq!(page.rows[2].chat_display_name, None, "no join row at all");
    }

    /// A message joining two chats picks the lowest-ROWID join deterministically
    /// rather than whichever row the scan happened to reach first.
    #[test]
    fn a_message_in_two_chats_resolves_to_one_of_them_deterministically() {
        let conn = fixture_db();
        conn.execute_batch(
            "
            INSERT INTO chat (ROWID, guid, chat_identifier, display_name)
                VALUES (1, 'chat-1', 'first', 'First'),
                       (2, 'chat-2', 'second', 'Second');
            ",
        )
        .unwrap();
        insert_message(&conn, 1, JAN_1_2026_COCOA_NANOS);
        conn.execute_batch(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (2, 1), (1, 1);",
        )
        .unwrap();

        for _ in 0..3 {
            let page = read_messages(&conn, None, 10).unwrap();
            assert_eq!(page.rows[0].chat_display_name.as_deref(), Some("Second"));
        }
    }

    #[test]
    fn cursor_excludes_everything_at_or_before_it() {
        let conn = fixture_db();
        for (rowid, offset_ns) in [(1, 0), (2, 60_000_000_000), (3, 120_000_000_000)] {
            insert_message(&conn, rowid, JAN_1_2026_COCOA_NANOS + offset_ns);
        }

        let cursor = ImessageCursor {
            date_raw: JAN_1_2026_COCOA_NANOS + 60_000_000_000, // exactly message 2's
            rowid: 2,
        };
        let page = read_messages(&conn, Some(cursor), 10).unwrap();
        assert_eq!(guids(&page), ["MSG-3"]);
    }

    #[test]
    fn limit_is_respected_and_order_is_ascending() {
        let conn = fixture_db();
        for (rowid, offset_ns) in [(1, 30_000_000_000), (2, 0), (3, 60_000_000_000)] {
            insert_message(&conn, rowid, JAN_1_2026_COCOA_NANOS + offset_ns);
        }

        let page = read_messages(&conn, None, 2).unwrap();
        assert_eq!(guids(&page), ["MSG-2", "MSG-1"]);
    }

    #[test]
    fn attachment_count_reflects_the_join_table() {
        let conn = fixture_db();
        insert_message(&conn, 1, JAN_1_2026_COCOA_NANOS);
        conn.execute_batch(
            "
            INSERT INTO attachment (ROWID, guid, transfer_name)
                VALUES (100, 'att-100', 'IMG_0042.HEIC'), (101, 'att-101', 'IMG_0043.HEIC');
            INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (1, 100);
            INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (1, 101);
            ",
        )
        .unwrap();

        let page = read_messages(&conn, None, 10).unwrap();
        assert_eq!(page.rows[0].attachment_count, 2);
    }

    #[test]
    fn a_message_with_no_chat_row_still_reads_with_none_for_chat_fields() {
        let conn = fixture_db();
        insert_message(&conn, 1, JAN_1_2026_COCOA_NANOS);

        let page = read_messages(&conn, None, 10).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].chat_identifier, None);
        assert_eq!(page.rows[0].chat_display_name, None);
    }
}

#[cfg(test)]
mod fidelity_tests {
    use super::*;
    use rusqlite::params;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn
    }

    /// A blob in the shape `attributedBody` actually takes — see
    /// `super::super::attributed_body` for the format.
    fn attributed(body: &str) -> Vec<u8> {
        let mut out = Vec::from(&b"\x04\x0Bstreamtyped\x81\xE8\x03"[..]);
        out.extend_from_slice(b"NSString");
        out.push(0x2b);
        out.push(body.len() as u8);
        out.extend_from_slice(body.as_bytes());
        out
    }

    fn only_row(conn: &Connection) -> ImessageRow {
        let page = read_messages(conn, None, 10).unwrap();
        assert_eq!(page.rows.len(), 1, "expected exactly one row");
        page.rows.into_iter().next().unwrap()
    }

    /// The gap that costs the most: 40,961 of 433,411 messages in the
    /// reference corpus carry no `text` at all, and shipped as empty rows.
    #[test]
    fn a_message_with_no_text_recovers_its_body_from_attributedbody() {
        let conn = db();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, attributedBody, is_from_me, date)
             VALUES (1, 'M1', NULL, ?1, 0, 788918400000000000)",
            params![attributed("recovered from the blob")],
        )
        .unwrap();

        assert_eq!(
            only_row(&conn).text.as_deref(),
            Some("recovered from the blob")
        );
    }

    /// `text` wins where both exist. They agree in practice, and preferring
    /// the plain column keeps the decode off the hot path.
    #[test]
    fn plain_text_is_preferred_over_the_blob() {
        let conn = db();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, attributedBody, is_from_me, date)
             VALUES (1, 'M1', 'the plain column', ?1, 0, 788918400000000000)",
            params![attributed("the blob")],
        )
        .unwrap();

        assert_eq!(only_row(&conn).text.as_deref(), Some("the plain column"));
    }

    /// Whitespace-only `text` is not a body — these fall through to the blob
    /// rather than shipping a message made of spaces.
    #[test]
    fn blank_text_falls_through_to_the_blob() {
        let conn = db();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, attributedBody, is_from_me, date)
             VALUES (1, 'M1', '   ', ?1, 0, 788918400000000000)",
            params![attributed("the real body")],
        )
        .unwrap();

        assert_eq!(only_row(&conn).text.as_deref(), Some("the real body"));
    }

    /// Neither source usable: still a row (it happened, and it has a sender
    /// and a time), just without words. Must not panic or drop.
    #[test]
    fn a_message_with_neither_text_nor_a_decodable_blob_survives_without_a_body() {
        let conn = db();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, attributedBody, is_from_me, date)
             VALUES (1, 'M1', NULL, NULL, 0, 788918400000000000)",
            [],
        )
        .unwrap();

        assert_eq!(only_row(&conn).text, None);
    }

    /// 106,880 messages sit in group chats in the reference corpus, and
    /// resolving only the chat left every one of them with no membership.
    #[test]
    fn a_group_chat_message_carries_its_participants() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO handle (ROWID, id, service) VALUES (1, '+15550001', 'iMessage'), (2, '+15550002', 'iMessage'), (3, 'a@b.com', 'iMessage');
             INSERT INTO chat (ROWID, guid, chat_identifier, display_name, style)
                VALUES (1, 'chat;+;group', 'group-1', 'Weekend Plans', 43);
             INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (1,1),(1,2),(1,3);
             INSERT INTO message (ROWID, guid, text, handle_id, is_from_me, date)
                VALUES (1, 'M1', 'hi all', 1, 0, 788918400000000000);
             INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1);",
        )
        .unwrap();

        let row = only_row(&conn);
        let mut got = row.participants.clone();
        got.sort();
        assert_eq!(got, vec!["+15550001", "+15550002", "a@b.com"]);
        assert_eq!(row.chat_display_name.as_deref(), Some("Weekend Plans"));
    }

    /// A handle address can contain a comma, so the participant list is joined
    /// on US (0x1F) rather than a comma — splitting on the wrong delimiter
    /// would shred one address into two.
    #[test]
    fn a_participant_address_containing_a_comma_stays_one_participant() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO handle (ROWID, id, service) VALUES (1, 'weird,address@example.com', 'iMessage');
             INSERT INTO chat (ROWID, guid, chat_identifier, style) VALUES (1, 'c', 'c1', 45);
             INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (1, 1);
             INSERT INTO message (ROWID, guid, text, is_from_me, date)
                VALUES (1, 'M1', 'hi', 0, 788918400000000000);
             INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1);",
        )
        .unwrap();

        assert_eq!(
            only_row(&conn).participants,
            vec!["weird,address@example.com"]
        );
    }

    /// A one-to-one chat with no membership rows must yield an empty list, not
    /// a list containing one empty string.
    #[test]
    fn a_chat_with_no_membership_rows_yields_no_participants() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO chat (ROWID, guid, chat_identifier, style) VALUES (1, 'c', 'c1', 45);
             INSERT INTO message (ROWID, guid, text, is_from_me, date)
                VALUES (1, 'M1', 'hi', 0, 788918400000000000);
             INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1);",
        )
        .unwrap();

        assert!(only_row(&conn).participants.is_empty());
    }

    /// 34,332 tapbacks in the reference corpus. The reader surfaces the type
    /// so the lane can refuse them; it does not filter them itself, because
    /// the cursor still has to advance over them.
    #[test]
    fn a_tapback_is_identifiable_as_a_reaction() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO message (ROWID, guid, text, is_from_me, date,
                                  associated_message_type, associated_message_guid)
             VALUES (1, 'M1', 'Liked a message', 0, 788918400000000000, 2000, 'p:0/M0'),
                    (2, 'M2', 'an actual message', 0, 788918400000000001, 0, NULL);",
        )
        .unwrap();

        let page = read_messages(&conn, None, 10).unwrap();
        assert!(page.rows[0].is_reaction());
        assert_eq!(
            page.rows[0].associated_message_guid.as_deref(),
            Some("p:0/M0")
        );
        assert!(!page.rows[1].is_reaction());
    }

    #[test]
    fn replies_and_edits_are_carried() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO message (ROWID, guid, text, is_from_me, date,
                                  thread_originator_guid, date_edited)
             VALUES (1, 'M1', 'a reply', 0, 788918400000000000, 'ORIGIN-GUID', 788918500000000000);",
        )
        .unwrap();

        let row = only_row(&conn);
        assert_eq!(row.thread_originator_guid.as_deref(), Some("ORIGIN-GUID"));
        assert!(row.date_edited_unix_ms.is_some());
    }

    /// `date_edited = 0` means "never edited", not "edited at the Cocoa
    /// epoch". Mapping it to Some(2001-01-01) would mark every unedited
    /// message as edited — which is most of them.
    #[test]
    fn an_unedited_message_reports_no_edit_time() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO message (ROWID, guid, text, is_from_me, date, date_edited, date_retracted)
             VALUES (1, 'M1', 'never touched', 0, 788918400000000000, 0, 0);",
        )
        .unwrap();

        let row = only_row(&conn);
        assert_eq!(row.date_edited_unix_ms, None);
        assert_eq!(row.date_retracted_unix_ms, None);
    }
}
