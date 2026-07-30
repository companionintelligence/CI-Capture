//! Inline connection tests for the settings window and the onboarding wizard.
//!
//! Each `test_*` builds a probe from the *current* (possibly unsaved) form
//! config and reports back `Ok(detail)` on success or `Err(friendly message)`.
//! All of them are read-only reachability/credential probes, safe to run
//! repeatedly from the UI — **except [`test_capture`]**, which deliberately
//! uploads one real screenshot so setup can end on *verified* rather than
//! *configured*. It only ever runs from an explicit button press.

use std::time::Duration;

use reqwest::Client;

use crate::config::AppConfig;
use crate::server_common::{AuthCredential, friendly_error, graphql_urls, server_origin};

const TIMEOUT: Duration = Duration::from_secs(10);

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

/// Probe the configured CI-Server: a `GET /health` plus a trivial
/// `{ __typename }` GraphQL query (with the credential attached when present).
pub async fn test_ci_server(config: &AppConfig) -> Result<String, String> {
    let portal = config.server.portal_url.trim();
    if portal.is_empty() {
        return Err("Set a CI-Server URL first.".into());
    }
    let origin = server_origin(portal).map_err(|e| format!("Invalid URL: {e}"))?;
    let client = client()?;
    let credential = AuthCredential::from_config(config).ok();

    let urls = graphql_urls(portal).map_err(|e| e.to_string())?;
    let mut last_err: Option<String> = None;
    for gql in urls {
        let mut req = client
            .post(gql.clone())
            .json(&serde_json::json!({ "query": "query{__typename}" }));
        if let Some(auth) = &credential {
            req = auth.apply(req);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    return Err(format!(
                        "Reached the server, but the credential was rejected (HTTP {}).",
                        status.as_u16()
                    ));
                }
                let body = resp.text().await.unwrap_or_default();
                if body.contains("__typename") || status.is_success() {
                    let auth_note = match &credential {
                        Some(c) => format!(" · {} auth accepted", c.label()),
                        None => " · no credential sent".to_string(),
                    };
                    return Ok(format!("Reachable · HTTP {}{auth_note}", status.as_u16()));
                }
                last_err = Some(format!("HTTP {} (unexpected response)", status.as_u16()));
            }
            Err(err) => last_err = Some(friendly_error(&err)),
        }
    }

    // Fall back to the health endpoint so a GraphQL route miss still confirms
    // basic reachability.
    if let Ok(health) = origin.join("/health") {
        if let Ok(resp) = client.get(health).send().await {
            return Ok(format!(
                "Server reachable · GET /health HTTP {} (GraphQL route not found)",
                resp.status().as_u16()
            ));
        }
    }

    Err(last_err.unwrap_or_else(|| "No CI-Server endpoint responded.".into()))
}

/// Probe a candidate OIDC issuer by fetching its discovery document.
///
/// This is what lets onboarding ask for one URL instead of two: the wizard
/// derives `<server>/api/auth`, calls this, and only surfaces the manual issuer
/// field when the probe fails. Read-only — discovery is a public GET.
pub async fn test_oidc_issuer(issuer: &str) -> Result<String, String> {
    let issuer = issuer.trim();
    if issuer.is_empty() {
        return Err("Set a portal sign-in URL first.".into());
    }
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;

    match crate::oidc::discover(&client, issuer).await {
        Ok(disco) => {
            let host = reqwest::Url::parse(&disco.authorization_endpoint)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| issuer.to_string());
            Ok(format!("Sign-in provider found at {host}"))
        }
        Err(err) => Err(crate::oidc::friendly_login_error(&err)),
    }
}

