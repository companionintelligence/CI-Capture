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
    let mut u =
        Url::parse(&disco.authorization_endpoint).context("parsing authorization endpoint")?;
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
    resp.json()
        .await
        .context("parsing refreshed token response")
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
/// Bind the loopback callback listener, returning it alongside the redirect URI
/// that actually applies.
///
/// Port 0 means "let the OS pick", which is the default: a fixed port fails
/// outright if anything else already holds it, and a capture app that cannot
/// sign in because some unrelated process owns 49473 is a bad failure. The
/// Portal validates the redirect per request and accepts any 127.0.0.0/8 port,
/// so the URI legitimately differs on every sign-in — which is exactly why the
/// resolved value has to be threaded through the authorize call *and* the token
/// exchange rather than read from config twice.
async fn bind_loopback(redirect_uri: &str) -> Result<(TcpListener, String)> {
    let mut url = Url::parse(redirect_uri).context("parsing oidc.redirect_uri")?;
    let requested = url
        .port()
        .context("oidc.redirect_uri must include a port, e.g. http://127.0.0.1:0/callback")?;
    let addr = format!("127.0.0.1:{requested}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding loopback {addr} for the OIDC callback"))?;

    if requested == 0 {
        let port = listener
            .local_addr()
            .context("reading the bound loopback port")?
            .port();
        url.set_port(Some(port))
            .map_err(|_| anyhow::anyhow!("could not apply the bound port to the redirect URI"))?;
    }

    Ok((listener, url.to_string()))
}

/// Branded success/error page for the loopback OIDC callback (Capture dark tokens).
fn oidc_callback_page(ok: bool) -> String {
    // Compact CI mark (white paths) — full `ci-mark-white.svg` is too large for
    // an inline success page; this keeps the product silhouette.
    const MARK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="56" height="56" aria-hidden="true"><rect width="64" height="64" rx="14" fill="#0a222e"/><path fill="#abd4d8" d="M18 34c2 8 8 14 16 16 9-1 16-8 17-17h-6c-1 6-5 11-11 12-6-1-10-5-12-11zm28-4c0-7-4-13-10-16l1-5c8 3 14 11 14 21h-5zm-20-9c4-3 9-4 14-2l2-5c-7-3-15-2-21 3l5 4zm8 21c5 0 10-2 13-6l4 3c-5 5-11 8-17 8v-5z"/></svg>"##;

    let (title, lead, detail, accent) = if ok {
        (
            "Signed in",
            "You're connected to Companion Capture.",
            "You can close this tab and return to the app.",
            "#58ebbf",
        )
    } else {
        (
            "Sign-in failed",
            "Companion Capture could not finish signing in.",
            "Close this tab and try again from the app.",
            "#ef7070",
        )
    };

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Companion Capture — {title}</title>
<style>
  :root {{
    --bg: #041620;
    --card: #0a222e;
    --text: #f5f8fa;
    --muted: #8f98a3;
    --primary: #abd4d8;
    --accent: {accent};
    --radius: 0.625rem;
    --font: "Montserrat", system-ui, -apple-system, sans-serif;
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0; min-height: 100vh; display: grid; place-items: center;
    background: var(--bg); color: var(--text); font-family: var(--font);
    background-image: radial-gradient(ellipse at 50% 0%, #0a222e 0%, #041620 55%);
  }}
  main {{
    width: min(22rem, calc(100% - 2rem));
    background: var(--card); border: 1px solid #2c676d; border-radius: var(--radius);
    padding: 2rem 1.5rem 1.75rem; text-align: center;
    box-shadow: 0 12px 40px rgba(0,0,0,0.35);
  }}
  .mark {{ margin: 0 auto 1.25rem; width: 56px; height: 56px; }}
  h1 {{ margin: 0 0 0.5rem; font-size: 1.35rem; font-weight: 600; color: var(--primary); }}
  .lead {{ margin: 0 0 0.35rem; color: var(--text); font-size: 0.95rem; }}
  .detail {{ margin: 0; color: var(--muted); font-size: 0.85rem; line-height: 1.45; }}
  .bar {{ height: 3px; width: 3rem; margin: 1.25rem auto 0; border-radius: 999px; background: var(--accent); }}
</style>
</head>
<body>
<main>
  <div class="mark">{MARK}</div>
  <h1>{title}</h1>
  <p class="lead">{lead}</p>
  <p class="detail">{detail}</p>
  <div class="bar" aria-hidden="true"></div>
</main>
</body>
</html>
"##
    )
}

