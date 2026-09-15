//! End-to-end coverage for the knowledge lane's drain pass, against a real
//! (file-backed, not `:memory:`) fixture `knowledgeC.db` and a real HTTP
//! server — the raw-TCP mock from `imessage::e2e`, extended for what this
//! lane's transport does differently:
//!
//! - the default 200 body is computed from the request (`ingested` = the
//!   number of events posted), because [`super::drain_once`] sums acks;
//! - a rule can answer a scripted JSON body, so CI-Server's "HTTP 200 with
//!   `rejected[]`" materialize failure can be reproduced;
//! - a rule can be *sticky* — answered every time its needle matches — so
//!   a bisect that re-posts the same poison event in ever-smaller ranges
//!   keeps hitting the same 400;
//! - every request runs through a validator that 400s any `occurred_at` /
//!   `ended_at` / `observed_at` not ending in `Z`, the one thing CI-Server's
//!   `z.string().datetime()` actually enforces about timestamps.
//!
//! `drain_once` is a single deterministic async call, so each test awaits
//! it directly and inspects the cursor, the park file and the recorded
//! requests — no background task to wait on.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use uuid::Uuid;

use crate::config::AppConfig;

use super::facts::{STREAM_APP_USAGE, STREAM_BLUETOOTH, STREAM_NOTIFICATION};
use super::reader::{self, FixtureMetadata, FixtureRow};
use super::{
    Cursor, MAX_BACKOFF_TICKS, MAX_PARKS_PER_TICK, PollOutcome, drain_once, load_cursor,
    save_cursor,
};

/// One recorded request: (path, x-api-key header value, body).
pub(super) type Recorded = (String, Option<String>, String);

/// What a matched rule answers with.
#[derive(Debug, Clone)]
pub(super) enum Reply {
    /// An error status with a generic JSON error body.
    Status(u16),
    /// An exact status and body — for the 200-with-`rejected[]` shape.
    Json { status: u16, body: String },
}

/// Scripted response rule: when a request body contains `needle`, answer
/// with `reply`. Content-addressed like `file_tracker::e2e`'s: which
/// request is "the one under test" is about what it says, not when it
/// arrived. `sticky` rules survive being matched; the others are consumed.
#[derive(Debug, Clone)]
pub(super) struct Rule {
    needle: &'static str,
    reply: Reply,
    sticky: bool,
}

pub(super) fn status_rule(needle: &'static str, status: u16, sticky: bool) -> Rule {
    Rule {
        needle,
        reply: Reply::Status(status),
        sticky,
    }
}

/// The 200-with-`rejected[]` body CI-Server answers when its materialize
/// step fails: `ingested` is 0 and every index is listed with one reason.
fn rejected_body(count: usize) -> String {
    let rejected: Vec<serde_json::Value> = (0..count)
        .map(|index| serde_json::json!({ "index": index, "reason": "materialize failed" }))
        .collect();
    serde_json::json!({ "ingested": 0, "duplicate": 0, "rejected": rejected }).to_string()
}

/// True if every timestamp field on every event ends in `Z` — CI-Server's
/// `z.string().datetime()` accepts nothing else.
fn timestamps_are_wire_utc(body: &str) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(events) = json["events"].as_array() else {
        return false;
    };
    events.iter().all(|event| {
        ["occurred_at", "ended_at", "observed_at"]
            .iter()
            .all(|key| match event.get(*key) {
                None => true,
                Some(serde_json::Value::String(s)) => s.ends_with('Z'),
                Some(_) => false,
            })
    })
}

/// The validator is scaffolding, not the product, but a validator that
/// quietly accepts everything would turn the module-doc promise above into
/// a lie without any e2e test noticing — every 400 the drain tests see
/// comes from a scripted rule, never from this branch. Pin its shape here
/// so a future "why is this 400-ing" loosening cannot go unseen.
#[test]
fn the_mock_validator_rejects_anything_but_a_trailing_z() {
    let body = |ts: serde_json::Value| {
        serde_json::json!({ "events": [{ "occurred_at": ts }] }).to_string()
    };
    assert!(timestamps_are_wire_utc(&body(
        "2026-09-01T00:00:00.000Z".into()
    )));
    assert!(!timestamps_are_wire_utc(&body(
        "2026-09-01T00:00:00+00:00".into()
    )));
    assert!(!timestamps_are_wire_utc(&body(
        "2026-09-01T00:00:00".into()
    )));
    assert!(!timestamps_are_wire_utc(&body(serde_json::json!(
        1_756_684_800_000_i64
    ))));
}