/// End-to-end proof that capture actually reaches CI-Server: take one real
/// screenshot and push it through the **production** ingest path.
///
/// Onboarding previously declared success from stored-credential + permission
/// state, which cannot see the failure that matters most — a token the server
/// stores fine but rejects at ingest (audience mismatch, revoked grant, wrong
/// mount path). Those surfaced hours later as log warnings. Running the real
/// upload while the user is still in the wizard turns "configured" into
/// "verified".
///
/// Side effect, and it is the point: this uploads one genuine screenshot of the
/// user's screen, exactly as normal capture would. The UI says so before it
/// runs.
pub async fn test_capture(config: &AppConfig) -> Result<String, String> {
    if config.server.portal_url.trim().is_empty() {
        return Err("Set a CI-Server URL first.".into());
    }
    if !crate::server_ingest::is_configured(config) {
        return Err("Sign in or paste an API key first.".into());
    }

    let image = tokio::task::spawn_blocking(crate::capture::capture_primary)
        .await
        .map_err(|e| format!("capture thread failed: {e}"))?
        .map_err(|e| format!("Couldn't capture the screen: {e}"))?;

    let png = image
        .to_png_bytes()
        .map_err(|e| format!("Couldn't encode the screenshot: {e}"))?;
    let kb = png.len() as f64 / 1024.0;

    // A blank frame means the OS grant is missing, not that ingest is broken —
    // uploading it would "pass" while capturing nothing. Catch it here.
    if is_uniform(&image) {
        return Err(
            "The captured frame is blank — macOS Screen Recording permission is missing. \
             Grant it on the previous step, restart the app, and try again."
                .into(),
        );
    }

    let filename = format!("ci-applet-setup-test-{}.png", chrono::Utc::now().format("%Y%m%dT%H%M%S"));
    // Mirror the fresh-install production path: no local analysis, so the
    // server runs its own description (inventory on, skip off).
    match crate::server_ingest::ingest_capture(config, &filename, &png, true, false, None).await {
        Ok(file_id) => Ok(format!(
            "Test capture accepted · {kb:.0} KB · server file {file_id}"
        )),
        Err(err) => Err(friendly_ingest_error(&err)),
    }
}

/// True when every pixel is identical — the signature of a permission-blocked
/// capture (macOS hands back a uniform frame rather than failing).
fn is_uniform(image: &crate::capture::CapturedImage) -> bool {
    let mut chunks = image.data.chunks_exact(4);
    let Some(first) = chunks.next() else {
        return true;
    };
    chunks.all(|px| px == first)
}

/// Turn an ingest failure into the specific thing to fix. The upload lane's
/// errors carry the HTTP status, which is the whole diagnosis here.
fn friendly_ingest_error(err: &anyhow::Error) -> String {
    let chain = err
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    let lower = chain.to_ascii_lowercase();
    if lower.contains("http 401") || lower.contains("http 403") {
        "The server rejected the credential at upload. If you signed in, the token's audience \
         may not be one CI-Server accepts — run `--login` and check `token aud` against the \
         server's PORTAL_OIDC_AUDIENCE."
            .to_string()
    } else if lower.contains("http 404") || lower.contains("http 405") {
        "Reached the server, but no upload endpoint answered — check the CI-Server URL points \
         at the API root."
            .to_string()
    } else if lower.contains("http 413") {
        "The server rejected the screenshot as too large.".to_string()
    } else if lower.contains("certificate") {
        "TLS certificate rejected — use a trusted CI-Server URL, or install its certificate."
            .to_string()
    } else if lower.contains("connection refused") || lower.contains("dns error") {
        "Couldn't reach CI-Server — check the URL and your network.".to_string()
    } else {
        format!("Upload failed: {}", chain.rsplit(": ").next().unwrap_or(&chain))
    }
}

/// Reachability probe for the vision endpoint. A `GET` on a POST-only path
/// commonly returns 405, which still proves the host is reachable, so any HTTP
/// response counts as success here.
pub async fn test_vision(config: &AppConfig) -> Result<String, String> {
    let endpoint = config.analysis.endpoint.trim();
    if endpoint.is_empty() {
        return Err("Set a vision endpoint first.".into());
    }
    let url = reqwest::Url::parse(endpoint).map_err(|e| format!("Invalid endpoint URL: {e}"))?;
    let client = client()?;
    match client.get(url).send().await {
        Ok(resp) => Ok(format!("Endpoint reachable · HTTP {}", resp.status().as_u16())),
        Err(err) => Err(friendly_error(&err)),
    }
}

