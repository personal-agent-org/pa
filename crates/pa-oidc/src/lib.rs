//! OAuth2 Device Authorization Grant (RFC 8628) - shared by the device agent (`pa-agent`)
//! and the terminal UI (`pa-tui`).
//!
//! Mode-agnostic: the backend either fronts Keycloak (`auth_mode = "oidc"`) or runs its own
//! local identity provider (`auth_mode = "local"`). The two device-grant endpoints are NOT
//! derived from the issuer any more - they are read from `GET /api/v1/public/client-config`,
//! which points them at Keycloak or at the backend itself. The flow (device code -> print
//! url + user code -> poll -> tokens -> refresh) is identical in both modes, because the
//! local token endpoint serves the same two grants as Keycloak's:
//! `urn:ietf:params:oauth:grant-type:device_code` and `refresh_token`.

pub mod tls;

/// The compiled-in device client id.
///
/// It exists in the shipped Keycloak realm-as-code and nowhere else, so it is a LAST resort:
/// what the server advertises comes first. Public here so the CLI can tell "the user chose
/// this" apart from "nobody chose anything".
pub const DEFAULT_DEVICE_CLIENT_ID: &str = "personal-agent-device";

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// The bootstrap config served by `GET /api/v1/public/client-config`.
///
/// Every field is optional-ish (serde defaults) so the SAME struct deserializes an OLDER
/// backend's response, which advertises only the issuer + the client ids.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct ClientConfig {
    /// "oidc" (Keycloak) or "local" (the backend's own identity provider). Older backends
    /// do not send it; they are always Keycloak-backed, hence the default.
    #[serde(default = "default_auth_mode")]
    pub auth_mode: String,
    #[serde(default)]
    pub oidc_issuer: String,
    #[serde(default)]
    pub spa_client_id: String,
    #[serde(default)]
    pub browser_client_id: String,
    #[serde(default)]
    pub android_client_id: String,
    /// The client id the SERVER wants headless callers to run the device grant under. Absent
    /// on older backends, hence the fallback chain in [`Discovery::client_id`].
    #[serde(default)]
    pub device_client_id: String,
    /// Absolute URL. Absent on backends older than the local-auth mode - see [`Endpoints::resolve`].
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
    /// Absolute URL, serving BOTH the device_code and the refresh_token grant.
    #[serde(default)]
    pub device_token_endpoint: Option<String>,
}

fn default_auth_mode() -> String {
    "oidc".into()
}

/// The two endpoints the device grant needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub device_authorization: String,
    pub token: String,
}

impl Endpoints {
    /// What this client hardcoded before client-config advertised the endpoints: Keycloak's
    /// URL shape, derived from the issuer.
    pub fn from_issuer(issuer: &str) -> Endpoints {
        let issuer = issuer.trim_end_matches('/');
        Endpoints {
            device_authorization: format!("{issuer}/protocol/openid-connect/auth/device"),
            token: format!("{issuer}/protocol/openid-connect/token"),
        }
    }