fn event_count(body: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json["events"].as_array().map(Vec::len))
        .unwrap_or(0)
}

pub(super) fn spawn_mock_server(rules: Vec<Rule>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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

            // The validator runs first: a non-Z timestamp is a 400 whatever
            // the script says, exactly as the real schema would answer.
            let reply = if !timestamps_are_wire_utc(&body) {
                Reply::Status(400)
            } else {
                let mut rules = rules.lock().unwrap();
                match rules.iter().position(|rule| body.contains(rule.needle)) {
                    Some(i) if rules[i].sticky => rules[i].reply.clone(),
                    Some(i) => rules.remove(i).reply,
                    None => Reply::Json {
                        status: 200,
                        body: serde_json::json!({
                            "ingested": event_count(&body),
                            "duplicate": 0,
                            "rejected": [],
                        })
                        .to_string(),
                    },
                }
            };
            let (status, payload) = match reply {
                Reply::Json { status, body } => (status, body),
                Reply::Status(status) => (
                    status,
                    serde_json::json!({
                        "statusCode": status,
                        "error": "scripted",
                        "message": "rejected by the mock",
                    })
                    .to_string(),
                ),
            };
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = reader.into_inner().write_all(response.as_bytes());
        }
    });

    (origin, recorded)
}

pub(super) fn lane_config(origin: &str) -> AppConfig {
    let mut config = AppConfig::default();
    config.server.portal_url = origin.to_string();
    // 64 lowercase hex → classified as an x-api-key credential, same fixture
    // value `file_tracker::e2e::lane_config` uses.
    config.server.applet_auth_token = Some("ab".repeat(32));
    config.knowledge.enabled = true;
    // Fixture rows are dated 2026-09-01; a 30-day window measured from the
    // real clock would exclude them on any later day, so ship everything.
    config.knowledge.backfill_days = 0;
    config
}

/// A fresh file-backed fixture store; `drain_once` opens the path fresh on
/// every page the way the real lane does, so `:memory:` can't stand in.
fn fixture_db(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(reader::FIXTURE_SCHEMA).unwrap();
    reader::insert_store_uuid(&conn, "STORE-A");
    conn
}

/// 2026-09-01T00:00:00Z in Cocoa seconds.
const COCOA_2026_09_01: f64 = 809_913_600.0;

/// An `/app/usage` row `pk` with a UUID `U<pk>`, `minutes` long — well over
/// the 30 s floor, so it ships.
fn usage(pk: i64, minutes: f64) -> FixtureRow {
    let start = COCOA_2026_09_01 + (pk as f64) * 3600.0;
    FixtureRow::new(STREAM_APP_USAGE, start, start + minutes * 60.0)
        .pk(pk)
        .uuid(&format!("U{pk}"))
        .value_string("com.example.Alpha")
}

/// Everything one test needs on disk: the store, the cursor file and the
/// park file share a tempdir that is removed on drop.
pub(super) struct Lane {
    _dir: tempfile::TempDir,
    pub(super) db_path: PathBuf,
    pub(super) cursor_path: PathBuf,
    pub(super) rejected_path: PathBuf,
}

impl Lane {
    pub(super) fn new(rows: &[FixtureRow]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("knowledgeC.db");
        let conn = fixture_db(&db_path);
        for row in rows {
            reader::insert_row(&conn, row);
        }
        drop(conn); // release the write lock before drain_once opens it read-only
        Self {
            cursor_path: dir.path().join("knowledge-cursor.json"),
            rejected_path: dir.path().join("knowledge-rejected.jsonl"),
            db_path,
            _dir: dir,
        }
    }

    /// `drain_once` with the plan's small test sizes: two-row pages, three-
    /// event chunks, and a generous budget unless a test says otherwise.
    async fn drain(
        &self,
        config: &AppConfig,
        cursor: &mut Cursor,
        page_limit: u32,
        batch_size: usize,
        budget: Duration,
    ) -> PollOutcome {
        drain_once(
            &self.db_path,
            config,
            Uuid::nil(),
            cursor,
            &self.cursor_path,
            &self.rejected_path,
            page_limit,
            batch_size,
            budget,
        )
        .await
        .unwrap()
    }

