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
/// connection (which dies with its handle) can't stand in here.
fn fixture_chat_db(path: &std::path::Path, rows: &[(i64, &str, &str, bool, i64)]) -> Connection {
    let conn = Connection::open(path).unwrap();
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
        INSERT INTO handle (ROWID, id) VALUES (1, '+15551234567');
        ",
    )
    .unwrap();
    for (rowid, guid, text, is_from_me, date_cocoa_secs) in rows {
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, is_from_me, service, date, handle_id)
             VALUES (?1, ?2, ?3, ?4, 'iMessage', ?5, 1)",
            params![rowid, guid, text, *is_from_me as i64, date_cocoa_secs],
        )
        .unwrap();
    }
    conn
}

const COCOA_2026: i64 = 788_918_400; // 2026-01-01T00:00:00Z, verified independently in reader::tests

#[tokio::test(flavor = "multi_thread")]
async fn a_message_reaches_the_server_with_its_text_and_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chat.db");
    let conn = fixture_chat_db(
        &db_path,
        &[(1, "MSG-1", "hello from the fixture", false, COCOA_2026)],
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
            (1, "MSG-1", "first", false, COCOA_2026),
            (2, "MSG-2", "second", true, COCOA_2026 + 60),
            (3, "MSG-3", "third", false, COCOA_2026 + 120),
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
            (1, "MSG-1", "ok", false, COCOA_2026),
            (2, "MSG-2", "this one gets rejected", false, COCOA_2026 + 60),
            (3, "MSG-3", "never reached", false, COCOA_2026 + 120),
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
    // Cursor sits at MSG-1's timestamp, not MSG-3's — so MSG-2 and MSG-3 are
    // both retried next call rather than MSG-2 being silently skipped.
    let expected_cursor = reader::read_messages(&Connection::open(&db_path).unwrap(), None, 1)
        .unwrap()[0]
        .date_created_unix_ms;
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
            (1, "MSG-1", "old", false, COCOA_2026),
            (2, "MSG-2", "new", false, COCOA_2026 + 60),
        ],
    );
    drop(conn);

    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let cursor_unix_ms = reader::read_messages(&Connection::open(&db_path).unwrap(), None, 1)
        .unwrap()[0]
        .date_created_unix_ms;

    let outcome = poll_once(&db_path, &config, Uuid::nil(), Some(cursor_unix_ms), 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 1);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].2.contains("imessage:MSG-2"));
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
    // `text` genuinely NULL, not an empty string — the shape a real
    // attributedBody-only row has.
    conn.execute(
        "INSERT INTO message (ROWID, guid, text, is_from_me, date) VALUES (1, 'MSG-NULL', NULL, 0, ?1)",
        params![COCOA_2026],
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