    /// Prefer the endpoints the server advertises; fall back to the Keycloak-derived ones.
    ///
    /// The fallback exists for OLDER backends: they serve `/public/client-config` without the
    /// two endpoint fields (they predate the local identity provider and are therefore always
    /// Keycloak-fronted), so deriving the Keycloak URL shape from the issuer is still correct
    /// for them. A new backend always advertises both, in either mode.
    pub fn resolve(cfg: &ClientConfig, issuer_fallback: &str) -> Endpoints {
        let derived = Endpoints::from_issuer(issuer_fallback);
        Endpoints {
            device_authorization: non_empty(cfg.device_authorization_endpoint.as_deref())
                .unwrap_or(derived.device_authorization),
            token: non_empty(cfg.device_token_endpoint.as_deref()).unwrap_or(derived.token),
        }
    }
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Everything a client needs to persist after a login: which issuer it talks to (kept for
/// back-compat with already-enrolled config files) and where the device grant lives.
#[derive(Clone, Debug)]
pub struct Discovery {
    pub auth_mode: String,
    pub issuer: String,
    pub endpoints: Endpoints,
    /// What the server says to authenticate as, if anything. See [`Discovery::client_id`].
    pub device_client_id: Option<String>,
}

impl Discovery {
    /// The client id to run the device grant under.
    ///
    /// Server first, because only the server knows which clients its IdP actually has. The
    /// compiled-in default (`personal-agent-device`) exists in the shipped Keycloak realm and
    /// NOWHERE else -- against any other provider it fails with a bare `invalid_client`, which
    /// says nothing about the client id being the problem. An explicit `--client` still wins:
    /// it is the escape hatch for a server that advertises the wrong thing.
    pub fn client_id(&self, flag: &str, flag_is_default: bool) -> String {
        if !flag_is_default {
            return flag.to_string();
        }
        non_empty(self.device_client_id.as_deref()).unwrap_or_else(|| flag.to_string())
    }
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

/// Ask the server where to run the device grant. `issuer_override` (the `--issuer` flag) wins
/// over the advertised issuer; it is also the only thing left if the server cannot be reached
/// or is too old to advertise anything.
pub async fn discover(server: &str, issuer_override: Option<&str>) -> Result<Discovery> {
    let cfg = match fetch_client_config(server).await {
        Ok(cfg) => cfg,
        Err(e) => {
            // No client-config: only an explicit --issuer can still get us to Keycloak.
            let issuer = non_empty(issuer_override)
                .ok_or_else(|| anyhow!("{e}; pass --issuer to configure the login manually"))?;
            return Ok(Discovery {
                auth_mode: default_auth_mode(),
                endpoints: Endpoints::from_issuer(&issuer),
                issuer,
                device_client_id: None,
            });
        }
    };
    let issuer = non_empty(issuer_override).unwrap_or_else(|| cfg.oidc_issuer.clone());
    if issuer.is_empty() && cfg.device_authorization_endpoint.is_none() {
        bail!("the server advertises neither an OIDC issuer nor a device endpoint");
    }
    Ok(Discovery {
        auth_mode: cfg.auth_mode.clone(),
        endpoints: Endpoints::resolve(&cfg, &issuer),
        issuer,
        // The SPA client is the sensible second choice: it is public and, on providers that
        // register one client for all the front ends (Authentik among them), it is the one with
        // the device grant enabled.
        device_client_id: non_empty(Some(cfg.device_client_id.as_str()))
            .or_else(|| non_empty(Some(cfg.spa_client_id.as_str()))),
    })
}

/// The token endpoint for a REFRESH: the one persisted at login, else the Keycloak-derived one
/// (a config file written before the endpoints were persisted, or by an old client).
pub fn token_endpoint(persisted: Option<&str>, issuer: &str) -> String {
    non_empty(persisted).unwrap_or_else(|| Endpoints::from_issuer(issuer).token)
}

/// How the caller shows the pending device authorization to the user, and how it phrases a
/// failure (the TUI localizes both, the agent prints plain text).
pub trait Prompt {
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

    const OIDC_MODE: &str = r#"{
        "auth_mode": "oidc",
        "oidc_issuer": "https://id.example.com/realms/personal-agent",
        "spa_client_id": "personal-agent-spa",
        "browser_client_id": "personal-agent-browser",
        "android_client_id": "personal-agent-app",
        "device_authorization_endpoint":
            "https://id.example.com/realms/personal-agent/protocol/openid-connect/auth/device",
        "device_token_endpoint":
            "https://id.example.com/realms/personal-agent/protocol/openid-connect/token"
    }"#;

    const LOCAL_MODE: &str = r#"{
        "auth_mode": "local",
        "oidc_issuer": "https://pa.example.com",
        "spa_client_id": "personal-agent-spa",
        "browser_client_id": "personal-agent-browser",
        "android_client_id": "personal-agent-app",
        "device_authorization_endpoint": "https://pa.example.com/api/v1/auth/device/code",
        "device_token_endpoint": "https://pa.example.com/api/v1/auth/device/token"
    }"#;

    // What a backend that predates the local identity provider serves: no auth_mode, no endpoints.
    const OLD_BACKEND: &str = r#"{
        "oidc_issuer": "https://id.example.com/realms/personal-agent",
        "oidc_audience": "personal-agent-api",
        "spa_client_id": "personal-agent-spa",
        "browser_client_id": "personal-agent-browser",
        "android_client_id": "personal-agent-app"
    }"#;