/// Accept one request on the loopback listener and pull `code` / `state` from
/// the query string, replying with a small "you can close this" page.
async fn accept_code(listener: TcpListener) -> Result<(String, Option<String>)> {
    let (mut sock, _) = listener.accept().await.context("accepting OIDC callback")?;

    let mut buf = vec![0u8; 16 * 1024];
    let n = sock
        .read(&mut buf)
        .await
        .context("reading OIDC callback request")?;
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
    let body = oidc_callback_page(ok);
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
        .no_proxy()
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building OIDC HTTP client")?;

    let disco = discover(&client, &cfg.issuer).await?;
    let (verifier, challenge) = pkce_pair();
    let state = random_b64url(16);
    let nonce = random_b64url(16);

    // Bind first: with an ephemeral port the real redirect URI isn't known until
    // the listener exists, and the authorize call must carry the same value the
    // token exchange will send or the Portal rejects the exchange.
    let (listener, redirect_uri) = bind_loopback(&cfg.redirect_uri).await?;
    let bound = OidcConfig {
        redirect_uri: redirect_uri.clone(),
        ..cfg.clone()
    };
    let url = authorize_url(&disco, &bound, &state, &nonce, &challenge)?;
    open_browser(&url);

    let (code, returned_state) = tokio::time::timeout(LOGIN_TIMEOUT, accept_code(listener))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for the browser sign-in to complete"))??;

    if returned_state.as_deref() != Some(state.as_str()) {
        bail!("OIDC state mismatch (possible CSRF) — aborting sign-in");
    }

    let tokens = exchange_code(&client, &disco, &bound, &code, &verifier).await?;
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

    #[tokio::test]
    async fn ephemeral_port_is_resolved_into_the_redirect_uri() {
        let (listener, uri) = bind_loopback("http://127.0.0.1:0/callback").await.unwrap();
        let bound = listener.local_addr().unwrap().port();

        assert_ne!(bound, 0, "the OS must have assigned a real port");
        // The URI handed back is what goes to the Portal in BOTH the authorize
        // call and the token exchange; a mismatch there is rejected.
        assert_eq!(uri, format!("http://127.0.0.1:{bound}/callback"));
    }

    #[tokio::test]
    async fn an_explicit_port_is_preserved() {
        // Self-hosters may pin a port to match a stricter allowlist.
        let (listener, uri) = bind_loopback("http://127.0.0.1:0/callback").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let explicit = format!("http://127.0.0.1:{port}/callback");
        let (_l, resolved) = bind_loopback(&explicit).await.unwrap();
        assert_eq!(resolved, explicit);
        assert_eq!(uri, explicit);
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
            authorization_endpoint:
                "https://hub.companionintelligence.com/api/auth/oauth2/authorize".into(),
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
        let t: TokenSet =
            serde_json::from_str(r#"{"access_token":"a","token_type":"Bearer"}"#).unwrap();
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
        assert!(!token_expiring(
            Some(now + chrono::Duration::minutes(10)),
            now,
            skew
        ));
        // Inside the skew window → expiring.
        assert!(token_expiring(
            Some(now + chrono::Duration::minutes(1)),
            now,
            skew
        ));
        // Already past → expiring.
        assert!(token_expiring(
            Some(now - chrono::Duration::minutes(5)),
            now,
            skew
        ));
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
