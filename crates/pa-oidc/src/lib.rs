//! OAuth2 Device Authorization Grant (RFC 8628) used by the terminal chat client.
//!
//! Mode-agnostic: the backend either uses an external OIDC provider (`auth_mode = "oidc"`) or runs its own
//! local identity provider (`auth_mode = "local"`). The two device-grant endpoints are NOT
//! derived from the issuer any more - they are read from `GET /api/v1/public/client-config`,
//! which points them at the provider or at the backend itself. The flow (device code -> print
//! url + user code -> poll -> tokens -> refresh) is identical in both modes, because the
//! local token endpoint serves the same two grants as Keycloak's:
//! `urn:ietf:params:oauth:grant-type:device_code` and `refresh_token`.

pub mod tls;

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// The bootstrap config served by `GET /api/v1/public/client-config`.
///
/// This is the single source of truth for authentication endpoints. Clients reject incomplete
/// discovery instead of guessing provider-specific URL layouts.
#[derive(Deserialize, Clone, Debug)]
pub struct ClientConfig {
    pub auth_mode: String,
    pub device_client_id: String,
    pub device_authorization_endpoint: Option<String>,
    pub device_token_endpoint: Option<String>,
}

/// The two endpoints the device grant needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub device_authorization: String,
    pub token: String,
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Server-discovered device-grant configuration.
#[derive(Clone, Debug)]
pub struct Discovery {
    pub auth_mode: String,
    pub endpoints: Endpoints,
    pub device_client_id: String,
}

/// A reqwest error with its whole cause chain, plus a hint when it is a trust failure.
///
/// `reqwest`'s Display is one line ("error sending request for url (...)"); the reason -- the
/// rustls verdict, the DNS error, the connection refused -- is only in `source()`. Printing the
/// top line alone is what turned an internal-CA problem into a hunt for a network fault.
fn describe(err: &reqwest::Error) -> String {
    let mut parts: Vec<String> = vec![err.to_string()];
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
    while let Some(e) = src {
        let text = e.to_string();
        // reqwest wraps hyper wraps rustls: the same sentence can appear at several levels.
        if !parts.iter().any(|p| p == &text) {
            parts.push(text);
        }
        src = std::error::Error::source(e);
    }
    let joined = parts.join(": ");
    if joined.contains("UnknownIssuer") || joined.contains("invalid peer certificate") {
        format!(
            "{joined}\n  hint: the server's certificate is signed by a CA this client does not \
             trust. Install it in the system trust store (update-ca-trust / \
             update-ca-certificates), or point SSL_CERT_FILE at the CA bundle."
        )
    } else {
        joined
    }
}

/// Fetch `GET {server}/api/v1/public/client-config`.
pub async fn fetch_client_config(server: &str) -> Result<ClientConfig> {
    let base = server.trim_end_matches('/');
    let url = format!("{base}/api/v1/public/client-config");
    let cfg: ClientConfig = tls::http_client()
        .get(&url)
        .send()
        .await
        // The cause matters more than the label. "unreachable" covers DNS, connect, TLS and
        // timeout, and an operator reading it goes looking at the network -- when the actual
        // message underneath is usually `invalid peer certificate: UnknownIssuer`, i.e. a CA
        // that is installed on the machine but was not trusted by this binary.
        .map_err(|e| anyhow!("{}", describe(&e)))
        .with_context(|| format!("could not load client-config ({url})"))?
        .error_for_status()
        .with_context(|| format!("client-config failed ({url})"))?
        .json()
        .await
        .with_context(|| format!("client-config is not valid JSON ({url})"))?;
    Ok(cfg)
}

/// Ask the backend for the complete device-grant configuration. No URL or client id is inferred.
pub async fn discover(server: &str) -> Result<Discovery> {
    let cfg = fetch_client_config(server).await?;
    let device_authorization = non_empty(cfg.device_authorization_endpoint.as_deref())
        .ok_or_else(|| anyhow!("the server advertises no device authorization endpoint"))?;
    let token = non_empty(cfg.device_token_endpoint.as_deref())
        .ok_or_else(|| anyhow!("the server advertises no device token endpoint"))?;
    let device_client_id = non_empty(Some(&cfg.device_client_id))
        .ok_or_else(|| anyhow!("the server advertises no device client id"))?;
    Ok(Discovery {
        auth_mode: cfg.auth_mode,
        endpoints: Endpoints {
            device_authorization,
            token,
        },
        device_client_id,
    })
}