    fn parse(json: &str) -> ClientConfig {
        serde_json::from_str(json).expect("client-config parses")
    }

    #[test]
    fn advertised_endpoints_win_in_oidc_mode() {
        let cfg = parse(OIDC_MODE);
        let ep = Endpoints::resolve(&cfg, &cfg.oidc_issuer);
        assert_eq!(cfg.auth_mode, "oidc");
        assert_eq!(
            ep.device_authorization,
            "https://id.example.com/realms/personal-agent/protocol/openid-connect/auth/device"
        );
        assert_eq!(
            ep.token,
            "https://id.example.com/realms/personal-agent/protocol/openid-connect/token"
        );
    }

    #[test]
    fn advertised_endpoints_point_at_the_backend_in_local_mode() {
        let cfg = parse(LOCAL_MODE);
        let ep = Endpoints::resolve(&cfg, &cfg.oidc_issuer);
        assert_eq!(cfg.auth_mode, "local");
        assert_eq!(
            ep.device_authorization,
            "https://pa.example.com/api/v1/auth/device/code"
        );
        assert_eq!(ep.token, "https://pa.example.com/api/v1/auth/device/token");
    }

    #[test]
    fn old_backend_falls_back_to_the_keycloak_shape() {
        let cfg = parse(OLD_BACKEND);
        let ep = Endpoints::resolve(&cfg, &cfg.oidc_issuer);
        assert_eq!(cfg.auth_mode, "oidc"); // defaulted
        assert_eq!(
            ep,
            Endpoints::from_issuer("https://id.example.com/realms/personal-agent")
        );
        assert_eq!(
            ep.device_authorization,
            "https://id.example.com/realms/personal-agent/protocol/openid-connect/auth/device"
        );
    }

    #[test]
    fn empty_advertised_endpoints_are_ignored() {
        let cfg: ClientConfig = serde_json::from_str(
            r#"{"device_authorization_endpoint": "", "device_token_endpoint": null}"#,
        )
        .unwrap();
        assert_eq!(
            Endpoints::resolve(&cfg, "https://id.example.com/realms/pa"),
            Endpoints::from_issuer("https://id.example.com/realms/pa")
        );
    }

    #[test]
    fn refresh_uses_the_persisted_endpoint_then_the_issuer() {
        assert_eq!(
            token_endpoint(Some("https://pa.example.com/api/v1/auth/device/token"), ""),
            "https://pa.example.com/api/v1/auth/device/token"
        );
        assert_eq!(
            token_endpoint(None, "https://id.example.com/realms/pa"),
            "https://id.example.com/realms/pa/protocol/openid-connect/token"
        );
    }

    /// Serve ONE canned HTTP response on loopback and hand back its base URL: the HTTP layer
    /// under test, without a network.
    async fn mock_server(status_line: &'static str, body: &'static str) -> String {
        // reqwest is built no-provider (the `pa` binary installs one in main()); tests must too.
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
    async fn discover_reads_the_endpoints_a_local_backend_advertises() {
        let server = mock_server(
            "200 OK",
            r#"{"auth_mode":"local","oidc_issuer":"https://pa.example.com",
                "device_authorization_endpoint":"https://pa.example.com/api/v1/auth/device/code",
                "device_token_endpoint":"https://pa.example.com/api/v1/auth/device/token"}"#,
        )
        .await;
        let d = discover(&server, None).await.unwrap();
        assert_eq!(d.auth_mode, "local");
        assert_eq!(
            d.endpoints.device_authorization,
            "https://pa.example.com/api/v1/auth/device/code"
        );
        assert_eq!(
            d.endpoints.token,
            "https://pa.example.com/api/v1/auth/device/token"
        );
    }

