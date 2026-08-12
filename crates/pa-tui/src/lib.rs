//! `pa_tui` — the Personal Agent terminal chat client as a library. It speaks the SAME
//! `/api/v1` HTTP + SSE endpoints as the web SPA: `login` runs the OAuth device-flow
//! (authenticating AS the user, no secret; against Keycloak or the backend's own local
//! identity provider - it discovers which from the server), and `run` opens the chat UI.

mod agui;
mod api;
mod app;
mod composer;
mod config;
mod i18n;
mod oidc;
mod picker;
mod sse;
mod terminal;
mod ui;
mod ws;

use std::sync::Arc;

use anyhow::Result;

use api::ApiClient;
use config::Config;
use i18n::{t, Msg};

/// Pick the UI language from an explicit value / `PA_TUI_LANG` / the system locale.
pub fn init_i18n(lang: Option<&str>) {
    i18n::init_from(lang);
}

/// Open the chat UI (default when no subcommand is given).
pub async fn run() -> Result<()> {
    let cfg = config::load()?;
    i18n::init_from(cfg.lang.as_deref());
    let client = Arc::new(ApiClient::new(&cfg)?);
    app::run(client).await
}

/// Log in via the device flow (Keycloak or the backend's local identity provider) and store the
/// connection config. `issuer` is optional: the server's client-config advertises it (and the
/// device endpoints), so only an override needs to be passed.
/// Run the device grant for `server` and persist TERMINAL credentials.
///
/// The desktop window and the terminal UI authenticate separately: the window loads the SPA and
/// lets it do its own in-window OIDC web login (the native bridge says `ownsAuth:false`), while
/// the terminal reads `config.toml`. Signing in to one therefore did nothing for the other, and
/// the terminal's advice ("run `pa login`") is unreachable from a window that is not a terminal.
///
/// This is the bridge, and it is a deliberate, visible act rather than a silent copy: the grant
/// runs again, the user confirms it, and only its OWN tokens are written. Lifting the webview's
/// tokens into a file would be a different security decision than the one already made.
pub async fn enroll_terminal_access<P: pa_oidc::Prompt>(
    server: &str,
    prompt: &P,
) -> Result<std::path::PathBuf> {
    let disco = pa_oidc::discover(server, None).await?;
    let client = disco.client_id(pa_oidc::DEFAULT_DEVICE_CLIENT_ID, true);
    let tokens = pa_oidc::device_login(&disco.endpoints, &client, prompt).await?;
    let cfg = Config {
        server: server.trim_end_matches('/').to_string(),
        issuer: disco.issuer,
        client_id: client,
        token_endpoint: Some(disco.endpoints.token),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        org: None,
        lang: None,
    };
    // Writes 0600 (config::save) - the file holds a refresh token.
    config::save(&cfg)?;
    Ok(config::config_path())
}

pub async fn login(
    server: String,
    issuer: Option<String>,
    client: String,
    org: Option<String>,
    lang: Option<String>,
) -> Result<()> {
    i18n::init_from(lang.as_deref());
    let disco = pa_oidc::discover(&server, issuer.as_deref()).await?;
    // Which client to authenticate as is the server's call unless the user overrode it: the
    // compiled-in default only exists in the shipped Keycloak realm, and against any other
    // provider it fails with a bare `invalid_client` that names nothing.
    let client = disco.client_id(&client, client == pa_oidc::DEFAULT_DEVICE_CLIENT_ID);
    let tokens = oidc::device_login(&disco.endpoints, &client).await?;
    let cfg = Config {
        server,
        issuer: disco.issuer,
        client_id: client,
        token_endpoint: Some(disco.endpoints.token),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        org,
        lang,
    };
    config::save(&cfg)?;
    println!(
        "{}",
        t(Msg::LoginSuccess(
            &config::config_path().display().to_string()
        ))
    );
    println!("{}", t(Msg::LoginStartHint));
    Ok(())
}

/// Remove the stored credentials.
pub async fn logout() -> Result<()> {
    let path = config::config_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("{}", t(Msg::LogoutDone(&path.display().to_string())));
    } else {
        println!("{}", t(Msg::LogoutNone));
    }
    Ok(())
}
