use std::error::Error;

use anyhow::{Context, Result};
use reqwest::{RequestBuilder, Url};

use crate::config::AppConfig;

/// Turn a `reqwest::Error` into a short, user-facing explanation. Shared by the
/// CLI `--check-server` report and the settings-window connection tests.
pub fn friendly_error(err: &reqwest::Error) -> String {
    let chain = error_chain(err);
    let lower = chain.to_ascii_lowercase();
    if lower.contains("certificate") || lower.contains("unknownissuer") {
        "TLS certificate rejected; use a trusted CI-Server URL or install the server certificate"
            .to_string()
    } else if lower.contains("tls") || lower.contains("ssl") || lower.contains("alert") {
        "TLS handshake failed; check the URL and certificate".to_string()
    } else if err.is_connect() {
        "connection refused / host unreachable".to_string()
    } else if err.is_timeout() {
        "timed out".to_string()
    } else {
        err.to_string()
    }
}

fn error_chain(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        text.push_str(": ");
        text.push_str(&err.to_string());
        source = err.source();
    }
    text
}

pub fn server_origin(portal_url: &str) -> Result<Url> {
    let parsed =
        Url::parse(portal_url).with_context(|| format!("parsing portal URL {portal_url}"))?;
    let host = parsed
        .host_str()
        .context("portal URL is missing a hostname")?;
    let mut origin = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Url::parse(&origin).context("building portal origin URL")
}

pub fn graphql_urls(portal_url: &str) -> Result<Vec<Url>> {
    api_endpoint_urls(portal_url, "/api/graphql")
}

pub fn upload_urls(portal_url: &str) -> Result<Vec<Url>> {
    api_endpoint_urls(portal_url, "/api/files/upload")
}

pub fn events_urls(portal_url: &str) -> Result<Vec<Url>> {
    api_endpoint_urls(portal_url, "/api/v1/events")
}

pub fn devices_urls(portal_url: &str) -> Result<Vec<Url>> {
    api_endpoint_urls(portal_url, "/api/devices")
}

/// Candidate URLs for a single file resource (`/api/files/{id}` and the bare
/// `/files/{id}` fallback), used for metadata updates.
pub fn files_urls(portal_url: &str, file_id: &str) -> Result<Vec<Url>> {
    api_endpoint_urls(portal_url, &format!("/api/files/{file_id}"))
}

pub fn api_endpoint_urls(portal_url: &str, prefixed_path: &str) -> Result<Vec<Url>> {
    let parsed =
        Url::parse(portal_url).with_context(|| format!("parsing portal URL {portal_url}"))?;
    let host = parsed
        .host_str()
        .context("portal URL is missing a hostname")?;
    let mut base = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        base.push(':');
        base.push_str(&port.to_string());
    }

    let trimmed_path = parsed.path().trim_matches('/');
    if !trimmed_path.is_empty() {
        base.push('/');
        base.push_str(trimmed_path);
        base.push('/');
        let base = Url::parse(&base).context("building CI-Server base URL")?;
        let bare = prefixed_path.strip_prefix("/api/").unwrap_or(prefixed_path);
        return Ok(vec![base.join(bare.trim_start_matches('/')).with_context(
            || format!("building CI-Server endpoint URL for {bare}"),
        )?]);
    }

    let origin = Url::parse(&base).context("building CI-Server origin URL")?;
    let bare_path = prefixed_path.strip_prefix("/api").unwrap_or(prefixed_path);
    let mut urls = Vec::new();
    for path in [prefixed_path, bare_path] {
        let url = origin
            .join(path)
            .with_context(|| format!("building CI-Server endpoint URL for {path}"))?;
        if !urls.iter().any(|existing: &Url| existing == &url) {
            urls.push(url);
        }
    }
    Ok(urls)
}

#[derive(Debug, Clone)]
pub enum AuthCredential {
    Bearer(String),
    ApiKey(String),
}

impl AuthCredential {
    pub fn from_config(config: &AppConfig) -> Result<Self> {
        let raw = config
            .server
            .applet_auth_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .context("applet auth token is not configured")?;
        Ok(Self::from_token(raw))
    }

    pub fn from_token(raw: &str) -> Self {
        let token = raw.trim();
        let bearer = token.strip_prefix("Bearer ").unwrap_or(token).trim();
        if is_ci_server_api_key(bearer) {
            Self::ApiKey(bearer.to_string())
        } else {
            Self::Bearer(bearer.to_string())
        }
    }

    pub fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        match self {
            Self::Bearer(token) => request.bearer_auth(token),
            Self::ApiKey(key) => request.header("x-api-key", key),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Bearer(_) => "bearer",
            Self::ApiKey(_) => "api-key",
        }
    }
}

fn is_ci_server_api_key(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::{AuthCredential, events_urls, graphql_urls, upload_urls};

    #[test]
    fn detects_ci_server_api_key_shape() {
        let raw = "a".repeat(64);
        assert!(matches!(
            AuthCredential::from_token(&raw),
            AuthCredential::ApiKey(_)
        ));
    }

    #[test]
    fn treats_jwt_like_token_as_bearer() {
        assert!(matches!(
            AuthCredential::from_token("Bearer header.payload.signature"),
            AuthCredential::Bearer(_)
        ));
    }

    #[test]
    fn origin_without_path_returns_prefixed_then_bare_candidates() {
        let upload = upload_urls("https://server.example.com").unwrap();
        assert_eq!(
            upload[0].as_str(),
            "https://server.example.com/api/files/upload"
        );
        assert_eq!(
            upload[1].as_str(),
            "https://server.example.com/files/upload"
        );

        let events = events_urls("http://192.168.1.20:62345").unwrap();
        assert_eq!(
            events[0].as_str(),
            "http://192.168.1.20:62345/api/v1/events"
        );
        assert_eq!(events[1].as_str(), "http://192.168.1.20:62345/v1/events");

        let graphql = graphql_urls("http://localhost:62345").unwrap();
        assert_eq!(graphql[0].as_str(), "http://localhost:62345/api/graphql");
        assert_eq!(graphql[1].as_str(), "http://localhost:62345/graphql");
    }

    #[test]
    fn configured_base_path_is_honored() {
        let upload = upload_urls("https://server.example.com/api").unwrap();
        assert_eq!(upload.len(), 1);
        assert_eq!(
            upload[0].as_str(),
            "https://server.example.com/api/files/upload"
        );

        let events = events_urls("https://server.example.com/custom/api/").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].as_str(),
            "https://server.example.com/custom/api/v1/events"
        );
    }
}
