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

/// Clean a user-typed CI-Server URL into the origin form the endpoint builders
/// expect. Shared by the settings window and the onboarding wizard so both
/// accept exactly the same input.
///
/// A bare host (`ci.example.com`, `core-1:8642`) is the single most common
/// first-run typo, so a missing scheme is filled in rather than rejected:
/// anything that looks like a loopback/plain host:port gets `http://`,
/// everything else `https://`. Query, fragment, and the portal's `/feed` UI
/// path are stripped — they're navigation state, not API mount points.
pub fn normalize_ci_server_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("CI-Server URL is required.".to_string());
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("{}://{trimmed}", default_scheme_for(trimmed))
    };

    let mut url =
        Url::parse(&with_scheme).map_err(|_| "CI-Server URL must be a valid URL.".to_string())?;
    if url.host_str().is_none() {
        return Err("CI-Server URL is missing a hostname.".to_string());
    }
    url.set_query(None);
    url.set_fragment(None);

    let path = url.path().trim_matches('/');
    if path.is_empty() || path == "feed" || path.starts_with("feed/") {
        url.set_path("");
    }

    Ok(url.to_string())
}

/// Scheme to assume for a scheme-less host. Local/LAN targets are typically
/// plain HTTP (the dev CI-Server default), public hostnames are not.
fn default_scheme_for(host_part: &str) -> &'static str {
    let host = host_part
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(host_part)
        .split(':')
        .next()
        .unwrap_or(host_part);
    let is_local = host == "localhost"
        || host.ends_with(".local")
        || host.ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok()
        // Single-label hosts are LAN machine names (`core-1`), not public DNS.
        || !host.contains('.');
    if is_local { "http" } else { "https" }
}

