//! `--check-server` — a connectivity self-test against the configured
//! CI-Server portal.
//!
//! Run with `cargo run -- --check-server`. It reports, in order:
//! 1. The resolved endpoints (origin, GraphQL, file upload).
//! 2. Local config state (identity + auth credential presence).
//! 3. A live reachability probe: a trivial `{ __typename }` GraphQL query
//!    plus a `GET /health`, with the bearer token attached when present.
//!
//! It never mutates anything on the server — it's safe to run anytime to
//! answer "can this machine actually talk to my CI-Server?".

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;

use crate::config::{AppConfig, ConfigManager};
use crate::server_common::{AuthCredential, graphql_url, server_origin};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(portal_override: Option<String>) -> Result<()> {
    let manager = ConfigManager::new().context("building config manager")?;
    let mut config = manager
        .load_or_default()
        .context("loading persisted configuration")?;
    if let Some(portal) = portal_override {
        config.server.portal_url = portal;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building probe runtime")?;
    runtime.block_on(async { report(&config).await })
}

async fn report(config: &AppConfig) -> Result<()> {
    println!("CI-Applet → CI-Server connection check");
    println!("======================================\n");

    let portal = config.server.portal_url.trim();
    println!("Portal URL : {portal}");

    let origin = match server_origin(portal) {
        Ok(u) => {
            println!("Origin     : {u}");
            Some(u)
        }
        Err(err) => {
            println!("Origin     : ✗ cannot parse portal URL: {err}");
            None
        }
    };
    match graphql_url(portal) {
        Ok(u) => println!("GraphQL    : {u}"),
        Err(err) => println!("GraphQL    : ✗ {err}"),
    }
    if let Some(o) = &origin
        && let Ok(u) = o.join("/api/files/upload")
    {
        println!("File upload: {u}");
    }

    println!("\nLocal config");
    println!("------------");
    println!(
        "user_id    : {}",
        opt(config.identity.user_id.map(|u| u.to_string()))
    );
    println!(
        "device_id  : {}",
        opt(config.identity.device_id.map(|u| u.to_string()))
    );
    let credential = AuthCredential::from_config(config).ok();
    println!(
        "auth cred  : {}",
        credential
            .as_ref()
            .map(|auth| auth.label())
            .unwrap_or("✗ not configured (login or paste API key in settings)")
    );
    println!(
        "file lane  : {}",
        if crate::file_tracker::is_runnable(config) {
            "runnable"
        } else {
            "idle (needs server credential and ≥1 watched folder)"
        }
    );

    println!("\nPortal sign-in (OIDC)");
    println!("---------------------");
    println!("issuer     : {}", config.oidc.issuer);
    println!("client_id  : {}", config.oidc.client_id);
    println!("redirect   : {}", config.oidc.redirect_uri);
    match &config.oidc_session {
        Some(s) => {
            let who = s
                .name
                .clone()
                .or_else(|| s.email.clone())
                .or_else(|| s.subject.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let expiry = s
                .expires_at
                .map(|e| e.to_rfc3339())
                .unwrap_or_else(|| "n/a".to_string());
            println!("signed in  : {who} (token expires {expiry})");
        }
        None => println!("signed in  : ✗ not signed in (browser login not completed)"),
    }

    println!("\nLive probe");
    println!("----------");
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(PROBE_TIMEOUT)
        .build()
        .context("building probe client")?;

    // Health endpoint (unauthenticated).
    if let Some(o) = &origin
        && let Ok(health) = o.join("/health")
    {
        match client.get(health.clone()).send().await {
            Ok(resp) => println!("GET /health           : HTTP {}", resp.status().as_u16()),
            Err(err) => println!("GET /health           : ✗ {}", short_err(&err)),
        }
    }

    // GraphQL reachability (attach bearer token if we have one).
    match graphql_url(portal) {
        Ok(gql) => {
            let mut req = client
                .post(gql)
                .json(&serde_json::json!({ "query": "query{__typename}" }));
            if let Some(auth) = &credential {
                req = auth.apply(req);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    let reachable = body.contains("__typename") || status.is_success();
                    println!(
                        "POST /api/graphql     : HTTP {} {}",
                        status.as_u16(),
                        if reachable {
                            "✓ reachable"
                        } else {
                            "(unexpected body)"
                        }
                    );
                }
                Err(err) => println!("POST /api/graphql     : ✗ {}", short_err(&err)),
            }
        }
        Err(err) => println!("POST /api/graphql     : ✗ {err}"),
    }

    println!("\nNote: browser login uses the OIDC issuer above. A pasted CI-Server");
    println!("API key is sent as x-api-key; JWT/OIDC tokens are sent as bearer auth.");
    Ok(())
}

fn opt(v: Option<String>) -> String {
    v.unwrap_or_else(|| "✗ unset".to_string())
}

fn short_err(err: &reqwest::Error) -> String {
    if err.is_connect() {
        "connection refused / host unreachable".to_string()
    } else if err.is_timeout() {
        "timed out".to_string()
    } else {
        err.to_string()
    }
}
