//! Best-effort CI-Server URL discovery from CI-Portal after browser sign-in.
//!
//! Portal's `GET /api/devices` lists each Hub device and its provisioned apps
//! (including `ci-memory` — the CI-Server that receives applet captures). When
//! the session cookie / bearer is accepted, we pick the first `ci-memory` app
//! URL so onboarding can autofill the CI-Server field.

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

/// App slugs Portal uses for Companion Memory / CI-Server.
const CI_MEMORY_SLUGS: &[&str] = &["ci-memory", "ci-server"];

#[derive(Debug, Deserialize)]
struct DevicesResponse {
    #[serde(default)]
    devices: Vec<DeviceRow>,
}

#[derive(Debug, Deserialize)]
struct DeviceRow {
    #[serde(default)]
    apps: Vec<AppRow>,
}

#[derive(Debug, Deserialize)]
struct AppRow {
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// Strip a trailing `/api/auth` (and any path) from an OIDC issuer to get the
/// portal origin used for REST calls.
pub fn portal_origin_from_issuer(issuer: &str) -> Result<String> {
    let trimmed = issuer.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("empty OIDC issuer");
    }
    let without_auth = trimmed
        .strip_suffix("/api/auth")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let url = reqwest::Url::parse(without_auth)
        .with_context(|| format!("parsing portal origin from issuer {issuer}"))?;
    Ok(format!(
        "{}://{}",
        url.scheme(),
        url.host_str().context("issuer missing host")?
    ))
}

fn is_ci_memory_app(app: &AppRow) -> bool {
    let slug = app.slug.as_deref().unwrap_or("").to_ascii_lowercase();
    if CI_MEMORY_SLUGS.iter().any(|s| slug == *s || slug.starts_with(&format!("{s}-"))) {
        return true;
    }
    let name = app.name.as_deref().unwrap_or("").to_ascii_lowercase();
    name.contains("ci memory") || name.contains("companion memory") || name == "ci server"
}

/// Ask Portal for the user's devices and return the first CI-Server (`ci-memory`)
/// public URL, if any. Failures are soft — callers leave the field for manual entry.
pub async fn discover_ci_server_url(
    issuer: &str,
    bearer: Option<&str>,
) -> Result<Option<String>> {
    let origin = portal_origin_from_issuer(issuer)?;
    let url = format!("{origin}/api/devices");
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(20))
        .build()
        .context("building portal discovery client")?;

    let mut req = client.get(&url).header("accept", "application/json");
    if let Some(token) = bearer.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("portal devices → HTTP {}", resp.status());
    }

    let body: DevicesResponse = resp.json().await.context("parsing /api/devices")?;
    for device in &body.devices {
        for app in &device.apps {
            if is_ci_memory_app(app)
                && let Some(app_url) = app.url.as_deref().map(str::trim).filter(|u| !u.is_empty())
            {
                return Ok(Some(app_url.trim_end_matches('/').to_string()));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_api_auth_suffix() {
        assert_eq!(
            portal_origin_from_issuer("https://hub.companionintelligence.com/api/auth").unwrap(),
            "https://hub.companionintelligence.com"
        );
        assert_eq!(
            portal_origin_from_issuer("https://hub.companionintelligence.com/api/auth/").unwrap(),
            "https://hub.companionintelligence.com"
        );
    }

    #[test]
    fn matches_ci_memory_slug() {
        let app = AppRow {
            slug: Some("ci-memory".into()),
            name: Some("CI Memory".into()),
            url: Some("https://ci-memory-core-acme.companionintelligence.com".into()),
        };
        assert!(is_ci_memory_app(&app));
    }
}
