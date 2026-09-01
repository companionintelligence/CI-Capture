//! End-to-end coverage for the bluebubbles lane's poll+ingest pass, against a
//! real HTTP BlueBubbles server and a real HTTP CI-Server — both hand-rolled
//! raw-TCP mocks, the pattern `imessage::e2e` and `file_tracker::e2e` already
//! use (there is no mock-HTTP crate in this workspace).
//!
//! This lane needs *two* mock servers where the imessage lane needs one: its
//! input arrives over the network rather than out of a file, and the two ends
//! are what a whole class of this lane's bugs live between — a request body
//! that says something different from what the client thinks it says, or a
//! server failure whose error text carries the password back into a log line.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::config::AppConfig;

use super::client::{BlueBubblesClient, BlueBubblesClientConfig};
use super::poll_once;

/// The BlueBubbles server password these tests configure. Every assertion
/// about redaction is really an assertion that this string does not appear.
const SERVER_PASSWORD: &str = "hunter2-server-password";

/// One recorded request: (path, x-api-key header value, body).
type Recorded = (String, Option<String>, String);

/// Reads one HTTP request off `stream`, returning `(path, api_key, body)`.
/// Shared by both mocks below — the wire format is the same, only the answers
/// differ.
fn read_request(reader: &mut BufReader<std::net::TcpStream>) -> Option<Recorded> {
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();

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
    Some((path, api_key, String::from_utf8_lossy(&body).to_string()))
}

fn respond(reader: BufReader<std::net::TcpStream>, status: u16, payload: &str) {
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    let _ = reader.into_inner().write_all(response.as_bytes());
}

/// What the BlueBubbles mock should answer with, in order — one entry per
/// `message/query` call.
enum Page {
    /// A normal `{status, message, data}` envelope carrying these messages.
    Messages(Vec<serde_json::Value>),
    /// An HTTP failure whose body echoes the request URL back, password and
    /// all — a real shape for a reverse proxy sitting in front of a
    /// BlueBubbles server, and the one thing `redact_url` on the *context*
    /// alone would not catch.
    ErrorEchoingTheUrl(u16),
}

/// Returns the mock's `(host, port)` plus every request body it received.
fn spawn_mock_bluebubbles(pages: Vec<Page>) -> ((String, u16), Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let rec = recorded.clone();
    let pages = Arc::new(Mutex::new(pages.into_iter().collect::<Vec<_>>()));

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let Some(request) = read_request(&mut reader) else {
                continue;
            };
            let path = request.0.clone();
            rec.lock().unwrap().push(request);

            let next = {
                let mut pages = pages.lock().unwrap();
                if pages.is_empty() {
                    None
                } else {
                    Some(pages.remove(0))
                }
            };
            match next {
                Some(Page::ErrorEchoingTheUrl(status)) => {
                    let payload = serde_json::json!({
                        "status": status,
                        "message": format!("upstream refused http://{addr}{path}"),
                        "data": null,
                    })
                    .to_string();
                    respond(reader, status, &payload);
                }
                Some(Page::Messages(messages)) => {
                    let payload = serde_json::json!({
                        "status": 200, "message": "Success", "data": messages,
                    })
                    .to_string();
                    respond(reader, 200, &payload);
                }
                // Past the script: an empty page, the steady state of an idle
                // server.
                None => {
                    let payload = r#"{"status":200,"message":"Success","data":[]}"#.to_string();
                    respond(reader, 200, &payload);
                }
            }
        }
    });

    ((addr.ip().to_string(), addr.port()), recorded)
}

/// The CI-Server end. `rejects` names the message guids to answer with a
/// permanent 400 — the same content-addressed scripting `imessage::e2e` uses,
/// for the same reason: which request is "the one under test" is about what it
/// says, not when it arrived.
fn spawn_mock_ci_server(rejects: Vec<&'static str>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let rec = recorded.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let Some(request) = read_request(&mut reader) else {
                continue;
            };
            let body = request.2.clone();
            rec.lock().unwrap().push(request);

            if let Some(guid) = rejects.iter().find(|guid| body.contains(**guid)) {
                let payload = serde_json::json!({
                    "statusCode": 400,
                    "error": "Bad Request",
                    "message": format!("rejected {guid}"),
                })
                .to_string();
                respond(reader, 400, &payload);
            } else {
                respond(reader, 200, r#"{"ingested":1,"duplicate":0,"rejected":[]}"#);
            }
        }
    });

    (origin, recorded)
}

