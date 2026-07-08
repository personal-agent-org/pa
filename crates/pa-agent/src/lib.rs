//! `pa_agent` — the Personal Agent device agent as a library. It connects a machine
//! (Linux, macOS, or Windows) to Personal Agent and serves coding tools. PTY/shell,
//! paths and process handling are platform-gated.

mod client;
mod config;
mod credential;
mod jail;
mod lsp;
mod oidc;
mod proc;
mod pty;
mod sandbox;

use anyhow::Result;

use config::Config;

/// Log in via Keycloak (device flow) and store the connection config.
pub async fn enroll(
    server: String,
    device: String,
    issuer: String,
    client: String,
    workspace: String,
) -> Result<()> {
    let abs = std::fs::canonicalize(&workspace)
        .map(|p| p.display().to_string())
        .unwrap_or(workspace);
    let tokens = oidc::device_login(&issuer, &client).await?;
    let cfg = Config {
        server,
        device_id: device,
        workspace: abs,
        issuer,
        client_id: client,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        sandbox_token: None,
        home_root: None, // default $HOME at runtime via home_root_resolved()
        sandbox: false,  // opt-in OS command sandbox; user enables in config.toml
        disabled_tools: Vec::new(), // everything exposed until the desktop app narrows it
        expose_home_index: true,
        jail: true, // jailed by default; user sets `jail = false` in config.toml
    };
    config::save(&cfg)?;
    println!(
        "\nVerbunden ✓ Gerät {} → {} (Workspace {})",
        cfg.device_id,
        config::config_path().display(),
        cfg.workspace
    );
    println!("Starte den Agenten mit: pa service run");
    Ok(())
}

/// Connect and serve tool calls.
pub async fn run() -> Result<()> {
    // A backend-spawned cloud sandbox injects its config via env (PA_SANDBOX_TOKEN);
    // a normal device reads the enrolled config file.
    let cfg = match Config::from_env() {
        Some(c) => c,
        None => config::load()?,
    };
    client::run(cfg).await
}

/// Print the available coding tools (name + description) as JSON. The desktop app uses this
/// to render the per-device tool-exposure toggles.
pub async fn tools() -> Result<()> {
    println!("{}", jail::tool_specs());
    Ok(())
}

/// Git credential helper (invoked by git for Personal-Agent-cloned repos). Internal.
pub async fn credential_helper(operation: &str) -> Result<()> {
    credential::run(operation).await
}
