//! Device-flow login for the agent: the shared `pa_oidc` device grant (RFC 8628) plus this
//! surface's user-facing prompts. The agent logs in AS the user from a headless CLI: it prints
//! a URL + code, the user authorizes in a browser, and it polls for tokens. The access token is
//! then presented to the device WebSocket as `bearer:<jwt>` - no per-device secret.
//!
//! Works against a Keycloak-fronted backend AND one running its own local identity provider:
//! the endpoints come from the server's client-config, not from the issuer's URL shape.

use anyhow::Result;
use pa_oidc::{Endpoints, Prompt, Tokens};

use crate::config::Config;

struct AgentPrompt;

impl Prompt for AgentPrompt {
    fn authorize(&self, url: &str, user_code: &str) {
        eprintln!("\n  Zum Verbinden im Browser öffnen:");
        eprintln!("    {url}");
        eprintln!("  und diesen Code bestätigen: {user_code}\n");
        eprintln!("  Warte auf Bestätigung …");
    }

    fn failed(&self, error: &str) -> String {
        format!("Anmeldung fehlgeschlagen: {error}")
    }
}

/// Run the full device-code login flow; blocks until the user authorizes (or it errors).
pub async fn device_login(endpoints: &Endpoints, client_id: &str) -> Result<Tokens> {
    pa_oidc::device_login(endpoints, client_id, &AgentPrompt).await
}

/// Exchange the stored refresh token for a fresh access (+ refresh) token.
pub async fn refresh(cfg: &Config) -> Result<Tokens> {
    let endpoint = pa_oidc::token_endpoint(cfg.token_endpoint.as_deref(), &cfg.issuer);
    pa_oidc::refresh(&endpoint, &cfg.client_id, &cfg.refresh_token).await
}