    fn parked_lines(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.rejected_path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// `source_id`s in one recorded request body, in order.
pub(super) fn source_ids(body: &str) -> Vec<String> {
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    json["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["source_id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backlog_ships_in_batches_to_v1_events_with_the_api_key_and_acks_sum() {
    let lane = Lane::new(&[
        usage(1, 5.0),
        usage(2, 5.0),
        usage(3, 5.0),
        usage(4, 5.0),
        usage(5, 5.0),
    ]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane
        .drain(&config, &mut cursor, 2, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.rows_returned, 5);
    assert_eq!(outcome.fetched, 5);
    assert_eq!(outcome.ingested, 5, "acks are summed across batches");
    assert_eq!(outcome.duplicate, 0);
    assert_eq!(outcome.parked, 0);
    assert_eq!(outcome.advanced_to, Some(5));
    assert_eq!(cursor.last_pk, Some(5));
    assert_eq!(cursor.store_uuid.as_deref(), Some("STORE-A"));
    assert_eq!(
        load_cursor(&lane.cursor_path).last_pk,
        Some(5),
        "cursor persisted"
    );

    let requests = recorded.lock().unwrap();
    // Pages of 2 → three POSTs (2, 2, 1); chunk size 3 never splits a page.
    assert_eq!(requests.len(), 3, "{requests:?}");
    for (path, api_key, _) in requests.iter() {
        assert_eq!(path, "/v1/events");
        assert_eq!(api_key.as_deref(), Some(&"ab".repeat(32)[..]));
    }
    let sent: Vec<String> = requests
        .iter()
        .flat_map(|(_, _, b)| source_ids(b))
        .collect();
    assert_eq!(
        sent,
        [
            "knowledge:U1",
            "knowledge:U2",
            "knowledge:U3",
            "knowledge:U4",
            "knowledge:U5"
        ]
    );
    let body = &requests[0].2;
    assert!(body.contains("knowledge_app_usage"), "{body}");
    assert!(body.contains("Used Alpha for 5 min"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transient_503_leaves_the_cursor_at_the_last_acked_chunk() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0), usage(3, 5.0), usage(4, 5.0)]);
    // The second page (U3, U4) is refused with a 503 — an outage, not a
    // verdict on the events.
    let (origin, recorded) = spawn_mock_server(vec![status_rule("knowledge:U3", 503, false)]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane
        .drain(&config, &mut cursor, 2, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.ingested, 2, "only the first page was accepted");
    assert_eq!(outcome.parked, 0, "a 503 never parks");
    assert_eq!(
        cursor.last_pk,
        Some(2),
        "cursor sits at the last acked chunk"
    );
    assert_eq!(load_cursor(&lane.cursor_path).last_pk, Some(2));
    assert_eq!(
        cursor.stalled_head.as_ref().map(|(id, _)| id.as_str()),
        Some("knowledge:U3"),
        "the head that stalled the lane is remembered for the 24 h valve"
    );
    assert_eq!(
        recorded.lock().unwrap().len(),
        2,
        "U3/U4 are not retried within the tick"
    );
    assert!(lane.parked_lines().is_empty());

    // The outage clears: the next tick resumes from U3 and a clean tick
    // forgets the stall.
    let second = lane
        .drain(&config, &mut cursor, 2, 3, Duration::from_secs(30))
        .await;
    assert_eq!(second.ingested, 2);
    assert_eq!(cursor.last_pk, Some(4));
    assert_eq!(cursor.stalled_head, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sticky_400_on_one_event_bisects_parks_exactly_that_event_and_ships_the_rest() {
    let lane = Lane::new(&[
        usage(1, 5.0),
        usage(2, 5.0),
        usage(3, 5.0),
        usage(4, 5.0),
        usage(5, 5.0),
        usage(6, 5.0),
    ]);
    // Every request that carries U3 is a 400 — the real server's
    // all-or-nothing validation, which the lane must bisect down to U3.
    let (origin, recorded) = spawn_mock_server(vec![status_rule("knowledge:U3", 400, true)]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    // One page of six, chunks of four: (U1..U4) fails → (U1,U2) ok →
    // (U3,U4) fails → (U3) parked → (U4) ok → (U5,U6) ok.
    let outcome = lane
        .drain(&config, &mut cursor, 10, 4, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.parked, 1);
    assert_eq!(
        outcome.ingested, 5,
        "everything but the poison event shipped"
    );
    assert_eq!(cursor.last_pk, Some(6));
    assert_eq!(
        cursor.backoff_ticks, 0,
        "one park is a per-event problem, not an outage"
    );

    let parked = lane.parked_lines();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0]["source_id"], "knowledge:U3");
    assert_eq!(parked[0]["pk"], 3);
    assert_eq!(parked[0]["kind"], "knowledge_app_usage");
    assert_eq!(parked[0]["stream"], STREAM_APP_USAGE);
    assert!(parked[0]["reason"].as_str().unwrap().contains("400"));
    assert!(parked[0]["rejected_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(parked[0]["event"]["source_id"], "knowledge:U3");

    let requests = recorded.lock().unwrap();
    let batches: Vec<Vec<String>> = requests.iter().map(|(_, _, b)| source_ids(b)).collect();
    assert_eq!(
        batches,
        vec![
            vec![
                "knowledge:U1",
                "knowledge:U2",
                "knowledge:U3",
                "knowledge:U4"
            ],
            vec!["knowledge:U1", "knowledge:U2"],
            vec!["knowledge:U3", "knowledge:U4"],
            vec!["knowledge:U3"],
            vec!["knowledge:U4"],
            vec!["knowledge:U5", "knowledge:U6"],
        ],
        "bisect order"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_200_with_rejected_entries_is_retried_next_tick_with_the_cursor_untouched() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0)]);
    let (origin, recorded) = spawn_mock_server(vec![Rule {
        needle: "knowledge:U1",
        reply: Reply::Json {
            status: 200,
            body: rejected_body(2),
        },
        sticky: false,
    }]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let first = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(first.ingested, 0);
    assert_eq!(
        first.parked, 0,
        "rejected[] is a re-send signal, never a park"
    );
    assert_eq!(
        cursor.last_pk,
        Some(0),
        "cursor stays at the first-run floor, nothing was acked"
    );
    assert!(lane.parked_lines().is_empty());

    let second = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(
        second.ingested, 2,
        "the identical batch succeeds on re-send"
    );
    assert_eq!(cursor.last_pk, Some(2));
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(source_ids(&requests[0].2), source_ids(&requests[1].2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_of_only_zero_duration_rows_advances_to_last_seen_pk_with_zero_posts() {
    // Sub-floor blips: 73% of real `/app/usage` rows look like this.
    let lane = Lane::new(&[usage(1, 0.0), usage(2, 0.0), usage(3, 0.0)]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane
        .drain(&config, &mut cursor, 2, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.rows_returned, 3);
    assert_eq!(outcome.skipped_short, 3);
    assert_eq!(outcome.ingested, 0);
    assert_eq!(
        outcome.advanced_to,
        Some(3),
        "the cursor must step past rows that will never ship"
    );
    assert_eq!(load_cursor(&lane.cursor_path).last_pk, Some(3));
    assert!(recorded.lock().unwrap().is_empty(), "nothing to post");

    // And the next tick reads nothing — the blips are not re-read forever.
    let second = lane
        .drain(&config, &mut cursor, 2, 3, Duration::from_secs(30))
        .await;
    assert_eq!(second.rows_returned, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn trailing_skipped_rows_are_covered_by_the_cursor() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 0.0), usage(3, 0.0)]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.ingested, 1);
    assert_eq!(outcome.skipped_short, 2);
    assert_eq!(
        cursor.last_pk,
        Some(3),
        "the cursor covers the skipped rows after the last acked event"
    );
    assert_eq!(recorded.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_budget_stops_after_the_first_acked_batch() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0), usage(3, 5.0), usage(4, 5.0)]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane.drain(&config, &mut cursor, 2, 3, Duration::ZERO).await;

    assert_eq!(
        outcome.ingested, 2,
        "one page, one chunk, then the budget ends the tick"
    );
    assert_eq!(cursor.last_pk, Some(2));
    assert_eq!(recorded.lock().unwrap().len(), 1);

    // The rest is still there next tick.
    let second = lane.drain(&config, &mut cursor, 2, 3, Duration::ZERO).await;
    assert_eq!(second.ingested, 2);
    assert_eq!(cursor.last_pk, Some(4));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_from_a_different_store_file_resets_to_the_first_run_floor() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0)]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    // A cursor left behind by a previous store file, far past anything the
    // new file holds. Without the UUID check the lane would wait forever
    // for Z_PK 9001.
    let mut cursor = Cursor {
        store_uuid: Some("STORE-OLD".to_owned()),
        last_pk: Some(9_000),
        ..Cursor::default()
    };
    save_cursor(&lane.cursor_path, &cursor).unwrap();

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(
        outcome.ingested, 2,
        "both rows of the new store were shipped"
    );
    assert_eq!(cursor.store_uuid.as_deref(), Some("STORE-A"));
    assert_eq!(cursor.last_pk, Some(2));
    assert_eq!(recorded.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_above_max_pk_in_the_same_store_resets_too() {
    let lane = Lane::new(&[usage(1, 5.0)]);
    let (origin, _recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(500),
        ..Cursor::default()
    };

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.ingested, 1);
    assert_eq!(cursor.last_pk, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_garbage_cursor_file_restarts_the_lane_from_the_floor() {
    let lane = Lane::new(&[usage(1, 5.0)]);
    std::fs::write(&lane.cursor_path, b"{not json").unwrap();
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = load_cursor(&lane.cursor_path);
    assert_eq!(cursor, Cursor::default());

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.ingested, 1);
    assert_eq!(recorded.lock().unwrap().len(), 1);
    assert_eq!(
        load_cursor(&lane.cursor_path).last_pk,
        Some(1),
        "rewritten as valid JSON"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_store_is_a_no_op_not_an_error() {
    let lane = Lane::new(&[]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.rows_returned, 0);
    assert_eq!(outcome.ingested, 0);
    assert_eq!(outcome.advanced_to, None);
    assert_eq!(
        cursor.last_pk,
        Some(0),
        "first run recorded even on an empty store"
    );
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn hitting_max_parks_per_tick_stops_the_tick_and_sets_backoff() {
    let rows: Vec<FixtureRow> = (1..=(MAX_PARKS_PER_TICK as i64 + 2))
        .map(|pk| usage(pk, 5.0))
        .collect();
    let lane = Lane::new(&rows);
    // A systemic rejection: every request with an app-usage event is a 400.
    let (origin, _recorded) =
        spawn_mock_server(vec![status_rule("knowledge_app_usage", 400, true)]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.parked, MAX_PARKS_PER_TICK);
    assert_eq!(outcome.ingested, 0);
    assert_eq!(
        cursor.last_pk,
        Some(MAX_PARKS_PER_TICK as i64),
        "cursor stops at the last parked event; the rest wait for the backoff"
    );
    assert_eq!(cursor.backoff_ticks, 1, "first systemic tick: 0*2+1");
    assert_eq!(cursor.backoff_level, 1);
    assert_eq!(lane.parked_lines().len(), MAX_PARKS_PER_TICK);

    // The next tick parks the two remaining rows — under the cap, so it
    // counts as clean and the backoff clears.
    let second = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(second.parked, 2, "the two remaining rows park");
    assert_eq!(
        cursor.backoff_ticks, 0,
        "under the cap this tick was clean, so backoff clears"
    );
    assert_eq!(cursor.backoff_level, 0);
}

/// `run` only reaches `drain_once` once the countdown is back at zero, so
/// the doubling has to come from a level the countdown does not erase:
/// each further systemic tick issues `2n+1` from the *last issued* backoff
/// (1, 3, 7, …, capped at 64), not from the always-zero countdown — the
/// difference between a park file that grows by five lines every other
/// tick forever and one that grows ever more slowly.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_systemic_rejection_doubles_the_backoff_up_to_the_cap() {
    let rows: Vec<FixtureRow> = (1..=(2 * MAX_PARKS_PER_TICK as i64 + 1))
        .map(|pk| usage(pk, 5.0))
        .collect();
    let lane = Lane::new(&rows);
    let (origin, _recorded) =
        spawn_mock_server(vec![status_rule("knowledge_app_usage", 400, true)]);
    let config = lane_config(&origin);
    // As `run` leaves the cursor after counting one backoff tick down.
    let mut cursor = Cursor {
        backoff_level: 1,
        ..Cursor::default()
    };

    lane.drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(cursor.backoff_level, 3, "1*2+1");
    assert_eq!(cursor.backoff_ticks, 3);

    cursor.backoff_ticks = 0; // `run` counted down again
    lane.drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(cursor.backoff_level, 7, "3*2+1");
    assert_eq!(cursor.backoff_ticks, 7);

    // A level already near the ceiling stops there.
    let rows: Vec<FixtureRow> = (1..=(MAX_PARKS_PER_TICK as i64 + 1))
        .map(|pk| usage(pk, 5.0))
        .collect();
    let lane = Lane::new(&rows);
    let mut cursor = Cursor {
        backoff_level: 40,
        backoff_ticks: 0,
        ..Cursor::default()
    };
    lane.drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(cursor.backoff_level, MAX_BACKOFF_TICKS);
    assert_eq!(cursor.backoff_ticks, MAX_BACKOFF_TICKS);

    // A clean tick (the server accepts again) clears both.
    let (origin, _recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let lane = Lane::new(&[usage(1, 5.0)]);
    let mut cursor = Cursor {
        backoff_level: 7,
        backoff_ticks: 0,
        ..Cursor::default()
    };
    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(outcome.ingested, 1);
    assert_eq!(cursor.backoff_level, 0);
    assert_eq!(cursor.backoff_ticks, 0);
}

/// The head-of-line valve: an event whose batch the server has been
/// refusing *transiently* for longer than 24 h gets a fresh attempt on its
/// own and, only because it still fails alone, is parked — so a server
/// that 500s on one specific event cannot wedge the lane.
#[tokio::test(flavor = "multi_thread")]
async fn a_head_of_line_event_stalled_for_a_day_is_parked_and_the_lane_moves_on() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0), usage(3, 5.0)]);
    // U1 poisons any batch it is in with a 500; batches without it succeed.
    let (origin, recorded) = spawn_mock_server(vec![status_rule("knowledge:U1", 500, true)]);
    let config = lane_config(&origin);
    let day_and_a_bit_ago = chrono::Utc::now().timestamp_millis() - 25 * 3600 * 1000;
    let mut cursor = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(0),
        stalled_head: Some(("knowledge:U1".to_owned(), day_and_a_bit_ago)),
        ..Cursor::default()
    };

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.parked, 1);
    assert_eq!(
        outcome.ingested, 2,
        "U2 and U3 ship once U1 is out of the way"
    );
    assert_eq!(cursor.last_pk, Some(3));
    assert_eq!(cursor.stalled_head, None);
    let parked = lane.parked_lines();
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0]["source_id"], "knowledge:U1");
    assert!(
        parked[0]["reason"]
            .as_str()
            .unwrap()
            .contains("head-of-line")
    );
    // The stalled head gets a fresh attempt first; only because it still
    // 500s is it parked. [U1,U2,U3] → 500, [U1] → 500 → park, [U2,U3] → 200.
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(
        requests
            .iter()
            .filter(|(_, _, b)| b.contains("knowledge:U1"))
            .count(),
        2
    );
    assert_eq!(
        source_ids(&requests[1].2),
        vec!["knowledge:U1"],
        "posted alone before being parked"
    );
}

/// A stall is about the *chunk* the server refused, not its first event:
/// when the poison sits behind the head, the head ships and the event that
/// fails on its own is the one parked — in this tick, not one halving per
/// day with an innocent event parked at each step.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_chunk_parks_the_event_that_fails_alone_not_the_head() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0), usage(3, 5.0)]);
    // U2 poisons any batch it is in; U1 and U3 are fine.
    let (origin, recorded) = spawn_mock_server(vec![status_rule("knowledge:U2", 500, true)]);
    let config = lane_config(&origin);
    let day_and_a_bit_ago = chrono::Utc::now().timestamp_millis() - 25 * 3600 * 1000;
    let mut cursor = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(0),
        stalled_head: Some(("knowledge:U1".to_owned(), day_and_a_bit_ago)),
        ..Cursor::default()
    };

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.parked, 1);
    assert_eq!(outcome.ingested, 2, "U1 and U3");
    assert_eq!(cursor.last_pk, Some(3));
    assert_eq!(cursor.stalled_head, None);
    let parked = lane.parked_lines();
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0]["source_id"], "knowledge:U2");
    // [U1,U2,U3] → 500, [U1] → 200, [U2,U3] → 500, [U2] → 500 → park, [U3] → 200.
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 5, "{requests:?}");
    assert_eq!(source_ids(&requests[3].2), vec!["knowledge:U2"]);
}

