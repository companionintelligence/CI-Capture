use anyhow::{Context, Result};
use reqwest::{RequestBuilder, Url};

use crate::config::AppConfig;

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

pub fn graphql_url(portal_url: &str) -> Result<Url> {
    server_origin(portal_url)?
        .join("/api/graphql")
        .context("building GraphQL endpoint URL")
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
    use super::AuthCredential;

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
}
