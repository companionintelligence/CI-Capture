//! End-to-end coverage for the imessage lane's poll+ingest pass, against a
//! real (file-backed, not `:memory:`) `chat.db` and a real HTTP server —
//! mirrors `file_tracker::e2e`'s pattern (a hand-rolled raw-TCP mock server,
//! no mock-HTTP crate in this workspace) rather than inventing a new one.
//!
//! Unlike `file_tracker`'s e2e tests, `poll_once` is a single deterministic
//! async call rather than an event-stream-driven background lane, so these
//! tests call it directly and await the result — no polling/timeout dance
//! needed to wait for a background task to notice a filesystem change.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::config::AppConfig;

use super::{poll_once, reader};

/// One recorded request: (path, x-api-key header value, body).
type Recorded = (String, Option<String>, String);

/// Scripted response rule: when a request body contains `needle`, answer with
/// the next status from `statuses` (consumed in order). Same content-addressed
/// design as `file_tracker::e2e`'s version, for the same reason: which request
/// is "the one under test" is about what it says, not when it arrived.
type Rule = (&'static str, Vec<u16>);

fn spawn_mock_server(rules: Vec<Rule>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let rules = Arc::new(Mutex::new(rules));
    let rec = recorded.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();

            let mut content_length = 0usize;
            let mut api_key = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if let Some(v) = line
                    .split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("x-api-key"))
                    .map(|(_, v)| v.trim().to_string())
                {
                    api_key = Some(v);
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body).to_string();
            rec.lock().unwrap().push((path, api_key, body.clone()));

            let status = {
                let mut rules = rules.lock().unwrap();
                rules
                    .iter_mut()
                    .find(|(needle, statuses)| !statuses.is_empty() && body.contains(needle))
                    .map(|(_, statuses)| statuses.remove(0))
                    .unwrap_or(200)
            };
            let response = if status == 200 {
                let payload = r#"{"ingested":1,"duplicate":0,"rejected":[]}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    payload.len(),
                    payload
                )
            } else {
                let payload = serde_json::json!({
                    "statusCode": status,
                    "error": "Unprocessable Entity",
                    "message": format!("rejected request: {body}"),
                })
                .to_string();
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    payload.len(),
                    payload
                )
            };
            let _ = reader.into_inner().write_all(response.as_bytes());
        }
    });

    (origin, recorded)
}

fn lane_config(origin: &str) -> AppConfig {
    let mut config = AppConfig::default();
    config.server.portal_url = origin.to_string();
    // 64 lowercase hex → classified as an x-api-key credential, same fixture
    // value `file_tracker::e2e::lane_config` uses.
    config.server.applet_auth_token = Some("ab".repeat(32));
    config.imessage.enabled = true;
    config
}

/// A real (file-backed) `chat.db`, not `:memory:` — `poll_once` opens the
/// path fresh on every call the way the real lane does, so an in-memory
/// connection (which dies with its handle) can't stand in here. The schema is
/// `reader`'s shared fixture, so these tests and the unit tests exercise the
/// same column layout.
fn fixture_chat_db(path: &std::path::Path, rows: &[(i64, &str, &str, bool, i64)]) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(reader::FIXTURE_SCHEMA).unwrap();
    conn.execute_batch(
        "INSERT INTO handle (ROWID, id, service) VALUES (1, '+15551234567', 'iMessage');",
    )
    .unwrap();
    for (rowid, guid, text, is_from_me, date_raw) in rows {
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, is_from_me, service, date, handle_id)
             VALUES (?1, ?2, ?3, ?4, 'iMessage', ?5, 1)",
            params![rowid, guid, text, *is_from_me as i64, date_raw],
        )
        .unwrap();
    }
    conn
}