/// Open a real Postgres connection and run `SELECT 1`. Connects without TLS,
/// matching the recorder's pool, so the "Require SSL" toggle is not exercised
/// by this probe.
pub async fn test_postgres(config: &AppConfig) -> Result<String, String> {
    use tokio_postgres::{Config, NoTls};

    let pg = &config.postgres;
    if pg.host.trim().is_empty() {
        return Err("Set a database host first.".into());
    }

    let mut cfg = Config::new();
    cfg.host(&pg.host)
        .port(pg.port)
        .user(&pg.user)
        .password(&pg.password)
        .dbname(&pg.database)
        .connect_timeout(TIMEOUT);

    match cfg.connect(NoTls).await {
        Ok((db, connection)) => {
            // Drive the connection just long enough to run the query.
            let handle = tokio::spawn(async move {
                let _ = connection.await;
            });
            let result = db.query_one("SELECT 1", &[]).await;
            handle.abort();
            match result {
                Ok(_) => Ok(format!(
                    "Connected · {}@{}:{}/{} · SELECT 1 ok",
                    pg.user, pg.host, pg.port, pg.database
                )),
                Err(e) => Err(format!("Connected, but query failed: {e}")),
            }
        }
        Err(e) => Err(format!("{e}")),
    }
}

/// Test the object store. With the `legacy-minio` feature this performs a full
/// S3 client + bucket check (the same path the legacy uploader uses); otherwise
/// it falls back to an HTTP reachability probe of the endpoint.
#[cfg(feature = "legacy-minio")]
pub async fn test_minio(config: &AppConfig) -> Result<String, String> {
    match crate::minio::MinioUploader::new(&config.minio).await {
        Ok(_) => Ok(format!("Bucket '{}' reachable", config.minio.bucket)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(feature = "legacy-minio"))]
pub async fn test_minio(config: &AppConfig) -> Result<String, String> {
    let raw = config.minio.endpoint.trim();
    if raw.is_empty() {
        return Err("Set a storage endpoint first.".into());
    }
    let endpoint = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        let scheme = if config.minio.use_ssl { "https" } else { "http" };
        format!("{scheme}://{raw}")
    };
    let url = reqwest::Url::parse(&endpoint).map_err(|e| format!("Invalid endpoint URL: {e}"))?;
    let client = client()?;
    match client.get(url).send().await {
        // MinIO answers the root with 403/400; any HTTP reply proves reachability.
        Ok(resp) => Ok(format!(
            "Endpoint reachable · HTTP {} · full bucket check needs the legacy-minio build",
            resp.status().as_u16()
        )),
        Err(err) => Err(friendly_error(&err)),
    }
}

#[cfg(test)]
mod tests {
    use super::{friendly_ingest_error, is_uniform};
    use crate::capture::CapturedImage;

    #[test]
    fn uniform_frame_is_detected() {
        // A permission-blocked macOS capture comes back as one flat colour.
        let blank = CapturedImage {
            width: 2,
            height: 1,
            data: vec![0, 0, 0, 255, 0, 0, 0, 255],
        };
        assert!(is_uniform(&blank));

        let real = CapturedImage {
            width: 2,
            height: 1,
            data: vec![0, 0, 0, 255, 12, 34, 56, 255],
        };
        assert!(!is_uniform(&real));
    }

    #[test]
    fn empty_frame_counts_as_uniform() {
        let empty = CapturedImage {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
        assert!(is_uniform(&empty));
    }

    #[test]
    fn ingest_401_points_at_the_audience() {
        let err = anyhow::anyhow!("capture upload failed with HTTP 401 Unauthorized");
        let msg = friendly_ingest_error(&err);
        assert!(msg.contains("PORTAL_OIDC_AUDIENCE"), "got: {msg}");
    }

    #[test]
    fn ingest_404_points_at_the_url() {
        let err = anyhow::anyhow!("capture upload failed with HTTP 404 Not Found");
        assert!(friendly_ingest_error(&err).contains("CI-Server URL"));
    }
}