/// A 200-with-`rejected[]` that repeats is the materialize step tripping
/// over one row of the payload (a column constraint, say) rather than over
/// the server's own state — but the body looks identical either way, so
/// the client keeps calling it transient and the stall valve is what bounds
/// it. The first tick stalls on the chunk's *head* (a healthy event); once
/// the stall has matured the chunk is bisected on that same `rejected[]`
/// answer, the healthy head ships, and the one event refused on its own is
/// the only one parked — in that one tick, not one halving per day.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeating_rejected_list_on_one_event_stalls_a_day_then_parks_only_that_event() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0), usage(3, 5.0)]);
    // U2 poisons any batch it is in with the materialize-failure shape; the
    // mock's index list is the full batch's whatever the request, which is
    // all the client reads from it (counts for the log line).
    let (origin, recorded) = spawn_mock_server(vec![Rule {
        needle: "knowledge:U2",
        reply: Reply::Json {
            status: 200,
            body: rejected_body(3),
        },
        sticky: true,
    }]);
    let config = lane_config(&origin);
    let mut cursor = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(0),
        ..Cursor::default()
    };

    // Tick 1: the whole chunk is refused; nothing is acked, nothing is
    // parked, and the chunk's head is the one remembered as stalled.
    let first = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(first.ingested, 0);
    assert_eq!(first.parked, 0, "one rejected[] is a re-send, never a park");
    assert_eq!(cursor.last_pk, Some(0));
    assert_eq!(
        cursor.stalled_head.as_ref().map(|(id, _)| id.as_str()),
        Some("knowledge:U1"),
        "the head of the refused chunk, not the poison, is what the valve tracks"
    );
    assert!(lane.parked_lines().is_empty());

    // A day later, still refused: the valve fires.
    let day_and_a_bit_ago = chrono::Utc::now().timestamp_millis() - 25 * 3600 * 1000;
    cursor.stalled_head = Some(("knowledge:U1".to_owned(), day_and_a_bit_ago));

    let second = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(second.parked, 1);
    assert_eq!(second.ingested, 2, "U1 and U3 ship in the same tick");
    assert_eq!(cursor.last_pk, Some(3));
    assert_eq!(cursor.stalled_head, None);
    let parked = lane.parked_lines();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0]["source_id"], "knowledge:U2");
    assert!(
        parked[0]["reason"]
            .as_str()
            .unwrap()
            .contains("head-of-line")
    );
    // Tick 1: [U1,U2,U3] → rejected[].
    // Tick 2: [U1,U2,U3] → rejected[], [U1] → 200, [U2,U3] → rejected[],
    //         [U2] → rejected[] → park, [U3] → 200.
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 6, "{requests:?}");
    assert_eq!(
        source_ids(&requests[4].2),
        vec!["knowledge:U2"],
        "posted alone before being parked"
    );
    assert!(
        requests
            .iter()
            .filter(|(_, _, b)| b.contains("knowledge:U1"))
            .all(|(_, _, b)| !b.contains("knowledge:U2") || source_ids(b).len() == 3),
        "once the split starts U1 is never re-batched with the poison: {requests:?}"
    );
}

