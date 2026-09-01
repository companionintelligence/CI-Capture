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
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// Milliseconds from the Unix epoch to Apple's Cocoa epoch (2001-01-01T00:00:00Z).
const COCOA_EPOCH_OFFSET_MS: i64 = 978_307_200_000;

/// Default page size for [`read_messages`] — matches `bluebubbles::client::PAGE_LIMIT`.
pub const DEFAULT_LIMIT: u32 = 1000;

/// macOS ≤10.12 stores `message.date` as seconds since the Cocoa epoch;
/// 10.13+/iOS 11+ stores nanoseconds. A whole `chat.db` is consistent one way
/// or the other, so this is detected once per connection (see
/// [`detect_date_unit`]) rather than per row like the TS importer does — that
/// lets a cursor be pushed down into SQL as a real `WHERE date > ?`, which
/// matters on a database that can hold hundreds of thousands of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CocoaDateUnit {
    Seconds,
    Nanoseconds,
}

fn cocoa_raw_to_unix_ms(raw: i64, unit: CocoaDateUnit) -> i64 {
    let ms_since_2001 = match unit {
        CocoaDateUnit::Nanoseconds => raw / 1_000_000,
        CocoaDateUnit::Seconds => raw * 1000,
    };
    COCOA_EPOCH_OFFSET_MS + ms_since_2001
}

fn unix_ms_to_cocoa_raw(unix_ms: i64, unit: CocoaDateUnit) -> i64 {
    let ms_since_2001 = unix_ms - COCOA_EPOCH_OFFSET_MS;
    match unit {
        CocoaDateUnit::Nanoseconds => ms_since_2001 * 1_000_000,
        CocoaDateUnit::Seconds => ms_since_2001 / 1000,
    }
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
    pub date_created_unix_ms: i64,
    pub chat_identifier: Option<String>,
    pub chat_display_name: Option<String>,
    pub attachment_count: i64,
}

