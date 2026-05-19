//! HTTP delivery for batched file-activity events.
//!
//! Endpoint contract: `POST <portal_origin>/api/history/file-events/batch`
//! with the body shape from
//! [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 4.2.
//! That endpoint does not exist on CI-Server yet — until it ships, the
//! dry-run sink in [`super`] is what you want for on-device testing.

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Url};
use serde::Serialize;

use super::batching::Outcome;
use crate::config::AppConfig;
use crate::server_common::server_origin;
use crate::watcher::types::NormalizedEvent;

const ROUTE_FILE_EVENTS_BATCH: &str = "/api/history/file-events/batch";

#[derive(Debug, Serialize)]
struct EventsBatchBody<'a> {
    device_id: &'a str,
    events: &'a [NormalizedEvent],
}

pub struct HttpSink {
    client: Client,
    url: Url,
    token: String,
    device_id: String,
}

impl HttpSink {
    /// Build an [`HttpSink`] from the active config. Errors if the
    /// applet hasn't been paired (no bearer token) or if the portal URL
    /// is malformed.
    pub fn from_config(config: &AppConfig) -> Result<Self> {
        let token = config
            .server
            .applet_auth_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| anyhow!("applet not paired — no bearer token in config"))?
            .to_string();

        let device_id = config
            .identity
            .device_id
            .map(|u| u.to_string())
            .unwrap_or_else(|| "unknown-device".to_string());

        let url = server_origin(&config.server.portal_url)?
            .join(ROUTE_FILE_EVENTS_BATCH)
            .context("building file-events-batch URL")?;

        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .context("building reqwest client")?;

        Ok(Self { client, url, token, device_id })
    }

    pub async fn deliver(&self, events: &[NormalizedEvent]) -> Outcome {
        if events.is_empty() {
            return Outcome::Success;
        }
        let body = EventsBatchBody {
            device_id: &self.device_id,
            events,
        };

        match self
            .client
            .post(self.url.clone())
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    Outcome::Success
                } else if status.is_server_error() {
                    log::warn!(
                        "sync HTTP {}: {} (transient)",
                        status,
                        status.canonical_reason().unwrap_or("")
                    );
                    Outcome::TransientFailure
                } else {
                    log::warn!("sync HTTP {} (permanent)", status);
                    Outcome::PermanentFailure
                }
            }
            Err(err) => {
                log::warn!("sync transport error: {err}");
                Outcome::TransientFailure
            }
        }
    }
}