    #[tokio::test]
    async fn discover_derives_keycloak_urls_from_an_old_backend() {
        let server = mock_server("200 OK", r#"{"oidc_issuer":"https://id.example.com/realms/pa","spa_client_id":"personal-agent-spa"}"#).await;
        let d = discover(&server, None).await.unwrap();
        assert_eq!(d.auth_mode, "oidc");
        assert_eq!(d.issuer, "https://id.example.com/realms/pa");
        assert_eq!(
            d.endpoints,
            Endpoints::from_issuer("https://id.example.com/realms/pa")
        );
    }

    #[tokio::test]
    async fn an_explicit_issuer_overrides_the_advertised_one() {
        let server = mock_server(
            "200 OK",
            r#"{"oidc_issuer":"https://id.example.com/realms/pa"}"#,
        )
        .await;
        let d = discover(&server, Some("https://other.example.com/realms/x"))
            .await
            .unwrap();
        assert_eq!(d.issuer, "https://other.example.com/realms/x");
        assert_eq!(
            d.endpoints,
            Endpoints::from_issuer("https://other.example.com/realms/x")
        );
    }

    #[tokio::test]
    async fn an_unreachable_client_config_still_works_with_an_explicit_issuer() {
        let server = mock_server("404 Not Found", "{}").await;
        let d = discover(&server, Some("https://id.example.com/realms/pa"))
            .await
            .unwrap();
        assert_eq!(
            d.endpoints,
            Endpoints::from_issuer("https://id.example.com/realms/pa")
        );

        let server = mock_server("404 Not Found", "{}").await;
        assert!(discover(&server, None).await.is_err());
    }

    // --- which client the device grant runs under -------------------------------------

    #[tokio::test]
    async fn the_server_names_the_device_client() {
        // The whole point: `personal-agent-device` exists in the shipped Keycloak realm and
        // nowhere else. On any other provider it fails with a bare `invalid_client`, which
        // names neither the client nor the fact that the client id is what is wrong.
        let server = mock_server(
            "200 OK",
            r#"{"oidc_issuer":"https://id.example.com/x","device_client_id":"pa-device-here"}"#,
        )
        .await;
        let d = discover(&server, None).await.unwrap();
        assert_eq!(
            d.client_id(DEFAULT_DEVICE_CLIENT_ID, true),
            "pa-device-here"
        );
    }

    #[tokio::test]
    async fn the_spa_client_is_the_fallback() {
        // Providers that register ONE public client for every front end (Authentik among them)
        // advertise it as the SPA client, and that is the one with the device grant enabled.
        let server = mock_server(
            "200 OK",
            r#"{"oidc_issuer":"https://id.example.com/x","spa_client_id":"personal-agent-spa"}"#,
        )
        .await;
        let d = discover(&server, None).await.unwrap();
        assert_eq!(
            d.client_id(DEFAULT_DEVICE_CLIENT_ID, true),
            "personal-agent-spa"
        );
    }

    #[tokio::test]
    async fn an_explicit_client_flag_wins() {
        // The escape hatch for a server that advertises the wrong thing.
        let server = mock_server(
            "200 OK",
            r#"{"oidc_issuer":"https://x","device_client_id":"advertised"}"#,
        )
        .await;
        let d = discover(&server, None).await.unwrap();
        assert_eq!(d.client_id("chosen-by-hand", false), "chosen-by-hand");
    }

    #[tokio::test]
    async fn an_old_backend_leaves_the_compiled_in_default() {
        // Advertises neither field: the Keycloak-era behaviour has to survive untouched.
        let server = mock_server("200 OK", r#"{"oidc_issuer":"https://id.example.com/x"}"#).await;
        let d = discover(&server, None).await.unwrap();
        assert_eq!(
            d.client_id(DEFAULT_DEVICE_CLIENT_ID, true),
            DEFAULT_DEVICE_CLIENT_ID
        );
    }
}
