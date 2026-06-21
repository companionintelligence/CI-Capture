//! Portal sign-in via OpenID Connect **Authorization Code + PKCE** — the same
//! scheme CI-Import-Tools and CI-Server use. Everything is discovery-driven
//! (`<issuer>/.well-known/openid-configuration`), so only the issuer URL and
//! client id change between deployments.
//!
//! Desktop flow (RFC 8252 native app):
//! 1. generate a PKCE verifier + S256 challenge, a `state`, and a `nonce`
//! 2. bind a one-shot loopback HTTP server on the redirect port
//! 3. open the system browser at the authorization endpoint
//! 4. the portal redirects back to `http://127.0.0.1:<port>/callback?code=…`
//! 5. exchange the code (+ verifier) at the token endpoint for tokens
//! 6. decode the `id_token` claims for the signed-in user
//!
//! The returned `id_token` is the JWKS-signed JWT CI-Server validates; the
//! applet forwards it as `Authorization: Bearer …` on ingest.
//!
//! A few discovery fields (`jwks_uri`, `userinfo_endpoint`,
//! `end_session_endpoint`) and the `exp` claim are decoded for completeness but
//! not yet consumed.
#![allow(dead_code)]

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::config::OidcConfig;

/// How long we wait for the user to complete the browser login.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Claims we read out of the `id_token` (no signature check needed — we got it
/// straight from the token endpoint over TLS; CI-Server verifies the signature).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct IdClaims {
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub exp: Option<i64>,
}

pub struct LoginOutcome {
    pub tokens: TokenSet,
    pub claims: IdClaims,
}

// ── PKCE / random ───────────────────────────────────────────────────────────

/// `n` bytes of CSPRNG randomness, base64url-encoded. Uses UUID v4 (getrandom)
/// as the entropy source so we don't add a crypto-rng dependency.
fn random_b64url(n: usize) -> String {
    let mut buf = Vec::with_capacity(n.div_ceil(16) * 16);
    while buf.len() < n {
        buf.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    buf.truncate(n);
    URL_SAFE_NO_PAD.encode(buf)
}

/// A PKCE verifier and its S256 challenge.
pub fn pkce_pair() -> (String, String) {
    let verifier = random_b64url(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

// ── Discovery / endpoints ───────────────────────────────────────────────────

pub async fn discover(client: &Client, issuer: &str) -> Result<Discovery> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let resp = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await
        .with_context(|| format!("OIDC discovery request to {url}"))?;
    if !resp.status().is_success() {
        bail!("OIDC discovery failed: {url} → HTTP {}", resp.status());
    }
    resp.json().await.context("parsing OIDC discovery document")
}

pub fn authorize_url(
    disco: &Discovery,
    cfg: &OidcConfig,
    state: &str,
    nonce: &str,
    challenge: &str,
) -> Result<String> {
    let mut u = Url::parse(&disco.authorization_endpoint).context("parsing authorization endpoint")?;
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &cfg.redirect_uri)
        .append_pair("scope", &cfg.scopes)
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(u.to_string())
}

pub async fn exchange_code(
    client: &Client,
    disco: &Discovery,
    cfg: &OidcConfig,
    code: &str,
    verifier: &str,
) -> Result<TokenSet> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", cfg.redirect_uri.as_str()),
        ("client_id", cfg.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    let resp = client
        .post(&disco.token_endpoint)
        .header("accept", "application/json")
        .form(&params)
        .send()
        .await
        .context("token exchange request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("token exchange failed: HTTP {status}{}", trim_body(&body));
    }
    resp.json().await.context("parsing token response")
}

pub async fn refresh(
    client: &Client,
    disco: &Discovery,
    cfg: &OidcConfig,
    refresh_token: &str,
) -> Result<TokenSet> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", cfg.client_id.as_str()),
    ];
    let resp = client
        .post(&disco.token_endpoint)
        .header("accept", "application/json")
        .form(&params)
        .send()
        .await
        .context("token refresh request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("token refresh failed: HTTP {status}{}", trim_body(&body));
    }
    resp.json().await.context("parsing refreshed token response")
}

