//! Plain REST client for one BlueBubbles server
//! (<https://github.com/BlueBubblesApp/bluebubbles-app> — a macOS app exposing
//! its host's iMessage/SMS history over HTTP).
//!
//! Deliberately mirrors CI-Server's own `bluebubblesApiClient.ts`
//! (`backend/apps/api/src/domains/import/datasources/bluebubblesApiClient.ts`)
//! field-for-field and endpoint-for-endpoint — same auth (`?password=` query
//! param on every request, BlueBubbles has no header-auth story), same
//! `{status, message, data}` envelope, same `message/query` / `chat/query`
//! POST bodies — so the two implementations can be cross-checked against each
//! other rather than against BlueBubbles' documentation alone.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rows requested per page. Matches CI-Server's `BLUEBUBBLES_PAGE_LIMIT`.
pub const PAGE_LIMIT: u32 = 1000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct BlueBubblesHandle {
    pub address: String,
    pub service: Option<String>,
}

/// `style`: 43 = group chat, 45 = direct message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct BlueBubblesChat {
    pub guid: String,
    #[serde(rename = "chatIdentifier")]
    pub chat_identifier: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    pub style: Option<i64>,
    #[serde(default)]
    pub participants: Vec<BlueBubblesHandle>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct BlueBubblesAttachment {
    pub guid: String,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(rename = "transferName")]
    pub transfer_name: Option<String>,
}

/// One iMessage/SMS row. Every `date_*` field is a standard Unix millisecond
/// epoch (unlike a raw chat.db, which stores the Cocoa epoch) — BlueBubbles'
/// API always normalizes to Unix ms.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct BlueBubblesMessage {
    pub guid: String,
    pub text: Option<String>,
    pub handle: Option<BlueBubblesHandle>,
    #[serde(rename = "isFromMe")]
    pub is_from_me: Option<bool>,
    #[serde(rename = "dateCreated")]
    pub date_created: Option<i64>,
    #[serde(default)]
    pub chats: Vec<BlueBubblesChat>,
    #[serde(default)]
    pub attachments: Vec<BlueBubblesAttachment>,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    status: i32,
    message: String,
    data: T,
}

#[derive(Debug, Clone)]
pub struct BlueBubblesClientConfig {
    pub host: String,
    pub port: u16,
    pub use_https: bool,
    /// BlueBubbles' own server password — a full-control credential.
    pub password: String,
}