/// 2026-01-01T00:00:00Z in Cocoa **nanoseconds** — the unit every macOS since
/// 10.13 writes, verified independently in `reader::tests`. The offsets added
/// to it below are deliberately not round milliseconds: a cursor that floors
/// to the millisecond passes on aligned fixtures and fails on real data.
const COCOA_2026_NANOS: i64 = 788_918_400_000_000_000;
const ONE_MINUTE_NANOS: i64 = 60_000_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn a_message_reaches_the_server_with_its_text_and_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[(
            1,
            "MSG-1",
            "hello from the fixture",
            false,
            COCOA_2026_NANOS + 373_504,
        )],
    );
    drop(conn); // release the write lock before poll_once opens it read-only

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&db_path, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 1);
    assert_eq!(outcome.ingested, 1);
    assert!(outcome.advanced_to.is_some());

    let requests = recorded.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "expected exactly one POST, got {requests:?}"
    );
    let (path, api_key, body) = &requests[0];
    assert_eq!(path, "/v1/events");
    assert_eq!(api_key.as_deref(), Some(&"ab".repeat(32)[..]));
    assert!(body.contains("hello from the fixture"), "{body}");
    assert!(body.contains("imessage_message"), "{body}");
    assert!(body.contains("imessage:MSG-1"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn all_messages_in_the_page_are_sent_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[
            (1, "MSG-1", "first", false, COCOA_2026_NANOS),
            (
                2,
                "MSG-2",
                "second",
                true,
                COCOA_2026_NANOS + ONE_MINUTE_NANOS,
            ),
            (
                3,
                "MSG-3",
                "third",
                false,
                COCOA_2026_NANOS + 2 * ONE_MINUTE_NANOS,
            ),
        ],
    );
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&db_path, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 3);
    assert_eq!(outcome.ingested, 3);

    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let bodies: Vec<&String> = requests.iter().map(|(_, _, b)| b).collect();
    assert!(bodies[0].contains("imessage:MSG-1"));
    assert!(bodies[1].contains("imessage:MSG-2"));
    assert!(bodies[2].contains("imessage:MSG-3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mid_page_rejection_stops_the_cursor_there_not_at_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[
            (1, "MSG-1", "ok", false, COCOA_2026_NANOS),
            (
                2,
                "MSG-2",
                "this one gets rejected",
                false,
                COCOA_2026_NANOS + ONE_MINUTE_NANOS,
            ),
            (
                3,
                "MSG-3",
                "never reached",
                false,
                COCOA_2026_NANOS + 2 * ONE_MINUTE_NANOS,
            ),
        ],
    );
    drop(conn);

    // MSG-2's body is rejected with a permanent 400; the real lane's
    // `IngestError::from_status` allowlist treats 400 as permanent, which is
    // exactly the "stop this page here" branch `poll_once` takes.
    let (origin, recorded) = spawn_mock_server(vec![("MSG-2", vec![400])]);
    let config = lane_config(&origin);

    let outcome = poll_once(&db_path, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 3, "the whole page is read from chat.db");
    assert_eq!(outcome.ingested, 1, "only MSG-1 was accepted");
    // Cursor sits at MSG-1's row, not MSG-3's — so MSG-2 and MSG-3 are both
    // retried next call rather than MSG-2 being silently skipped.
    let expected_cursor = reader::read_messages(&Connection::open(&db_path).unwrap(), None, 1)
        .unwrap()
        .rows[0]
        .cursor();
    assert_eq!(outcome.advanced_to, Some(expected_cursor));

    let requests = recorded.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "MSG-3 must never be sent once MSG-2 stopped the page: {requests:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn passing_a_cursor_excludes_everything_at_or_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[
            (1, "MSG-1", "old", false, COCOA_2026_NANOS + 373_504),
            (
                2,
                "MSG-2",
                "new",
                false,
                COCOA_2026_NANOS + ONE_MINUTE_NANOS + 1,
            ),
        ],
    );
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let cursor = reader::read_messages(&Connection::open(&db_path).unwrap(), None, 1)
        .unwrap()
        .rows[0]
        .cursor();

    let outcome = poll_once(&db_path, &config, Uuid::nil(), Some(cursor), 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 1);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].2.contains("imessage:MSG-2"));
}

/// The whole-lane version of `reader`'s poll-twice test: an idle Mac used to
/// re-send its newest message on every tick, because the cursor round-tripped
/// through Unix milliseconds and landed back below the row that produced it.
/// This drives the real `poll_once`, so it also covers the cursor surviving the
/// `PollOutcome` → next-call hand-off the `run` loop performs.
#[tokio::test(flavor = "multi_thread")]
async fn polling_twice_with_the_returned_cursor_sends_nothing_the_second_time() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[
            (1, "MSG-1", "first", false, COCOA_2026_NANOS + 373_504),
            (
                2,
                "MSG-2",
                "newest",
                false,
                COCOA_2026_NANOS + ONE_MINUTE_NANOS + 999_999,
            ),
        ],
    );
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    let first = poll_once(&db_path, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();
    assert_eq!(first.ingested, 2);

    let second = poll_once(&db_path, &config, Uuid::nil(), first.advanced_to, 10)
        .await
        .unwrap();

    assert_eq!(
        second.fetched, 0,
        "the newest message came back a second time"
    );
    assert_eq!(second.ingested, 0);
    assert_eq!(second.advanced_to, None);
    assert_eq!(
        recorded.lock().unwrap().len(),
        2,
        "exactly the two messages, each sent once"
    );
}