fn lane_config(origin: &str) -> AppConfig {
    let mut config = AppConfig::default();
    config.server.portal_url = origin.to_string();
    // 64 lowercase hex → classified as an x-api-key credential, the same
    // fixture value the other lanes' e2e tests use.
    config.server.applet_auth_token = Some("ab".repeat(32));
    config.bluebubbles.enabled = true;
    config
}

fn client_for(host: &str, port: u16) -> BlueBubblesClient {
    BlueBubblesClient::new(BlueBubblesClientConfig {
        host: host.to_string(),
        port,
        use_https: false,
        password: SERVER_PASSWORD.to_string(),
    })
    .unwrap()
}

/// `dateCreated` is a Unix millisecond epoch on this API (unlike a raw
/// chat.db) — see `client::BlueBubblesMessage`.
fn message(guid: &str, text: &str, date_created: Option<i64>) -> serde_json::Value {
    let mut value = serde_json::json!({
        "guid": guid,
        "text": text,
        "isFromMe": false,
        "handle": {"address": "+15551234567", "service": "iMessage"},
        "chats": [{"guid": "chat-1", "chatIdentifier": "+15551234567", "displayName": "Alice"}],
        "attachments": [],
    });
    if let Some(date_created) = date_created {
        value["dateCreated"] = date_created.into();
    }
    value
}

const JAN_1_2026_UNIX_MS: i64 = 1_767_225_600_000;

#[tokio::test(flavor = "multi_thread")]
async fn a_message_reaches_the_server_with_its_text_and_api_key() {
    let ((host, port), bb_requests) = spawn_mock_bluebubbles(vec![Page::Messages(vec![message(
        "MSG-1",
        "hello from the fixture",
        Some(JAN_1_2026_UNIX_MS),
    )])]);
    let (origin, ci_requests) = spawn_mock_ci_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&client_for(&host, port), &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 1);
    assert_eq!(outcome.ingested, 1);
    assert_eq!(outcome.advanced_to, Some(JAN_1_2026_UNIX_MS));

    let bb = bb_requests.lock().unwrap();
    assert_eq!(bb.len(), 1);
    assert!(
        bb[0].0.starts_with("/api/v1/message/query"),
        "{:?}",
        bb[0].0
    );

    let ci = ci_requests.lock().unwrap();
    assert_eq!(ci.len(), 1, "expected exactly one POST, got {ci:?}");
    let (path, api_key, body) = &ci[0];
    assert_eq!(path, "/v1/events");
    assert_eq!(api_key.as_deref(), Some(&"ab".repeat(32)[..]));
    assert!(body.contains("hello from the fixture"), "{body}");
    assert!(body.contains("bluebubbles_message"), "{body}");
    assert!(body.contains("bluebubbles:MSG-1"), "{body}");
}

/// The client claims to mirror CI-Server's `bluebubblesApiClient.ts`
/// field-for-field, and that client OMITS `after` on a first sync rather than
/// sending `"after": null`.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_poll_omits_after_rather_than_sending_null() {
    let ((host, port), bb_requests) = spawn_mock_bluebubbles(vec![
        Page::Messages(vec![message("MSG-1", "first", Some(JAN_1_2026_UNIX_MS))]),
        Page::Messages(vec![]),
    ]);
    let (origin, _) = spawn_mock_ci_server(vec![]);
    let config = lane_config(&origin);
    let client = client_for(&host, port);

    poll_once(&client, &config, Uuid::nil(), None, 10)
        .await
        .unwrap();
    poll_once(&client, &config, Uuid::nil(), Some(JAN_1_2026_UNIX_MS), 10)
        .await
        .unwrap();

    let bb = bb_requests.lock().unwrap();
    let first: serde_json::Value = serde_json::from_str(&bb[0].2).unwrap();
    assert!(
        !first.as_object().unwrap().contains_key("after"),
        "`after` must be absent, not null, on a first sync: {}",
        bb[0].2
    );
    assert_eq!(first["sort"], "ASC");

    let second: serde_json::Value = serde_json::from_str(&bb[1].2).unwrap();
    assert_eq!(second["after"], JAN_1_2026_UNIX_MS);
}