/// Strip every `password=` query value out of `text` before it is ever logged
/// — BlueBubbles has no header-auth story, so the password rides in the query
/// string of every request.
///
/// Takes arbitrary text, not only a bare URL, because a URL comes back inside
/// a sentence at least as often as it appears alone (an error body echoing the
/// request, a message with the URL in the middle). That is also why the value
/// ends at whitespace or a quote as well as at `&`: terminating only on `&`
/// meant a URL at the end of a sentence swallowed the rest of the message,
/// taking the actual diagnosis with it.
pub fn redact_url(text: &str) -> String {
    const NEEDLE: &str = "password=";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(NEEDLE) {
        out.push_str(&rest[..idx]);
        out.push_str("password=REDACTED");
        let after = &rest[idx + NEEDLE.len()..];
        let end = after
            .find(|c: char| c == '&' || c == '#' || c == '"' || c.is_ascii_whitespace())
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// A BlueBubbles request failure with the password already gone from every
/// layer of it.
///
/// Applying [`redact_url`] to the *context* is not enough, which is how the
/// password reached the log despite that function being correct:
/// `reqwest::Error`'s own `Display` appends ` for url (<the full URL>)`, and
/// `{err:?}` on an anyhow chain prints every source beneath the context. The
/// warning that carried it — "poll failed, will retry next tick" — fires on
/// the routine case of the LAN Mac being asleep, at a level the default
/// `info` filter emits.
///
/// So this type is the only thing the client returns for a transport or
/// decode failure, and it deliberately has **no `source`**: there is nothing
/// left in the chain for anything downstream to print.
#[derive(Debug)]
pub struct BlueBubblesError(String);

impl std::fmt::Display for BlueBubblesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BlueBubblesError {}

pub struct BlueBubblesClient {
    config: BlueBubblesClientConfig,
    http: Client,
}

impl BlueBubblesClient {
    pub fn new(config: BlueBubblesClientConfig) -> Result<Self> {
        // No proxy / no redirect, matching every other client in this crate —
        // a capture client whose whole claim is "your data reaches only your
        // own machines" cannot let an inherited shell proxy override that.
        let http = Client::builder()
            .no_proxy()
            .use_rustls_tls()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building BlueBubbles client")?;
        Ok(Self { config, http })
    }

    fn base_url(&self) -> String {
        let scheme = if self.config.use_https {
            "https"
        } else {
            "http"
        };
        format!(
            "{scheme}://{}:{}/api/v1",
            self.config.host, self.config.port
        )
    }

    fn url_for(&self, path: &str) -> String {
        format!(
            "{}/{path}?password={}",
            self.base_url(),
            urlencoding_password(&self.config.password)
        )
    }

    /// Every string this client puts into an error goes through here — not
    /// just the ones that look like a URL. [`redact_url`] only knows the
    /// query-string shape, and the two other ways the secret gets out are a
    /// reqwest source layer and a server (or a proxy in front of it) echoing
    /// the request URL back inside an error body. The literal value is what
    /// must not survive, wherever it turns up.
    fn redact(&self, text: &str) -> String {
        if self.config.password.is_empty() {
            // `replace("")` would splice REDACTED between every character.
            return redact_url(text);
        }
        let text = text
            .replace(&self.config.password, "REDACTED")
            .replace(&urlencoding_password(&self.config.password), "REDACTED");
        redact_url(&text)
    }

    /// Rebuild reqwest's message from the error's *shape* plus its sources.
    /// reqwest's own `Display` is unusable — it is the thing that carries the
    /// URL — but the layers beneath it (hyper/rustls/io) hold the diagnosis
    /// worth keeping ("Connection refused", "dns error", "certificate
    /// expired") and no URL, so they are kept, redacted anyway.
    fn transport_error(&self, action: &str, url: &str, err: &reqwest::Error) -> BlueBubblesError {
        let shape = if err.is_timeout() {
            "timed out"
        } else if err.is_connect() {
            "could not be reached"
        } else if err.is_decode() {
            "sent a response this client could not decode"
        } else if err.is_body() {
            "sent a response body that failed mid-transfer"
        } else if err.is_request() {
            "rejected the request before it was sent"
        } else {
            "failed"
        };

        let mut causes = Vec::new();
        let mut source = std::error::Error::source(err);
        while let Some(cause) = source {
            causes.push(cause.to_string());
            source = cause.source();
        }
        let detail = if causes.is_empty() {
            String::new()
        } else {
            format!(" — {}", causes.join(": "))
        };

        BlueBubblesError(self.redact(&format!("{action} {} {shape}{detail}", redact_url(url))))
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let url = self.url_for(path);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| self.transport_error("GET", &url, &err))?;
        self.decode(response, &url).await
    }

    async fn post<T: for<'de> Deserialize<'de>>(&self, path: &str, body: Value) -> Result<T> {
        let url = self.url_for(path);
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|err| self.transport_error("POST", &url, &err))?;
        self.decode(response, &url).await
    }

    async fn decode<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
        url: &str,
    ) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlueBubblesError(self.redact(&format!(
                "BlueBubbles request failed: HTTP {status} {} — {}",
                redact_url(url),
                body.chars().take(300).collect::<String>()
            )))
            .into());
        }
        let envelope: Envelope<T> = response
            .json()
            .await
            .map_err(|err| self.transport_error("decoding the response to", url, &err))?;
        if envelope.status >= 400 {
            return Err(BlueBubblesError(self.redact(&format!(
                "BlueBubbles reported an error: {} ({})",
                envelope.message, envelope.status
            )))
            .into());
        }
        Ok(envelope.data)
    }

    /// `GET /server/info` — a cheap authenticated ping.
    pub async fn ping(&self) -> Result<Value> {
        self.get("server/info").await
    }

    /// One page of messages, ascending by `dateCreated`, strictly after
    /// `after` (ms epoch) when given — mirrors `bluebubblesApiClient.ts`'s
    /// `queryMessages`.
    pub async fn query_messages(
        &self,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<BlueBubblesMessage>> {
        let mut body = serde_json::json!({
            "sort": "ASC",
            "limit": limit,
            "offset": 0,
            "with": ["chats", "chats.participants", "handle", "attachments"],
        });
        // Omitted, not `null`. `json!({"after": after})` on a `None` emits
        // `"after": null`, and the TS client this file mirrors field-for-field
        // leaves the key out entirely — a server that treats a present-but-null
        // filter differently from an absent one would silently diverge between
        // the two implementations, which is the whole thing cross-checking them
        // is meant to prevent.
        if let Some(after) = after {
            body["after"] = after.into();
        }
        self.post("message/query", body).await
    }
}