/// The OIDC issuer a CI-Server deployment conventionally serves, derived from
/// the server URL (`<origin>/api/auth`).
///
/// This is what lets onboarding ask one question instead of two: the wizard
/// derives a candidate, probes its discovery document, and only falls back to
/// asking when the probe fails. Returns `None` when the server URL isn't
/// usable yet.
pub fn derive_issuer_url(server_url: &str) -> Option<String> {
    let normalized = normalize_ci_server_url(server_url).ok()?;
    let base = Url::parse(&normalized).ok()?;
    // Honour a mounted base path (`https://host/ci/`) the same way the
    // endpoint builders do, rather than assuming the origin root.
    let joined = if base.path().trim_matches('/').is_empty() {
        base.join("/api/auth").ok()?
    } else {
        Url::parse(&format!("{}/api/auth", normalized.trim_end_matches('/'))).ok()?
    };
    Some(joined.to_string())
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

/// Candidate URLs for the unified capture-ingest endpoint.
///
/// CI-Server's canonical route is the bare `POST /v1/events` (no `/api` prefix),
/// per `docs/architecture/event-ingest-spec.md` — the same path the browser
/// extension posts to. We probe that first and keep the legacy `/api/v1/events`
/// only as a compatibility fallback for older/proxied deployments, so a modern
/// CI-Server no longer eats a wasted 404 round-trip on every event. (File upload
/// stays at `/api/files/upload`; only the events route reconciles to bare.)
pub fn events_urls(portal_url: &str) -> Result<Vec<Url>> {
    api_endpoint_urls_ordered(portal_url, "/v1/events", &["/v1/events", "/api/v1/events"])
}

pub fn devices_urls(portal_url: &str) -> Result<Vec<Url>> {
    api_endpoint_urls(portal_url, "/api/devices")
}

/// Candidate URLs for a single file resource (`/api/files/{id}` and the bare
/// `/files/{id}` fallback), used for metadata updates.
pub fn files_urls(portal_url: &str, file_id: &str) -> Result<Vec<Url>> {
    api_endpoint_urls(portal_url, &format!("/api/files/{file_id}"))
}

/// Build endpoint candidates for a `/api`-prefixed path, probing the prefixed
/// form first and the bare form second (the historical default for upload,
/// graphql, devices, and file-metadata routes).
pub fn api_endpoint_urls(portal_url: &str, prefixed_path: &str) -> Result<Vec<Url>> {
    let bare_path = prefixed_path.strip_prefix("/api").unwrap_or(prefixed_path);
    api_endpoint_urls_ordered(portal_url, bare_path, &[prefixed_path, bare_path])
}

/// Build endpoint candidates from an explicit, ordered candidate list so callers
/// can decide whether the bare or the `/api`-prefixed form is canonical.
///
/// When the portal URL carries its own base path (e.g. `https://host/custom/`),
/// there is a single unambiguous mount point, so we join `bare_when_based` onto
/// it and ignore the prefixed/bare probing entirely — matching prior behavior.
fn api_endpoint_urls_ordered(
    portal_url: &str,
    bare_when_based: &str,
    candidates: &[&str],
) -> Result<Vec<Url>> {
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
        return Ok(vec![
            base.join(bare_when_based.trim_start_matches('/'))
                .with_context(|| {
                    format!("building CI-Server endpoint URL for {bare_when_based}")
                })?,
        ]);
    }

    let origin = Url::parse(&base).context("building CI-Server origin URL")?;
    let mut urls = Vec::new();
    for path in candidates {
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
    use super::{
        AuthCredential, derive_issuer_url, events_urls, graphql_urls, normalize_ci_server_url,
        upload_urls,
    };

    #[test]
    fn normalize_fills_in_a_missing_scheme() {
        // The most common first-run typo: a bare host. Public names get https,
        // LAN/loopback names get http (the dev CI-Server default).
        assert_eq!(
            normalize_ci_server_url("ci.example.com").unwrap(),
            "https://ci.example.com/"
        );
        assert_eq!(
            normalize_ci_server_url("core-1:8642").unwrap(),
            "http://core-1:8642/"
        );
        assert_eq!(
            normalize_ci_server_url("localhost:62345").unwrap(),
            "http://localhost:62345/"
        );
        assert_eq!(
            normalize_ci_server_url("192.168.1.20:62345").unwrap(),
            "http://192.168.1.20:62345/"
        );
    }

    #[test]
    fn normalize_strips_ui_path_query_and_fragment() {
        assert_eq!(
            normalize_ci_server_url("https://ci.example.com/feed?cursor=abc#top").unwrap(),
            "https://ci.example.com/"
        );
        // A real mount path is preserved — only the portal's own UI path goes.
        assert_eq!(
            normalize_ci_server_url("https://ci.example.com/api").unwrap(),
            "https://ci.example.com/api"
        );
    }

    #[test]
    fn normalize_rejects_unusable_input() {
        assert!(normalize_ci_server_url("   ").is_err());
        assert!(normalize_ci_server_url("https://").is_err());
    }

    #[test]
    fn issuer_is_derived_from_the_server_url() {
        assert_eq!(
            derive_issuer_url("https://ci.example.com").unwrap(),
            "https://ci.example.com/api/auth"
        );
        // Scheme-less input flows through the same normalization.
        assert_eq!(
            derive_issuer_url("ci.example.com/feed").unwrap(),
            "https://ci.example.com/api/auth"
        );
        // A mounted base path is respected rather than replaced.
        assert_eq!(
            derive_issuer_url("https://host.example.com/ci/").unwrap(),
            "https://host.example.com/ci/api/auth"
        );
        assert!(derive_issuer_url("").is_none());
    }

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

        // Events reconcile to the canonical bare `/v1/events` first (CI-Server's
        // real route, matching the browser extension), with `/api/v1/events`
        // kept only as a compatibility fallback.
        let events = events_urls("http://192.168.1.20:62345").unwrap();
        assert_eq!(events[0].as_str(), "http://192.168.1.20:62345/v1/events");
        assert_eq!(
            events[1].as_str(),
            "http://192.168.1.20:62345/api/v1/events"
        );

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