#[tokio::test(flavor = "multi_thread")]
async fn all_messages_in_the_page_are_sent_in_order() {
    let ((host, port), _) = spawn_mock_bluebubbles(vec![Page::Messages(vec![
        message("MSG-1", "first", Some(JAN_1_2026_UNIX_MS)),
        message("MSG-2", "second", Some(JAN_1_2026_UNIX_MS + 60_000)),
        message("MSG-3", "third", Some(JAN_1_2026_UNIX_MS + 120_000)),
    ])]);
    let (origin, ci_requests) = spawn_mock_ci_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&client_for(&host, port), &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 3);
    assert_eq!(outcome.ingested, 3);
    assert_eq!(outcome.advanced_to, Some(JAN_1_2026_UNIX_MS + 120_000));

    let ci = ci_requests.lock().unwrap();
    let bodies: Vec<&String> = ci.iter().map(|(_, _, b)| b).collect();
    assert!(bodies[0].contains("bluebubbles:MSG-1"));
    assert!(bodies[1].contains("bluebubbles:MSG-2"));
    assert!(bodies[2].contains("bluebubbles:MSG-3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mid_page_rejection_stops_the_cursor_there_not_at_the_end() {
    let ((host, port), _) = spawn_mock_bluebubbles(vec![Page::Messages(vec![
        message("MSG-1", "ok", Some(JAN_1_2026_UNIX_MS)),
        message("MSG-2", "rejected", Some(JAN_1_2026_UNIX_MS + 60_000)),
        message("MSG-3", "never reached", Some(JAN_1_2026_UNIX_MS + 120_000)),
    ])]);
    let (origin, ci_requests) = spawn_mock_ci_server(vec!["MSG-2"]);
    let config = lane_config(&origin);

    let outcome = poll_once(&client_for(&host, port), &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 3);
    assert_eq!(outcome.ingested, 1, "only MSG-1 was accepted");
    // MSG-2 and MSG-3 are both retried next poll rather than MSG-2 being
    // silently skipped.
    assert_eq!(outcome.advanced_to, Some(JAN_1_2026_UNIX_MS));
    assert_eq!(
        ci_requests.lock().unwrap().len(),
        2,
        "MSG-3 must never be sent once MSG-2 stopped the page"
    );
}

/// A message with no `dateCreated` is genuinely lost — the cursor is a
/// `dateCreated`, so a later message in the same page moves past it for good.
/// It has to be counted, not passed over at `debug`.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_with_no_date_created_is_counted_as_dropped_not_silently_passed_over() {
    let ((host, port), _) = spawn_mock_bluebubbles(vec![Page::Messages(vec![
        message("MSG-NO-DATE", "when did this happen?", None),
        message("MSG-2", "later", Some(JAN_1_2026_UNIX_MS + 60_000)),
    ])]);
    let (origin, ci_requests) = spawn_mock_ci_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&client_for(&host, port), &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 2);
    assert_eq!(outcome.ingested, 1);
    assert_eq!(outcome.dropped_no_timestamp, 1);
    // And the cursor really has moved past it — this is a report of a loss,
    // not a deferral.
    assert_eq!(outcome.advanced_to, Some(JAN_1_2026_UNIX_MS + 60_000));

    let ci = ci_requests.lock().unwrap();
    assert_eq!(ci.len(), 1);
    assert!(!ci[0].2.contains("MSG-NO-DATE"), "{:?}", ci[0].2);
}

/// The redaction guarantee end to end, through a real socket: a server error
/// whose body quotes the request URL back. The password rides in that URL on
/// every single request (BlueBubbles has no header auth), and `poll_once`'s
/// caller logs the failure with `{err:?}` at `warn`.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_echoing_the_request_url_still_renders_without_the_password() {
    let ((host, port), _) = spawn_mock_bluebubbles(vec![Page::ErrorEchoingTheUrl(502)]);
    let (origin, _) = spawn_mock_ci_server(vec![]);
    let config = lane_config(&origin);

    let err = poll_once(&client_for(&host, port), &config, Uuid::nil(), None, 10)
        .await
        .unwrap_err();

    let rendered = format!("{err:?}");
    assert!(
        !rendered.contains(SERVER_PASSWORD),
        "password leaked through a server error body: {rendered}"
    );
    assert!(
        rendered.contains("502") && rendered.contains("upstream refused"),
        "the failure itself must still be diagnosable — redaction must not eat the message: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_page_is_a_no_op_not_an_error() {
    let ((host, port), _) = spawn_mock_bluebubbles(vec![Page::Messages(vec![])]);
    let (origin, ci_requests) = spawn_mock_ci_server(vec![]);
    let config = lane_config(&origin);

    let outcome = poll_once(&client_for(&host, port), &config, Uuid::nil(), None, 10)
        .await
        .unwrap();

    assert_eq!(outcome.fetched, 0);
    assert_eq!(outcome.ingested, 0);
    assert_eq!(outcome.advanced_to, None);
    assert!(ci_requests.lock().unwrap().is_empty());
}
