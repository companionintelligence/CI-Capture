use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::server_common::{graphql_url, server_origin};

#[derive(Debug, Clone)]
pub struct PairingLaunch {
    pub pairing_token: String,
    pub poll_key: String,
    pub pairing_url: String,
}

#[derive(Debug, Clone)]
pub struct PairingCompletion {
    pub user_id: Uuid,
    pub user_name: Option<String>,
    pub user_email: Option<String>,
    pub device_id: Uuid,
    pub applet_auth_token: String,
}

#[derive(Debug, Serialize)]
struct GraphqlRequest<TVariables> {
    query: &'static str,
    variables: TVariables,
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse<TData> {
    data: Option<TData>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: Option<String>,
}


#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartPairingEnvelope {
    start_pairing: PairingStartPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingStartPayload {
    success: bool,
    pairing_token: Option<String>,
    poll_key: Option<String>,
    pairing_path: Option<String>,
    error_message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingStatusVariables {
    pairing_token: String,
    poll_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingStatusEnvelope {
    pairing_status: PairingStatusPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingStatusPayload {
    success: bool,
    status: String,
    user_id: Option<String>,
    user_name: Option<String>,
    user_email: Option<String>,
    device_id: Option<String>,
    applet_access_token: Option<String>,
    error_message: Option<String>,
}

const START_PAIRING_MUTATION: &str = r#"
mutation StartPairing {
  startPairing {
    success
    pairingToken
    pollKey
    pairingPath
    errorMessage
  }
}
"#;

const PAIRING_STATUS_QUERY: &str = r#"
query PairingStatus($pairingToken: String!, $pollKey: String!) {
  pairingStatus(pairingToken: $pairingToken, pollKey: $pollKey) {
    success
    status
    userId
    userName
    userEmail
    deviceId
    appletAccessToken
    errorMessage
  }
}
"#;

pub async fn start_pairing(portal_url: &str) -> Result<PairingLaunch> {
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("building GraphQL client")?;
    let graphql_url = graphql_url(portal_url)?;
    let response = graphql::<serde_json::Value, StartPairingEnvelope>(
        &client,
        graphql_url,
        GraphqlRequest {
            query: START_PAIRING_MUTATION,
            variables: serde_json::Value::Null,
        },
    )
    .await?;

    let payload = response.start_pairing;
    if !payload.success {
        return Err(anyhow!(
            "{}",
            payload
                .error_message
                .unwrap_or_else(|| "Unable to start portal pairing.".to_string())
        ));
    }

    let pairing_token = payload
        .pairing_token
        .context("pairing response missing pairing token")?;
    let poll_key = payload
        .poll_key
        .context("pairing response missing poll key")?;
    let pairing_path = payload
        .pairing_path
        .context("pairing response missing pairing path")?;
    let pairing_url = pairing_url(portal_url, &pairing_path)?;

    Ok(PairingLaunch {
        pairing_token,
        poll_key,
        pairing_url,
    })
}

pub async fn wait_for_pairing(
    portal_url: &str,
    launch: &PairingLaunch,
    timeout: Duration,
) -> Result<PairingCompletion> {
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("building GraphQL client")?;
    let graphql_url = graphql_url(portal_url)?;
    let started = Instant::now();

    loop {
        if started.elapsed() > timeout {
            return Err(anyhow!("Timed out waiting for portal pairing to complete."));
        }

        let response = graphql::<_, PairingStatusEnvelope>(
            &client,
            graphql_url.clone(),
            GraphqlRequest {
                query: PAIRING_STATUS_QUERY,
                variables: PairingStatusVariables {
                    pairing_token: launch.pairing_token.clone(),
                    poll_key: launch.poll_key.clone(),
                },
            },
        )
        .await?;

        let payload = response.pairing_status;
        match payload.status.as_str() {
            "PENDING" => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            "COMPLETED" => {
                let user_id = payload
                    .user_id
                    .as_deref()
                    .context("pairing completed without a user id")
                    .and_then(|value| {
                        Uuid::parse_str(value).context("parsing paired user id from GraphQL")
                    })?;
                let device_id = payload
                    .device_id
                    .as_deref()
                    .context("pairing completed without a device id")
                    .and_then(|value| {
                        Uuid::parse_str(value).context("parsing paired device id from GraphQL")
                    })?;
                let applet_auth_token = payload
                    .applet_access_token
                    .filter(|value| !value.trim().is_empty())
                    .context("pairing completed without an applet access token")?;

                return Ok(PairingCompletion {
                    user_id,
                    user_name: payload.user_name,
                    user_email: payload.user_email,
                    device_id,
                    applet_auth_token,
                });
            }
            "EXPIRED" | "NOT_FOUND" => {
                return Err(anyhow!(
                    "{}",
                    payload
                        .error_message
                        .unwrap_or_else(|| "Portal pairing is no longer available.".to_string())
                ));
            }
            other => {
                if !payload.success {
                    return Err(anyhow!(
                        "{}",
                        payload.error_message.unwrap_or_else(|| {
                            format!("Portal pairing failed with status {other}.")
                        })
                    ));
                }
                return Err(anyhow!("Unexpected pairing status: {other}"));
            }
        }
    }
}

async fn graphql<TVariables, TData>(
    client: &Client,
    url: Url,
    request: GraphqlRequest<TVariables>,
) -> Result<TData>
where
    TVariables: Serialize,
    TData: for<'de> Deserialize<'de>,
{
    let response = client
        .post(url)
        .json(&request)
        .send()
        .await
        .context("sending GraphQL request")?;

    let status = response.status();
    let body: GraphqlResponse<TData> =
        response.json().await.context("decoding GraphQL response")?;

    if !status.is_success() {
        let message = body
            .errors
            .as_ref()
            .and_then(|errors| errors.first())
            .and_then(|error| error.message.clone())
            .unwrap_or_else(|| format!("GraphQL request failed with HTTP {status}."));
        return Err(anyhow!(message));
    }

    if let Some(error) = body
        .errors
        .as_ref()
        .and_then(|errors| errors.first())
        .and_then(|error| error.message.clone())
    {
        return Err(anyhow!(error));
    }

    body.data.context("GraphQL response missing data")
}

fn pairing_url(portal_url: &str, pairing_path: &str) -> Result<String> {
    let url = server_origin(portal_url)?
        .join(pairing_path)
        .with_context(|| format!("building pairing URL for path {pairing_path}"))?;
    Ok(url.to_string())
}
