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

use anyhow::{Context, Result, anyhow};
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

/// Strip the `password` query param out of a URL before it is ever logged —
/// BlueBubbles has no header-auth story, so the password rides in the query
/// string of every request.
pub fn redact_url(url: &str) -> String {
    match url.find("password=") {
        Some(idx) => {
            let end = url[idx..]
                .find('&')
                .map(|rel| idx + rel)
                .unwrap_or(url.len());
            format!("{}password=REDACTED{}", &url[..idx], &url[end..])
        }
        None => url.to_string(),
    }
}

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

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let url = self.url_for(path);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {}", redact_url(&url)))?;
        Self::decode(response, &url).await
    }

    async fn post<T: for<'de> Deserialize<'de>>(&self, path: &str, body: Value) -> Result<T> {
        let url = self.url_for(path);
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", redact_url(&url)))?;
        Self::decode(response, &url).await
    }

    async fn decode<T: for<'de> Deserialize<'de>>(
        response: reqwest::Response,
        url: &str,
    ) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "BlueBubbles request failed: HTTP {status} {} — {}",
                redact_url(url),
                body.chars().take(300).collect::<String>()
            ));
        }
        let envelope: Envelope<T> = response
            .json()
            .await
            .with_context(|| format!("decoding BlueBubbles response from {}", redact_url(url)))?;
        if envelope.status >= 400 {
            return Err(anyhow!(
                "BlueBubbles reported an error: {} ({})",
                envelope.message,
                envelope.status
            ));
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
        self.post(
            "message/query",
            serde_json::json!({
                "after": after,
                "sort": "ASC",
                "limit": limit,
                "offset": 0,
                "with": ["chats", "chats.participants", "handle", "attachments"],
            }),
        )
        .await
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

    #[test]
    fn percent_encodes_reserved_characters_in_the_password() {
        assert_eq!(urlencoding_password("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlencoding_password("plain-Pass_123.~"), "plain-Pass_123.~");
    }
}