/// Decode the (unverified) claims from an `id_token` JWT payload segment.
pub fn decode_id_claims(id_token: &str) -> Result<IdClaims> {
    let payload = id_token
        .split('.')
        .nth(1)
        .context("id_token is not a JWT (missing payload segment)")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("base64url-decoding id_token payload")?;
    serde_json::from_slice(&bytes).context("parsing id_token claims")
}

/// Whether a token with the given expiry should be refreshed now: true when it
/// expires within `skew` of `now`. Unknown expiry → false (don't churn).
pub fn token_expiring(
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    skew: chrono::Duration,
) -> bool {
    match expires_at {
        Some(exp) => exp <= now + skew,
        None => false,
    }
}

fn trim_body(body: &str) -> String {
    let b = body.trim();
    if b.is_empty() {
        String::new()
    } else {
        format!(" — {}", &b[..b.len().min(300)])
    }
}

// ── Loopback callback server ────────────────────────────────────────────────

/// Bind the loopback listener for the redirect. Done *before* opening the
/// browser so the redirect can never race the bind.
async fn bind_loopback(redirect_uri: &str) -> Result<TcpListener> {
    let url = Url::parse(redirect_uri).context("parsing oidc.redirect_uri")?;
    let port = url
        .port()
        .context("oidc.redirect_uri must include a port, e.g. http://127.0.0.1:49473/callback")?;
    let addr = format!("127.0.0.1:{port}");
    TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding loopback {addr} for the OIDC callback"))
}

/// Accept one request on the loopback listener and pull `code` / `state` from
/// the query string, replying with a small "you can close this" page.
async fn accept_code(listener: TcpListener) -> Result<(String, Option<String>)> {
    let (mut sock, _) = listener.accept().await.context("accepting OIDC callback")?;

    let mut buf = vec![0u8; 16 * 1024];
    let n = sock.read(&mut buf).await.context("reading OIDC callback request")?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let request_target = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let query = request_target.split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code = None;
    let mut state = None;
    let mut err = None;
    // Reuse Url's parser for robust percent-decoding.
    if let Ok(parsed) = Url::parse(&format!("http://localhost/?{query}")) {
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => err = Some(v.into_owned()),
                _ => {}
            }
        }
    }

    let ok = code.is_some() && err.is_none();
    let body = if ok {
        "<!doctype html><meta charset=utf-8><title>CI-Applet</title><body style=\"font-family:-apple-system,system-ui,sans-serif;text-align:center;padding:3rem;color:#0d1117\"><h2>✅ Signed in</h2><p>You can close this tab and return to CI-Applet.</p></body>"
    } else {
        "<!doctype html><meta charset=utf-8><title>CI-Applet</title><body style=\"font-family:-apple-system,system-ui,sans-serif;text-align:center;padding:3rem;color:#0d1117\"><h2>Sign-in failed</h2><p>You can close this tab and try again from CI-Applet.</p></body>"
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.flush().await;

    if let Some(e) = err {
        bail!("portal returned an authorization error: {e}");
    }
    let code = code.context("authorization callback did not include a code")?;
    Ok((code, state))
}

// ── Orchestration ───────────────────────────────────────────────────────────