/// The page-boundary half of the same fix, end to end: `message.date` is not
/// unique (real runs reach 56 rows sharing one timestamp), so a `LIMIT` cut
/// inside a run used to skip the remainder of it permanently.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_of_tied_timestamps_crosses_page_boundaries_intact() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[
            (1, "MSG-1", "a", false, COCOA_2026_NANOS),
            (2, "MSG-2", "b", false, COCOA_2026_NANOS),
            (3, "MSG-3", "c", false, COCOA_2026_NANOS),
            (4, "MSG-4", "d", false, COCOA_2026_NANOS),
            (5, "MSG-5", "e", false, COCOA_2026_NANOS),
        ],
    );
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    // Page size 2 against 5 tied rows: every boundary lands mid-run.
    let mut cursor = None;
    for _ in 0..10 {
        let outcome = poll_once(&db_path, &config, Uuid::nil(), cursor, 2)
            .await
            .unwrap();
        if outcome.fetched == 0 {
            break;
        }
        cursor = outcome.advanced_to;
    }

    let requests = recorded.lock().unwrap();
    let sent: Vec<String> = requests
        .iter()
        .map(|(_, _, body)| {
            let json: serde_json::Value = serde_json::from_str(body).unwrap();
            json["events"][0]["source_id"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        sent,
        [
            "imessage:MSG-1",
            "imessage:MSG-2",
            "imessage:MSG-3",
            "imessage:MSG-4",
            "imessage:MSG-5"
        ],
        "every tied row sent exactly once, in order"
    );
}

/// `WHERE date IS NOT NULL` does not exclude `date = 0`, and `is_from_me` is
/// nullable in the real schema. Both used to be fatal at the page level: the
/// zero row (newest by ROWID) typed the whole database as seconds, and the
/// NULL failed the entire `query_map`.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_date_and_a_null_is_from_me_do_not_take_the_page_down_with_them() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(reader::FIXTURE_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO message (ROWID, guid, text, is_from_me, date) VALUES (1, 'MSG-REAL', 'real', NULL, ?1)",
        params![COCOA_2026_NANOS + 373_504],
    )
    .unwrap();
    // Newest by ROWID — the row the old one-shot unit detection sampled.
    conn.execute(
        "INSERT INTO message (ROWID, guid, text, is_from_me, date) VALUES (2, 'MSG-ZERO', 'zero', 0, 0)",
        [],
    )
    .unwrap();
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&db_path, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();
    assert_eq!(outcome.fetched, 2);
    assert_eq!(outcome.ingested, 2);

    let requests = recorded.lock().unwrap();
    let occurred: Vec<String> = requests
        .iter()
        .map(|(_, _, body)| {
            let json: serde_json::Value = serde_json::from_str(body).unwrap();
            json["events"][0]["occurred_at"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    // The zero row is Cocoa's own epoch and sorts first; the real row keeps its
    // 2026 date instead of being re-typed into the year 79,000,000.
    assert_eq!(occurred[0], "2001-01-01T00:00:00.000Z");
    assert_eq!(occurred[1], "2026-01-01T00:00:00.000Z");
}

/// Regression test for a real bug found running this against a live
/// database: a message with `text IS NULL` (e.g. an `attributedBody`-only
/// row, see `reader`'s module doc) produced `description: null` in the
/// ingest JSON, and CI-Server's schema rejects an explicit `null` for an
/// optional string field — it wants the key omitted entirely. 35 of 50 real
/// messages failed this way in one batch before `CaptureEvent` got
/// `skip_serializing_if` on `description`.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_with_no_text_is_still_ingested_not_rejected_as_a_null_description() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(reader::FIXTURE_SCHEMA).unwrap();
    // `text` genuinely NULL, not an empty string — the shape a real
    // attributedBody-only row has.
    conn.execute(
        "INSERT INTO message (ROWID, guid, text, is_from_me, date) VALUES (1, 'MSG-NULL', NULL, 0, ?1)",
        params![COCOA_2026_NANOS],
    )
    .unwrap();
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&db_path, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(
        outcome.ingested, 1,
        "a NULL-text message must still be accepted"
    );

    let requests = recorded.lock().unwrap();
    let body = &requests[0].2;
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    let event = &json["events"][0];
    assert!(
        !event.as_object().unwrap().contains_key("description"),
        "description must be OMITTED, not sent as null, when there is no text: {body}"
    );
}

/// A page with nothing ingestible left in it has no acknowledged row to
/// advance the cursor off, so it advances past the drops itself — otherwise
/// the same unreadable rows come back on every poll for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_of_nothing_but_unreadable_rows_still_moves_the_cursor_past_them() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[
            (1, "MSG-BAD-1", "unreadable date", false, i64::MIN),
            (2, "MSG-BAD-2", "unreadable date", false, i64::MIN + 1),
            (3, "MSG-GOOD", "fine", false, COCOA_2026_NANOS + 373_504),
        ],
    );
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    // Page size 2: the first page is entirely drops.
    let first = poll_once(&db_path, &config, Uuid::nil(), None, 2)
        .await
        .unwrap();
    assert_eq!(first.fetched, 0);
    assert_eq!(first.skipped_invalid_timestamp, 2);
    assert!(
        first.advanced_to.is_some(),
        "a fully-dropped page must still hand back a cursor"
    );

    let second = poll_once(&db_path, &config, Uuid::nil(), first.advanced_to, 2)
        .await
        .unwrap();
    assert_eq!(
        second.ingested, 1,
        "the good row is reached on the next poll"
    );
    assert_eq!(recorded.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_database_is_a_no_op_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    drop(fixture_chat_db(&db_path, &[]));

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&db_path, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 0);
    assert_eq!(outcome.ingested, 0);
    assert_eq!(outcome.advanced_to, None);
    assert!(recorded.lock().unwrap().is_empty());
}
