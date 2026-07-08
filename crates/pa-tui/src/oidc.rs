//! OAuth2 Device Authorization Grant (RFC 8628) against Keycloak — the same flow the
//! device-agent uses. The TUI prints a URL + code, the user authorizes in a browser,
//! and we poll for tokens. The access token is then sent as `Authorization: Bearer`
//! on every REST/SSE call (and as the `bearer,<jwt>` subprotocol on the control WS).

use std::time::Duration;

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::i18n::{t, Msg};

#[derive(Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

/// Run the full device-code login flow; blocks until the user authorizes (or it errors).
pub async fn device_login(issuer: &str, client_id: &str) -> Result<Tokens> {
    let http = reqwest::Client::new();
    let da: DeviceAuthResponse = http
        .post(format!("{issuer}/protocol/openid-connect/auth/device"))
        .form(&[("client_id", client_id), ("scope", "openid offline_access")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let url = da
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| da.verification_uri.clone());
    eprintln!("{}", t(Msg::OidcOpen));
    eprintln!("    {url}");
    eprintln!("{}", t(Msg::OidcCode(&da.user_code)));
    eprintln!("{}", t(Msg::OidcWaiting));

    loop {
        tokio::time::sleep(Duration::from_secs(da.interval)).await;
        let resp = http
            .post(format!("{issuer}/protocol/openid-connect/token"))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", da.device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(resp.json::<Tokens>().await?);
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        match body.get("error").and_then(|e| e.as_str()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => tokio::time::sleep(Duration::from_secs(5)).await,
            other => bail!("{}", t(Msg::OidcFailed(&format!("{other:?}")))),
        }
    }
}

/// Exchange a refresh token for a fresh access (+ refresh) token.
pub async fn refresh(issuer: &str, client_id: &str, refresh_token: &str) -> Result<Tokens> {
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{issuer}/protocol/openid-connect/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json::<Tokens>().await?)
}
