//! Post-sign-in Core discovery against CI-Portal.
//!
//! After the OIDC flow completes, the app asks the Portal *where the user's
//! Companion Memory runs* so the Core address configures itself instead of
//! being typed. This is the difference between a fresh install that works and
//! one that asks for a hostname the user has to go look up.
//!
//! A direct port of CI-Browser-Extension's `src/services/portalApps.ts`, so the
//! two capture clients behave identically and a Portal-side change only has to
//! be reasoned about once.
//!
//! **The credential is the opaque `access_token`, not the `id_token.**` This is
//! the single easiest thing to get backwards here: `/api/users/me/apps` is a
//! *Portal* API guarded by `oauthBearerMiddleware`, which looks the opaque token
//! up in `oauth_access_token` and rejects anything whose `clientId` is not in
//! `PUBLIC_CLIENTS`. Every other authenticated call this app makes goes to
//! CI-Server and uses the `id_token`. Swapping them yields a 401 that looks
//! exactly like a sign-in problem.

use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

use crate::server_common::normalize_core_url;

/// The marketplace slug for Companion Memory (CI-Server).
pub const MEMORY_APP_SLUG: &str = "ci-memory";

/// Discovery is a convenience, never a gate on sign-in — keep it short.
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalApp {
    pub slug: String,
    pub url: String,
    pub device_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AppsResponse {
    #[serde(default)]
    apps: Vec<RawApp>,
}

#[derive(Debug, Deserialize)]
struct RawApp {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "deviceName")]
    device_name: Option<String>,
}

/// Outcome of a discovery attempt.
///
/// Deliberately a discriminated result rather than `Result<Vec<_>>`: the caller
/// has to treat "this Portal is too old to know" differently from "your token
/// was refused", and collapsing them into one error would make the app fall
/// back to manual entry while telling the user their sign-in failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// The Portal answered. May legitimately be empty (no Core provisioned).
    Ok(Vec<PortalApp>),
    /// 404/405 — a Portal predating `/api/users/me/apps`. Fall back quietly.
    Unsupported,
    /// 401/403 — the token was refused. Usually means `ci-applet` is not
    /// registered in the Portal's `PUBLIC_CLIENTS`.
    Unauthorized,
    /// Network failure, timeout, or an unparseable body.
    Error(String),
}

/// Build the discovery URL for `slug`.
pub fn me_apps_url(portal_url: &str, slug: &str) -> Result<Url, String> {
    let base = portal_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("no portal URL configured".into());
    }
    let mut url = Url::parse(&format!("{base}/api/users/me/apps"))
        .map_err(|e| format!("invalid portal URL: {e}"))?;
    url.query_pairs_mut().append_pair("slug", slug);
    Ok(url)
}

/// Ask the Portal which Companion Memory instances this user can reach.
pub async fn fetch_portal_apps(portal_url: &str, access_token: &str, slug: &str) -> Discovery {
    let url = match me_apps_url(portal_url, slug) {
        Ok(url) => url,
        Err(e) => return Discovery::Error(e),
    };

    let client = match Client::builder()
        // Same reasoning as every other client in this crate: an ambient proxy
        // must not be able to see the user's access token.
        .no_proxy()
        .timeout(TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => return Discovery::Error(format!("building http client: {e}")),
    };

    let response = match client.get(url).bearer_auth(access_token).send().await {
        Ok(r) => r,
        Err(e) => return Discovery::Error(crate::server_common::friendly_error(&e)),
    };

    match response.status() {
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => return Discovery::Unsupported,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Discovery::Unauthorized,
        s if !s.is_success() => return Discovery::Error(format!("HTTP {}", s.as_u16())),
        _ => {}
    }

    match response.json::<AppsResponse>().await {
        Ok(body) => Discovery::Ok(
            body.apps
                .into_iter()
                .map(|a| PortalApp {
                    slug: a.slug,
                    url: a.url,
                    device_name: a.device_name,
                })
                .collect(),
        ),
        Err(e) => Discovery::Error(format!("unreadable response: {e}")),
    }
}

/// Reduce a raw app list to reachable Companion Memory candidates: right slug,
/// normalized URL, deduplicated.
///
/// One candidate means the Core can be configured without asking. Several means
/// Memory is installed on more than one machine and the user has to choose —
/// picking for them would silently stream a laptop's captures to the wrong box.
pub fn memory_candidates(apps: &[PortalApp]) -> Vec<PortalApp> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for app in apps {
        if app.slug != MEMORY_APP_SLUG {
            continue;
        }
        let Ok(url) = normalize_core_url(&app.url) else {
            continue;
        };
        if seen.contains(&url) {
            continue;
        }
        seen.push(url.clone());
        out.push(PortalApp {
            slug: app.slug.clone(),
            url,
            device_name: app.device_name.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(slug: &str, url: &str, device: Option<&str>) -> PortalApp {
        PortalApp {
            slug: slug.into(),
            url: url.into(),
            device_name: device.map(Into::into),
        }
    }

    #[test]
    fn builds_the_discovery_url_with_an_encoded_slug() {
        let url = me_apps_url("https://hub.ci.computer", MEMORY_APP_SLUG).unwrap();
        assert_eq!(
            url.as_str(),
            "https://hub.ci.computer/api/users/me/apps?slug=ci-memory"
        );
        // A trailing slash on the configured portal must not double up.
        let url = me_apps_url("https://hub.ci.computer/", MEMORY_APP_SLUG).unwrap();
        assert_eq!(
            url.as_str(),
            "https://hub.ci.computer/api/users/me/apps?slug=ci-memory"
        );
    }

    #[test]
    fn rejects_an_unset_portal() {
        assert!(me_apps_url("", MEMORY_APP_SLUG).is_err());
        assert!(me_apps_url("   ", MEMORY_APP_SLUG).is_err());
    }

    #[test]
    fn keeps_only_memory_apps() {
        let apps = [
            app("ci-memory", "http://core-1:8642", Some("Core")),
            app("ci-web-xr-scan", "http://scan:3000", None),
        ];
        let out = memory_candidates(&apps);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].device_name.as_deref(), Some("Core"));
    }

    #[test]
    fn deduplicates_by_normalized_url() {
        // The same Core advertised twice with cosmetic differences must not
        // present as a choice between two machines.
        let apps = [
            app("ci-memory", "http://core-1:8642", Some("Core")),
            app("ci-memory", "http://core-1:8642/", Some("Core (dup)")),
        ];
        assert_eq!(memory_candidates(&apps).len(), 1);
    }

    #[test]
    fn keeps_genuinely_distinct_cores_so_the_user_can_choose() {
        let apps = [
            app("ci-memory", "http://core-1:8642", Some("Desk")),
            app("ci-memory", "http://core-2.tail.ts.net:8642", Some("Shed")),
        ];
        assert_eq!(memory_candidates(&apps).len(), 2);
    }

    #[test]
    fn drops_entries_with_an_unusable_url() {
        let apps = [
            app("ci-memory", "not a url", None),
            app("ci-memory", "", None),
            app("ci-memory", "http://core-1:8642", None),
        ];
        assert_eq!(memory_candidates(&apps).len(), 1);
    }
}
