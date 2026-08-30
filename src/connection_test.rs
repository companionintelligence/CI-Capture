//! Inline connection tests for the settings window.
//!
//! Each `test_*` builds a probe from the *current* (possibly unsaved) form
//! config and reports back `Ok(detail)` on success or `Err(friendly message)`.
//! These are read-only reachability/credential probes — they never mutate
//! remote state, so they're safe to run repeatedly from the UI.

use std::time::Duration;

use reqwest::{Client, redirect};

use crate::config::AppConfig;
use crate::server_common::{AuthCredential, friendly_error, graphql_urls, server_origin};

const TIMEOUT: Duration = Duration::from_secs(10);

fn client() -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        // Never follow redirects off the Memory host — Hub edge-SSO 302s to a
        // Hub login HTML page that would otherwise look like HTTP 200 success.
        .redirect(redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn looks_like_html(body: &str, content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| ct.to_ascii_lowercase().contains("text/html"))
        || body.contains("<!DOCTYPE html")
        || body.contains("<html")
}

/// Probe the configured CI-Server: a trivial `{ __typename }` GraphQL query
/// (with the credential attached when present). Success requires a GraphQL
/// body on the Memory origin — not a Hub login page after a redirect.
pub async fn test_ci_server(config: &AppConfig) -> Result<String, String> {
    let portal = config.server.portal_url.trim();
    if portal.is_empty() {
        return Err(rust_i18n::t!("status.check.no_server_address").into());
    }
    let origin = server_origin(portal)
        .map_err(|e| rust_i18n::t!("status.check.invalid_url", error = e).to_string())?;
    let client = client()?;
    let credential = AuthCredential::from_config(config).ok();

    let urls = graphql_urls(portal)
        .map_err(|e| rust_i18n::t!("status.check.invalid_url", error = e).to_string())?;
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
                if status.is_redirection() {
                    return Err(rust_i18n::t!("status.check.edge_sso_intercept").to_string());
                }
                if status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    return Err(rust_i18n::t!(
                        "status.check.credential_rejected",
                        status = status.as_u16()
                    )
                    .to_string());
                }
                let ct = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let body = resp.text().await.unwrap_or_default();
                if looks_like_html(&body, ct.as_deref()) {
                    return Err(rust_i18n::t!("status.check.edge_sso_intercept").to_string());
                }
                if body.contains("__typename") {
                    let auth_note = match &credential {
                        Some(c) => rust_i18n::t!("status.check.auth_accepted", kind = c.label())
                            .to_string(),
                        None => rust_i18n::t!("status.check.no_credential").to_string(),
                    };
                    let reachable =
                        rust_i18n::t!("status.check.reachable", status = status.as_u16());
                    return Ok(format!("{reachable} · {auth_note}"));
                }
                last_err = Some(
                    rust_i18n::t!("status.check.unexpected_response", status = status.as_u16())
                        .to_string(),
                );
            }
            Err(err) => last_err = Some(friendly_error(&err)),
        }
    }

    // Fall back to the health endpoint so a GraphQL route miss still confirms
    // basic reachability — but still refuse redirects / HTML login pages.
    if let Ok(health) = origin.join("/health")
        && let Ok(resp) = client.get(health).send().await
    {
        let status = resp.status();
        if status.is_redirection() {
            return Err(rust_i18n::t!("status.check.edge_sso_intercept").to_string());
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp.text().await.unwrap_or_default();
        if looks_like_html(&body, ct.as_deref()) {
            return Err(rust_i18n::t!("status.check.edge_sso_intercept").to_string());
        }
        if status.is_success() {
            return Ok(
                rust_i18n::t!("status.check.health_fallback", status = status.as_u16()).to_string(),
            );
        }
    }

    Err(last_err.unwrap_or_else(|| rust_i18n::t!("status.check.no_response").to_string()))
}

/// Reachability probe for the vision endpoint. A `GET` on a POST-only path
/// commonly returns 405, which still proves the host is reachable, so any HTTP
/// response counts as success here.
pub async fn test_vision(config: &AppConfig) -> Result<String, String> {
    let endpoint = config.analysis.endpoint.trim();
    if endpoint.is_empty() {
        return Err(rust_i18n::t!("status.check.no_vision_endpoint").into());
    }
    let url = reqwest::Url::parse(endpoint)
        .map_err(|e| rust_i18n::t!("status.check.invalid_endpoint_url", error = e).to_string())?;
    let client = client()?;
    match client.get(url).send().await {
        Ok(resp) => {
            if resp.status().is_redirection() {
                return Err(rust_i18n::t!("status.check.edge_sso_intercept").to_string());
            }
            Ok(rust_i18n::t!(
                "status.check.endpoint_reachable",
                status = resp.status().as_u16()
            )
            .to_string())
        }
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
        return Err(rust_i18n::t!("status.check.no_database_host").into());
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
            let handle = tokio::spawn(async move {
                let _ = connection.await;
            });
            let result = db.query_one("SELECT 1", &[]).await;
            handle.abort();
            match result {
                Ok(_) => {
                    let target = format!("{}@{}:{}/{}", pg.user, pg.host, pg.port, pg.database);
                    Ok(
                        rust_i18n::t!("status.check.postgres_connected", target = target)
                            .to_string(),
                    )
                }
                Err(e) => {
                    Err(rust_i18n::t!("status.check.postgres_query_failed", error = e).to_string())
                }
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
        Ok(_) => Ok(rust_i18n::t!(
            "status.check.bucket_reachable",
            bucket = config.minio.bucket
        )
        .to_string()),
        Err(e) => Err(format!("{e}")),
    }
}