fn detect_date_unit(conn: &Connection) -> Result<CocoaDateUnit> {
    let sample: Option<i64> = conn
        .query_row(
            "SELECT date FROM message WHERE date IS NOT NULL ORDER BY ROWID DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("sampling message.date to detect its unit")?;
    Ok(match sample {
        // Seconds-since-2001 stays under ~1e10 for centuries; anything past
        // 1e11 can only be nanoseconds. Matches the TS importer's own
        // `parseCocoaDate` threshold.
        Some(v) if v > 100_000_000_000 => CocoaDateUnit::Nanoseconds,
        _ => CocoaDateUnit::Seconds,
    })
}

/// Open `chat.db` read-only. Doesn't create the file, doesn't write a
/// `-wal`/`-shm` sidecar next to a database this process doesn't own.
pub fn open_readonly(path: &std::path::Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", path.display()))
}

/// Up to `limit` messages with `date_created_unix_ms > after` (or everything,
/// oldest first, when `after` is `None`), ascending. Mirrors
/// `bluebubbles::poll_once`'s contract so both lanes' `run` loops read the
/// same way.
pub fn read_messages(
    conn: &Connection,
    after: Option<i64>,
    limit: u32,
) -> Result<Vec<ImessageRow>> {
    let unit = detect_date_unit(conn)?;
    let after_raw = after.map(|ms| unix_ms_to_cocoa_raw(ms, unit));

    // chat.db's schema drifts across macOS releases (columns added/removed);
    // `attributedBody`/reactions/etc. are exactly that kind of column, which
    // is why the TS importer probes `PRAGMA table_info` first. This subset
    // sticks to columns present since chat.db's schema stabilized (10.12+),
    // so no probing is needed yet — add it here if that stops being true.
    let mut stmt = conn
        .prepare(
            "SELECT
                message.ROWID AS rowid,
                message.guid AS guid,
                message.text AS text,
                message.is_from_me AS is_from_me,
                message.service AS service,
                message.date AS date_raw,
                handle.id AS handle,
                (SELECT COUNT(*) FROM message_attachment_join maj WHERE maj.message_id = message.ROWID)
                    AS attachment_count
             FROM message
             LEFT JOIN handle ON handle.ROWID = message.handle_id
             WHERE message.date IS NOT NULL
               AND (?1 IS NULL OR message.date > ?1)
             ORDER BY message.date ASC
             LIMIT ?2",
        )
        .context("preparing message query")?;

    let mut chat_by_message = chat_lookup(conn).unwrap_or_default();

    let rows = stmt
        .query_map(params![after_raw, limit], |row| {
            let rowid: i64 = row.get("rowid")?;
            let date_raw: i64 = row.get("date_raw")?;
            let (chat_identifier, chat_display_name) =
                chat_by_message.remove(&rowid).unwrap_or((None, None));

            Ok(ImessageRow {
                rowid,
                guid: row.get("guid")?,
                text: row.get("text")?,
                is_from_me: row.get::<_, i64>("is_from_me")? == 1,
                service: row.get("service")?,
                handle: row.get("handle")?,
                date_created_unix_ms: cocoa_raw_to_unix_ms(date_raw, unit),
                chat_identifier,
                chat_display_name,
                attachment_count: row.get("attachment_count")?,
            })
        })
        .context("running message query")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("reading message rows")?;

    Ok(rows)
}

/// `chat.chat_identifier` + `chat.display_name` for one chat.
type ChatIdentity = (Option<String>, Option<String>);

/// `message.ROWID` → its chat's identity, for whichever one chat each
/// message belongs to. A message can technically join more than one chat
/// row; like the TS importer, the first one found wins — good enough for
/// "which conversation is this," not a claim about exhaustive membership
/// (see the module doc's participants gap).
fn chat_lookup(conn: &Connection) -> Result<std::collections::HashMap<i64, ChatIdentity>> {
    let mut stmt = conn.prepare(
        "SELECT cmj.message_id AS message_id, c.chat_identifier AS chat_identifier, c.display_name AS display_name
         FROM chat_message_join cmj
         JOIN chat c ON c.ROWID = cmj.chat_id",
    )?;
    let mut map = std::collections::HashMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>("message_id")?,
            row.get::<_, Option<String>>("chat_identifier")?,
            row.get::<_, Option<String>>("display_name")?,
        ))
    })?;
    for row in rows {
        let (message_id, identifier, display_name) = row?;
        map.entry(message_id).or_insert((identifier, display_name));
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `chat.db`-shaped schema — just the tables/columns
    /// `read_messages` actually touches — seeded in memory. Lets the query
    /// logic (joins, cursor pushdown, epoch conversion) be exercised without
    /// Full Disk Access or a real Messages history.
    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
            CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, display_name TEXT);
            CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
            CREATE TABLE message_attachment_join (message_id INTEGER, attachment_id INTEGER);
            CREATE TABLE message (
                ROWID INTEGER PRIMARY KEY,
                guid TEXT,
                text TEXT,
                is_from_me INTEGER,
                service TEXT,
                date INTEGER,
                handle_id INTEGER
            );
            ",
        )
        .unwrap();
        conn
    }

    /// 2026-01-01T00:00:00Z in Cocoa **seconds** — computed independently
    /// (not via this module's own conversion function) so the test can't pass
    /// by construction.
    const JAN_1_2026_COCOA_SECONDS: i64 = 788_918_400;
    const JAN_1_2026_UNIX_MS: i64 = 1_767_225_600_000;

    #[test]
    fn cocoa_seconds_round_trip_to_the_known_unix_timestamp() {
        assert_eq!(
            cocoa_raw_to_unix_ms(JAN_1_2026_COCOA_SECONDS, CocoaDateUnit::Seconds),
            JAN_1_2026_UNIX_MS
        );
        assert_eq!(
            unix_ms_to_cocoa_raw(JAN_1_2026_UNIX_MS, CocoaDateUnit::Seconds),
            JAN_1_2026_COCOA_SECONDS
        );
    }

    #[test]
    fn cocoa_nanoseconds_round_trip_to_the_same_unix_timestamp() {
        let nanos = JAN_1_2026_COCOA_SECONDS * 1_000_000_000;
        assert_eq!(
            cocoa_raw_to_unix_ms(nanos, CocoaDateUnit::Nanoseconds),
            JAN_1_2026_UNIX_MS
        );
    }

    #[test]
    fn detects_seconds_vs_nanoseconds_from_a_sampled_row() {
        let conn = fixture_db();
        conn.execute(
            "INSERT INTO message (ROWID, date) VALUES (1, ?1)",
            params![JAN_1_2026_COCOA_SECONDS],
        )
        .unwrap();
        assert_eq!(detect_date_unit(&conn).unwrap(), CocoaDateUnit::Seconds);

        conn.execute("DELETE FROM message", []).unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, date) VALUES (2, ?1)",
            params![JAN_1_2026_COCOA_SECONDS * 1_000_000_000],
        )
        .unwrap();
        assert_eq!(detect_date_unit(&conn).unwrap(), CocoaDateUnit::Nanoseconds);
    }

    #[test]
    fn reads_an_inbound_message_with_its_chat_and_sender() {
        let conn = fixture_db();
        conn.execute_batch(
            "
            INSERT INTO handle (ROWID, id) VALUES (1, '+15551234567');
            INSERT INTO chat (ROWID, chat_identifier, display_name) VALUES (1, '+15551234567', NULL);
            ",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, is_from_me, service, date, handle_id)
             VALUES (1, 'MSG-1', 'hello there', 0, 'iMessage', ?1, 1)",
            params![JAN_1_2026_COCOA_SECONDS],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1)",
            [],
        )
        .unwrap();

        let rows = read_messages(&conn, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.guid.as_deref(), Some("MSG-1"));
        assert_eq!(row.text.as_deref(), Some("hello there"));
        assert!(!row.is_from_me);
        assert_eq!(row.handle.as_deref(), Some("+15551234567"));
        assert_eq!(row.chat_identifier.as_deref(), Some("+15551234567"));
        assert_eq!(row.date_created_unix_ms, JAN_1_2026_UNIX_MS);
        assert_eq!(row.attachment_count, 0);
    }

    #[test]
    fn cursor_excludes_everything_at_or_before_it() {
        let conn = fixture_db();
        for (rowid, offset_secs) in [(1, 0), (2, 60), (3, 120)] {
            conn.execute(
                "INSERT INTO message (ROWID, guid, is_from_me, date) VALUES (?1, ?2, 0, ?3)",
                params![
                    rowid,
                    format!("MSG-{rowid}"),
                    JAN_1_2026_COCOA_SECONDS + offset_secs
                ],
            )
            .unwrap();
        }

        let cursor = JAN_1_2026_UNIX_MS + 60_000; // exactly message 2's timestamp
        let rows = read_messages(&conn, Some(cursor), 10).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "only the strictly-later message should return"
        );
        assert_eq!(rows[0].guid.as_deref(), Some("MSG-3"));
    }

    #[test]
    fn limit_is_respected_and_order_is_ascending() {
        let conn = fixture_db();
        for (rowid, offset_secs) in [(1, 30), (2, 0), (3, 60)] {
            conn.execute(
                "INSERT INTO message (ROWID, guid, is_from_me, date) VALUES (?1, ?2, 0, ?3)",
                params![
                    rowid,
                    format!("MSG-{rowid}"),
                    JAN_1_2026_COCOA_SECONDS + offset_secs
                ],
            )
            .unwrap();
        }

        let rows = read_messages(&conn, None, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].guid.as_deref(), Some("MSG-2")); // earliest
        assert_eq!(rows[1].guid.as_deref(), Some("MSG-1"));
    }

    #[test]
    fn attachment_count_reflects_the_join_table() {
        let conn = fixture_db();
        conn.execute(
            "INSERT INTO message (ROWID, guid, is_from_me, date) VALUES (1, 'MSG-1', 1, ?1)",
            params![JAN_1_2026_COCOA_SECONDS],
        )
        .unwrap();
        conn.execute_batch(
            "
            INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (1, 100);
            INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (1, 101);
            ",
        )
        .unwrap();

        let rows = read_messages(&conn, None, 10).unwrap();
        assert_eq!(rows[0].attachment_count, 2);
    }

    #[test]
    fn a_message_with_no_chat_row_still_reads_with_none_for_chat_fields() {
        let conn = fixture_db();
        conn.execute(
            "INSERT INTO message (ROWID, guid, is_from_me, date) VALUES (1, 'MSG-1', 1, ?1)",
            params![JAN_1_2026_COCOA_SECONDS],
        )
        .unwrap();

        let rows = read_messages(&conn, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_identifier, None);
        assert_eq!(rows[0].chat_display_name, None);
    }
}