/// An old stall whose server is healthy again ships; the valve only parks
/// what is *still* failing (file_tracker's PARK_AFTER precedent). A Mac
/// that slept through the weekend after one refused batch on Friday must
/// not park Friday's event on Monday.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_stall_against_a_recovered_server_ships_instead_of_parking() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0), usage(3, 5.0)]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let day_and_a_bit_ago = chrono::Utc::now().timestamp_millis() - 25 * 3600 * 1000;
    let mut cursor = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(0),
        stalled_head: Some(("knowledge:U1".to_owned(), day_and_a_bit_ago)),
        ..Cursor::default()
    };

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.parked, 0);
    assert_eq!(outcome.ingested, 3);
    assert_eq!(cursor.last_pk, Some(3));
    assert_eq!(cursor.stalled_head, None);
    assert!(lane.parked_lines().is_empty());
    assert_eq!(recorded.lock().unwrap().len(), 1);
}

/// A server that cannot be reached at all says nothing about the event at
/// the head: an old stall stays exactly as it was (neither parked nor
/// restarted), so a day-long outage never matures into a park.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_stalled_head_with_the_server_unreachable_parks_nothing() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0)]);
    // Bind, read the port, drop: nothing listens there any more.
    let origin = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let config = lane_config(&origin);
    let day_and_a_bit_ago = chrono::Utc::now().timestamp_millis() - 25 * 3600 * 1000;
    let stalled = Some(("knowledge:U1".to_owned(), day_and_a_bit_ago));
    let mut cursor = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(0),
        stalled_head: stalled.clone(),
        ..Cursor::default()
    };

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.parked, 0);
    assert_eq!(outcome.ingested, 0);
    assert_eq!(cursor.last_pk, Some(0));
    assert_eq!(
        cursor.stalled_head, stalled,
        "untouched by a transport failure"
    );
    assert!(lane.parked_lines().is_empty());

    // Nor does a transport failure *start* the clock on a fresh cursor.
    let mut fresh = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(0),
        ..Cursor::default()
    };
    lane.drain(&config, &mut fresh, 10, 3, Duration::from_secs(30))
        .await;
    assert_eq!(fresh.stalled_head, None);
}

