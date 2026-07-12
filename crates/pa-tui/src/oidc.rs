//! Device-flow login for the TUI: the shared `pa_oidc` device grant (RFC 8628) plus this
//! surface's localized prompts. The TUI prints a URL + code, the user authorizes in a browser,
//! and we poll for tokens. The access token is then sent as `Authorization: Bearer` on every
//! REST/SSE call (and as the `bearer,<jwt>` subprotocol on the control WS).
//!
//! Works against a Keycloak-fronted backend AND one running its own local identity provider:
//! the endpoints come from the server's client-config, not from the issuer's URL shape.

use anyhow::Result;
use pa_oidc::{Endpoints, Prompt, Tokens};

use crate::i18n::{t, Msg};

struct TuiPrompt;

impl Prompt for TuiPrompt {
    fn authorize(&self, url: &str, user_code: &str) {
        eprintln!("{}", t(Msg::OidcOpen));
        eprintln!("    {url}");
        eprintln!("{}", t(Msg::OidcCode(user_code)));
        eprintln!("{}", t(Msg::OidcWaiting));
    }

    fn failed(&self, error: &str) -> String {
        t(Msg::OidcFailed(error))
    }
}

/// Run the full device-code login flow; blocks until the user authorizes (or it errors).
pub async fn device_login(endpoints: &Endpoints, client_id: &str) -> Result<Tokens> {
    pa_oidc::device_login(endpoints, client_id, &TuiPrompt).await
}

/// Exchange a refresh token for a fresh access (+ refresh) token. `token_endpoint` is the one
/// persisted at login; it falls back to the Keycloak shape derived from `issuer`.
pub async fn refresh(
    token_endpoint: Option<&str>,
    issuer: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let endpoint = pa_oidc::token_endpoint(token_endpoint, issuer);
    pa_oidc::refresh(&endpoint, client_id, refresh_token).await
}
