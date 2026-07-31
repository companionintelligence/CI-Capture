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
        rust_i18n::t!("status.error.tls_certificate").to_string()
    } else if lower.contains("tls") || lower.contains("ssl") || lower.contains("alert") {
        rust_i18n::t!("status.error.tls_handshake").to_string()
    } else if err.is_connect() {
        rust_i18n::t!("status.error.connect_refused").to_string()
    } else if err.is_timeout() {
        rust_i18n::t!("status.error.timed_out").to_string()
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

/// Canonicalize a user-entered Companion Memory server address into an origin.
///
/// Strips query and fragment, and drops a `/feed` path — the Companion Memory
/// web app's own route, which users reasonably paste when copying from a
/// browser but which is not the API root.
///
/// Supplies a scheme when the address has none: `core-1:8642` is what people
/// actually type (it's the placeholder text minus the `http://`), and
/// `Url::parse` reads that as scheme `core-1` with path `8642` — a URL with no
/// host, which parses fine here and then fails in every caller with
/// "portal URL is missing a hostname". Guessing the scheme is safe because the
/// connection test immediately proves it: a server is normally reached over the
/// LAN or a tailnet, so a local-looking host gets `http`, and anything else
/// gets `https` rather than silently sending a credential in the clear.
///
/// Rejecting host-less input is the point of the final check — an address that
/// can never work must not reach `config.json`, where it becomes an opaque
/// runtime error far from the field that caused it.
///
/// Shared by the settings form, the setup assistant, and Portal discovery so a
/// hand-typed address and a discovered one normalize identically; otherwise the
/// same server arriving by both routes would look like two different servers.
pub fn normalize_server_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(rust_i18n::t!("status.address.empty").to_string());
    }

    let invalid = || rust_i18n::t!("status.address.invalid").to_string();
    let parsed = Url::parse(raw).ok().filter(|u| u.has_host());
    let mut url = match parsed {
        Some(url) => url,
        // An address that already spells out a scheme and *still* didn't yield
        // a host is just wrong (`http://`, `://core-1`); supplying another
        // scheme would turn it into nonsense like `http://http/`.
        None if raw.contains("://") => return Err(invalid()),
        // No scheme (or a "scheme" that is really the hostname): re-parse with
        // one supplied.
        None => {
            let scheme = if is_local_host(host_of(raw)) {
                "http"
            } else {
                "https"
            };
            Url::parse(&format!("{scheme}://{raw}"))
                .ok()
                .filter(|u| u.has_host())
                .ok_or_else(invalid)?
        }
    };
    url.set_query(None);
    url.set_fragment(None);

    let path = url.path().trim_matches('/');
    if path.is_empty() || path == "feed" || path.starts_with("feed/") {
        url.set_path("");
    }

    Ok(url.to_string())
}

/// The host portion of a scheme-less address (`core-1:8642/x` → `core-1`).
fn host_of(raw: &str) -> &str {
    raw.split('/')
        .next()
        .unwrap_or(raw)
        .rsplit('@')
        .next()
        .unwrap_or(raw)
        .split(':')
        .next()
        .unwrap_or(raw)
}