/// A stall younger than the valve is still just an outage.
#[tokio::test(flavor = "multi_thread")]
async fn a_recent_stall_keeps_retrying_instead_of_parking() {
    let lane = Lane::new(&[usage(1, 5.0), usage(2, 5.0)]);
    let (origin, _recorded) = spawn_mock_server(vec![status_rule("knowledge:U1", 500, true)]);
    let config = lane_config(&origin);
    let an_hour_ago = chrono::Utc::now().timestamp_millis() - 3600 * 1000;
    let mut cursor = Cursor {
        store_uuid: Some("STORE-A".to_owned()),
        last_pk: Some(0),
        stalled_head: Some(("knowledge:U1".to_owned(), an_hour_ago)),
        ..Cursor::default()
    };

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(outcome.parked, 0);
    assert_eq!(outcome.ingested, 0);
    assert_eq!(cursor.last_pk, Some(0));
    assert_eq!(
        cursor.stalled_head,
        Some(("knowledge:U1".to_owned(), an_hour_ago)),
        "the original stall time is kept so the valve measures from the first failure"
    );
    assert!(lane.parked_lines().is_empty());
}

/// Mixed streams through the real transport: a point-event notification
/// (no `ended_at`), a Bluetooth span and an app session all land in one
/// batch, and every timestamp on the wire passes the `Z` validator — the
/// mock would 400 the batch otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_streams_ship_with_wire_utc_timestamps_and_point_events_carry_no_ended_at() {
    let t = COCOA_2026_09_01 + 7_200.0;
    let lane = Lane::new(&[
        usage(1, 5.0),
        FixtureRow::new(STREAM_NOTIFICATION, t, t)
            .pk(2)
            .uuid("N2")
            .metadata(FixtureMetadata {
                notification_bundle_id: Some("com.example.Chat".to_owned()),
                ..Default::default()
            }),
        FixtureRow::new(STREAM_BLUETOOTH, t, t + 5_400.0)
            .pk(3)
            .uuid("B3")
            .value_integer(1)
            .metadata(FixtureMetadata {
                bt_name: Some("AirPods Pro".to_owned()),
                bt_is_apple_audio: Some(1),
                ..Default::default()
            }),
    ]);
    let (origin, recorded) = spawn_mock_server(vec![]);
    let config = lane_config(&origin);
    let mut cursor = Cursor::default();

    let outcome = lane
        .drain(&config, &mut cursor, 10, 3, Duration::from_secs(30))
        .await;

    assert_eq!(
        outcome.ingested, 3,
        "the validator accepted every timestamp"
    );
    let requests = recorded.lock().unwrap();
    let json: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
    let events = json["events"].as_array().unwrap();
    assert_eq!(events[0]["kind"], "knowledge_app_usage");
    assert_eq!(events[1]["kind"], "knowledge_notification");
    assert!(
        events[1].get("ended_at").is_none(),
        "a zero-length point event omits ended_at: {}",
        events[1]
    );
    assert_eq!(events[2]["kind"], "knowledge_bluetooth");
    assert_eq!(events[2]["title"], "AirPods Pro connected for 1 h 30 min");
    assert_eq!(events[2]["occurred_at"], "2026-09-01T02:00:00.000Z");
    assert_eq!(events[2]["ended_at"], "2026-09-01T03:30:00.000Z");
    assert_eq!(events[2]["payload"]["duration_secs"], 5_400);
}