/// How the caller shows the pending device authorization to the user, and how it phrases a
/// failure (the TUI localizes both, the agent prints plain text).
/// `Sync` is part of the contract, not an accident: the grant polls across `.await` points, so
/// a caller driving it from a multi-threaded runtime (the desktop window does) needs the prompt
/// to be shareable. Every implementation is a unit struct or holds a handle that already is.
pub trait Prompt: Sync {
    /// Tell the user to open `url` in a browser and confirm `user_code`.
    fn authorize(&self, url: &str, user_code: &str);
    /// The user-facing message for a failed authorization (`error` = the OAuth error code).
    fn failed(&self, error: &str) -> String;
}

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
pub async fn device_login(
    endpoints: &Endpoints,
    client_id: &str,
    prompt: &dyn Prompt,
) -> Result<Tokens> {
    let http = tls::http_client();
    // client_id is sent in BOTH modes: the local provider has no client registry, but it
    // records the id and shows it on the /activate approval screen ("pa-cli wants access").
    let da: DeviceAuthResponse = http
        .post(&endpoints.device_authorization)
        .form(&[("client_id", client_id), ("scope", "openid offline_access")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // The verification URL comes from the RESPONSE in both modes (Keycloak's login page, or
    // the SPA's /activate page) - never constructed here.
    let url = da
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| da.verification_uri.clone());
    prompt.authorize(&url, &da.user_code);

    loop {
        tokio::time::sleep(Duration::from_secs(da.interval)).await;
        let resp = http
            .post(&endpoints.token)
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
            other => bail!("{}", prompt.failed(&format!("{other:?}"))),
        }
    }
}

/// Exchange a refresh token for a fresh access (+ refresh) token.
pub async fn refresh(token_endpoint: &str, client_id: &str, refresh_token: &str) -> Result<Tokens> {
    let resp = tls::http_client()
        .post(token_endpoint)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_server(status_line: &'static str, body: &'static str) -> String {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn discovers_oidc_endpoints_exactly_as_advertised() {
        let server = mock_server(
            "200 OK",
            r#"{"auth_mode":"oidc","device_client_id":"pa-device",
                "device_authorization_endpoint":"https://id.example/device",
                "device_token_endpoint":"https://id.example/token"}"#,
        )
        .await;
        let d = discover(&server).await.unwrap();
        assert_eq!(d.auth_mode, "oidc");
        assert_eq!(d.device_client_id, "pa-device");
        assert_eq!(
            d.endpoints.device_authorization,
            "https://id.example/device"
        );
        assert_eq!(d.endpoints.token, "https://id.example/token");
    }

    #[tokio::test]
    async fn discovers_backend_local_auth_endpoints() {
        let server = mock_server(
            "200 OK",
            r#"{"auth_mode":"local","device_client_id":"pa-device",
                "device_authorization_endpoint":"https://pa.example/api/v1/auth/device/code",
                "device_token_endpoint":"https://pa.example/api/v1/auth/device/token"}"#,
        )
        .await;
        let d = discover(&server).await.unwrap();
        assert_eq!(d.auth_mode, "local");
        assert_eq!(
            d.endpoints.device_authorization,
            "https://pa.example/api/v1/auth/device/code"
        );
        assert_eq!(
            d.endpoints.token,
            "https://pa.example/api/v1/auth/device/token"
        );
    }

    #[tokio::test]
    async fn rejects_incomplete_discovery_instead_of_deriving_urls() {
        for body in [
            r#"{"auth_mode":"oidc","device_client_id":"pa-device","device_token_endpoint":"https://id.example/token"}"#,
            r#"{"auth_mode":"oidc","device_client_id":"pa-device","device_authorization_endpoint":"https://id.example/device"}"#,
            r#"{"auth_mode":"oidc","device_authorization_endpoint":"https://id.example/device","device_token_endpoint":"https://id.example/token"}"#,
        ] {
            let server = mock_server("200 OK", body).await;
            assert!(discover(&server).await.is_err());
        }
    }

    #[tokio::test]
    async fn discovery_endpoint_failure_is_not_bypassed() {
        let server = mock_server("404 Not Found", "{}").await;
        assert!(discover(&server).await.is_err());
    }
}