/// Whether a host is one we'd expect to reach over plain HTTP: a bare hostname
/// with no dots (`core-1`), mDNS/tailnet names, loopback, or a private IPv4
/// range. Everything else is treated as public, where HTTPS is the safe guess.
fn is_local_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    if !host.contains('.') {
        return true; // single label: `core-1`, `localhost`
    }
    if host.ends_with(".local") || host.ends_with(".ts.net") || host.ends_with(".internal") {
        return true;
    }
    match host.parse::<std::net::Ipv4Addr>() {
        Ok(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        Err(_) => false,
    }
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

/// The user-facing Companion Memory web app, for opening in a browser.
///
/// CI-Server serves the memory SPA and the API from a single origin, so this is
/// the ingest origin with `/memory` joined on. `query` pairs are appended
/// percent-encoded — `/memory` accepts `search`, `view`, `from`/`to`, `tags`,
/// `people` and `semantic`.
///
/// Unlike the ingest builders there is no candidate list: a browser gets one
/// URL, and guessing wrong would open a 404 rather than fall through.
pub fn memory_url(portal_url: &str, query: &[(&str, &str)]) -> Result<Url> {
    let mut url = server_origin(portal_url)?
        .join("/memory")
        .context("building memory URL")?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query {
            pairs.append_pair(k, v);
        }
    }
    Ok(url)
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
        AuthCredential, Url, events_urls, graphql_urls, memory_url, normalize_server_url,
        server_origin, upload_urls,
    };

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

    #[test]
    fn memory_url_targets_the_spa_on_the_ingest_origin() {
        assert_eq!(
            memory_url("http://core-1:8642", &[]).unwrap().as_str(),
            "http://core-1:8642/memory"
        );
        // An API base path on the ingest URL must not leak into the SPA link —
        // the web app is mounted at the origin root, not under /api.
        assert_eq!(
            memory_url("https://core.example.com/api/", &[])
                .unwrap()
                .as_str(),
            "https://core.example.com/memory"
        );
    }

    #[test]
    fn memory_url_percent_encodes_query_values() {
        let url = memory_url(
            "http://core-1:8642",
            &[("search", "budget & forecast"), ("view", "list")],
        )
        .unwrap();
        // `&` inside a value must arrive encoded, or it reads as a second
        // parameter — and on Windows would also truncate the shell invocation.
        assert_eq!(
            url.as_str(),
            "http://core-1:8642/memory?search=budget+%26+forecast&view=list"
        );
    }

    #[test]
    fn memory_url_rejects_an_unset_server() {
        assert!(memory_url("", &[]).is_err());
    }

    /// The exact thing people type: the settings placeholder reads
    /// `http://core-1:8642`, and the `http://` is the part that gets dropped.
    /// This used to normalize "successfully" into a URL with no host, which
    /// saved cleanly and then failed at every call site.
    #[test]
    fn scheme_less_address_gets_one_instead_of_becoming_hostless() {
        for raw in ["core-1:8642", "core-1", "my-core.tailnet.ts.net:8642"] {
            let out = normalize_server_url(raw)
                .unwrap_or_else(|e| panic!("{raw:?} should normalize, got {e:?}"));
            let url = Url::parse(&out).unwrap();
            assert!(
                url.host_str().is_some(),
                "{raw:?} normalized to {out:?}, which still has no host"
            );
            // Every consumer goes through server_origin; it must not error.
            assert!(server_origin(&out).is_ok(), "{out:?} has no usable origin");
        }
    }

    #[test]
    fn local_addresses_get_http_and_public_ones_get_https() {
        for (raw, expected) in [
            ("core-1:8642", "http://core-1:8642/"),
            ("localhost:8642", "http://localhost:8642/"),
            (
                "my-core.tailnet.ts.net:8642",
                "http://my-core.tailnet.ts.net:8642/",
            ),
            ("mac.local:8642", "http://mac.local:8642/"),
            ("192.168.1.9:8642", "http://192.168.1.9:8642/"),
            ("[::1]:8642", "http://[::1]:8642/"),
            // Public name: guessing http would put the credential on the wire
            // in the clear, so https is the only safe default.
            ("ci.example.com", "https://ci.example.com/"),
            ("ci.example.com:8642", "https://ci.example.com:8642/"),
        ] {
            assert_eq!(
                normalize_server_url(raw).unwrap(),
                expected,
                "input {raw:?}"
            );
        }
    }

    #[test]
    fn an_explicit_scheme_is_always_respected() {
        // Never "upgrade" what the user actually typed.
        assert_eq!(
            normalize_server_url("http://ci.example.com").unwrap(),
            "http://ci.example.com/"
        );
        assert_eq!(
            normalize_server_url("https://core-1:8642").unwrap(),
            "https://core-1:8642/"
        );
    }

    #[test]
    fn feed_route_and_query_are_still_stripped_without_a_scheme() {
        assert_eq!(
            normalize_server_url("core-1:8642/feed?cursor=abc").unwrap(),
            "http://core-1:8642/"
        );
    }

    #[test]
    fn junk_is_rejected_rather_than_saved() {
        for raw in ["", "   ", "http://", "://core-1", "not a url at all"] {
            assert!(
                normalize_server_url(raw).is_err(),
                "{raw:?} should be rejected, got {:?}",
                normalize_server_url(raw)
            );
        }
    }
}