/// Run the full interactive login. `open_browser` is invoked with the
/// authorization URL (the caller opens it in the system browser).
pub async fn login(cfg: &OidcConfig, open_browser: impl Fn(&str)) -> Result<LoginOutcome> {
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building OIDC HTTP client")?;

    let disco = discover(&client, &cfg.issuer).await?;
    let (verifier, challenge) = pkce_pair();
    let state = random_b64url(16);
    let nonce = random_b64url(16);
    let url = authorize_url(&disco, cfg, &state, &nonce, &challenge)?;

    // Bind the listener before opening the browser, then accept with a timeout.
    let listener = bind_loopback(&cfg.redirect_uri).await?;
    open_browser(&url);

    let (code, returned_state) = tokio::time::timeout(LOGIN_TIMEOUT, accept_code(listener))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for the browser sign-in to complete"))??;

    if returned_state.as_deref() != Some(state.as_str()) {
        bail!("OIDC state mismatch (possible CSRF) — aborting sign-in");
    }

    let tokens = exchange_code(&client, &disco, cfg, &code, &verifier).await?;
    let claims = match tokens.id_token.as_deref() {
        Some(jwt) => decode_id_claims(jwt).unwrap_or_default(),
        None => IdClaims::default(),
    };
    Ok(LoginOutcome { tokens, claims })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OidcConfig {
        OidcConfig {
            issuer: "https://hub.companionintelligence.com/api/auth".into(),
            client_id: "ci-applet".into(),
            scopes: "openid profile email offline_access".into(),
            redirect_uri: "http://127.0.0.1:49473/callback".into(),
        }
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let (verifier, challenge) = pkce_pair();
        // Verifier is 32 random bytes → 43 base64url chars.
        assert_eq!(verifier.len(), 43);
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        // No padding, url-safe alphabet.
        assert!(!challenge.contains('=') && !challenge.contains('+') && !challenge.contains('/'));
    }

    #[test]
    fn random_tokens_are_unique() {
        assert_ne!(random_b64url(16), random_b64url(16));
    }

    #[test]
    fn authorize_url_has_all_pkce_params() {
        let disco = Discovery {
            authorization_endpoint: "https://hub.companionintelligence.com/api/auth/oauth2/authorize"
                .into(),
            token_endpoint: "https://x/token".into(),
            jwks_uri: "https://x/jwks".into(),
            userinfo_endpoint: None,
            end_session_endpoint: None,
        };
        let url = authorize_url(&disco, &cfg(), "st4te", "n0nce", "ch4llenge").unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "ci-applet");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:49473/callback");
        assert_eq!(q["scope"], "openid profile email offline_access");
        assert_eq!(q["state"], "st4te");
        assert_eq!(q["code_challenge"], "ch4llenge");
        assert_eq!(q["code_challenge_method"], "S256");
    }

    #[test]
    fn decodes_id_token_claims() {
        // header.payload.signature — payload base64url, no signature check.
        let payload = serde_json::json!({
            "sub": "user_123",
            "email": "a@b.com",
            "name": "Ada",
            "exp": 1999999999i64
        });
        let seg = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let jwt = format!("eyJhbGciOiJFZERTQSJ9.{seg}.sig");
        let claims = decode_id_claims(&jwt).unwrap();
        assert_eq!(claims.sub.as_deref(), Some("user_123"));
        assert_eq!(claims.email.as_deref(), Some("a@b.com"));
        assert_eq!(claims.name.as_deref(), Some("Ada"));
        assert_eq!(claims.exp, Some(1999999999));
    }

    #[test]
    fn token_response_parses_minimal() {
        let t: TokenSet = serde_json::from_str(r#"{"access_token":"a","token_type":"Bearer"}"#).unwrap();
        assert_eq!(t.access_token, "a");
        assert!(t.id_token.is_none() && t.refresh_token.is_none());
    }

    #[test]
    fn token_expiring_window() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let skew = chrono::Duration::minutes(2);
        // Plenty of time left → not expiring.
        assert!(!token_expiring(Some(now + chrono::Duration::minutes(10)), now, skew));
        // Inside the skew window → expiring.
        assert!(token_expiring(Some(now + chrono::Duration::minutes(1)), now, skew));
        // Already past → expiring.
        assert!(token_expiring(Some(now - chrono::Duration::minutes(5)), now, skew));
        // Unknown expiry → don't churn.
        assert!(!token_expiring(None, now, skew));
    }

    #[tokio::test]
    async fn loopback_captures_code_and_state() {
        use tokio::net::TcpStream;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(accept_code(listener));

        // Simulate the browser hitting the redirect URI.
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /callback?code=abc123&state=xyz789&iss=portal HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        let _ = client.read_to_end(&mut resp).await;

        let (code, state) = server.await.unwrap().unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz789"));
        assert!(String::from_utf8_lossy(&resp).contains("Signed in"));
    }

    #[tokio::test]
    async fn loopback_surfaces_provider_error() {
        use tokio::net::TcpStream;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(accept_code(listener));

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /callback?error=access_denied HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        let _ = client.read_to_end(&mut resp).await;

        let err = server.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("access_denied"), "got: {err}");
    }
}
