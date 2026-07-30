//! Inline connection tests for the settings window.
//!
//! Each `test_*` builds a probe from the *current* (possibly unsaved) form
//! config and reports back `Ok(detail)` on success or `Err(friendly message)`.
//! These are read-only reachability/credential probes — they never mutate
//! remote state, so they're safe to run repeatedly from the UI.

use std::time::Duration;

use reqwest::Client;

use crate::config::AppConfig;
use crate::server_common::{AuthCredential, friendly_error, graphql_urls, server_origin};

const TIMEOUT: Duration = Duration::from_secs(10);

fn client() -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

/// Probe the configured CI-Server: a `GET /health` plus a trivial
/// `{ __typename }` GraphQL query (with the credential attached when present).
pub async fn test_ci_server(config: &AppConfig) -> Result<String, String> {
    let portal = config.server.portal_url.trim();
    if portal.is_empty() {
        return Err("Add your Core's address first.".into());
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

    Err(last_err.unwrap_or_else(|| "Your Core didn't respond on any known address.".into()))
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
        Ok(resp) => Ok(format!(
            "Endpoint reachable · HTTP {}",
            resp.status().as_u16()
        )),
        Err(err) => Err(friendly_error(&err)),
    }
}

/// Open a real Postgres connection and run `SELECT 1`. Connects without TLS,
/// matching the recorder's pool, so the "Require SSL" toggle is not exercised
/// by this probe.
///
/// Only meaningful alongside the direct-Postgres writer, so it shares that
/// path's `legacy-postgres` gate rather than offering to probe a database the
/// default build can never write to.
#[cfg(feature = "legacy-postgres")]
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