/// BlueBubbles passwords are plain server-set strings, not usually containing
/// reserved query characters, but this is a network boundary — encode it
/// properly rather than trust that.
fn urlencoding_password(password: &str) -> String {
    let mut out = String::with_capacity(password.len());
    for byte in password.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_password_from_a_query_string() {
        let url = "http://192.168.0.4:1234/api/v1/server/info?password=hunter2&x=1";
        assert_eq!(
            redact_url(url),
            "http://192.168.0.4:1234/api/v1/server/info?password=REDACTED&x=1"
        );
    }

    #[test]
    fn redacts_password_as_the_trailing_param_too() {
        let url = "http://host:1234/api/v1/server/info?x=1&password=hunter2";
        assert_eq!(
            redact_url(url),
            "http://host:1234/api/v1/server/info?x=1&password=REDACTED"
        );
    }

    #[test]
    fn leaves_a_url_with_no_password_untouched() {
        let url = "http://host:1234/api/v1/server/info";
        assert_eq!(redact_url(url), url);
    }

    /// A URL usually arrives inside a sentence — an error body quoting the
    /// request back, say. Terminating the value only at `&` used to eat the
    /// remainder of the message along with the secret.
    #[test]
    fn redacts_a_url_embedded_in_text_without_swallowing_the_rest_of_it() {
        assert_eq!(
            redact_url("upstream refused http://host:1234/api/v1/x?password=hunter2 after 3 tries"),
            "upstream refused http://host:1234/api/v1/x?password=REDACTED after 3 tries"
        );
    }

    #[test]
    fn redacts_every_occurrence_not_just_the_first() {
        assert_eq!(
            redact_url("GET ?password=a failed; retrying ?password=b"),
            "GET ?password=REDACTED failed; retrying ?password=REDACTED"
        );
    }

    #[test]
    fn percent_encodes_reserved_characters_in_the_password() {
        assert_eq!(urlencoding_password("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlencoding_password("plain-Pass_123.~"), "plain-Pass_123.~");
    }

    const SECRET: &str = "hunter2-server-password";

    fn client_pointed_at_nothing() -> BlueBubblesClient {
        // A port nothing is listening on: bind one to learn a free number,
        // then drop it. Connecting there fails the way the user's LAN Mac
        // failing to answer does — which is the routine case that logged the
        // password, not an exotic one.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        BlueBubblesClient::new(BlueBubblesClientConfig {
            host: "127.0.0.1".to_string(),
            port,
            use_https: false,
            password: SECRET.to_string(),
        })
        .unwrap()
    }

    /// `redact_url` was already correct and already applied — the leak went
    /// around it. `reqwest::Error`'s own `Display` appends ` for url (<full
    /// url>)`, and `{err:?}` on an anyhow chain prints every source under the
    /// redacted context, so the password came back out in
    /// `bluebubbles: poll failed, will retry next tick: {err:?}` — a warning
    /// the default `info` filter emits in ordinary builds.
    #[tokio::test]
    async fn a_transport_failure_renders_with_no_password_anywhere_in_its_chain() {
        let err = client_pointed_at_nothing().ping().await.unwrap_err();

        let debug = format!("{err:?}");
        assert!(
            !debug.contains(SECRET),
            "password leaked into {{err:?}}: {debug}"
        );
        assert!(
            !debug.contains(&urlencoding_password(SECRET)),
            "url-encoded password leaked into {{err:?}}: {debug}"
        );
        assert!(
            debug.contains("password=REDACTED"),
            "the redacted URL should still be there for diagnosis: {debug}"
        );
        assert!(
            !format!("{err}").contains(SECRET),
            "password leaked into Display"
        );
        // Belt and braces: no link of the chain may render it either, however
        // a caller chooses to walk it.
        for link in err.chain() {
            assert!(
                !link.to_string().contains(SECRET),
                "password leaked into a chain link: {link}"
            );
        }
    }

    /// The same guarantee for the other reqwest-producing call, and through
    /// the POST body path rather than the GET one.
    #[tokio::test]
    async fn a_failed_message_query_renders_with_no_password_either() {
        let err = client_pointed_at_nothing()
            .query_messages(None, 10)
            .await
            .unwrap_err();
        let debug = format!("{err:?}");
        assert!(!debug.contains(SECRET), "{debug}");
    }
}
