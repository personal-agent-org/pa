//! `pa` — the Personal Agent multitool. One binary bundles the terminal chat UI, the
//! (future) desktop GUI, and the device service (agent). Each surface lives in its own
//! library crate; this binary is just the clap dispatch plus the one-time TLS setup.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pa",
    version,
    about = "Personal Agent - one binary: terminal UI, desktop GUI, device service"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in via Keycloak (device flow) and store the connection config.
    Login {
        /// Personal Agent base URL, e.g. https://pa.example.com
        #[arg(long)]
        server: String,
        /// Keycloak issuer, e.g. https://id.example.com/realms/personal-agent
        #[arg(long)]
        issuer: String,
        /// OIDC client id for the device flow.
        #[arg(long, default_value = "personal-agent-device")]
        client: String,
        /// Optional active org (X-Personal-Agent-Org); omit to use the token default.
        #[arg(long)]
        org: Option<String>,
        /// UI language ("de" | "en"); stored in the config. Omit for the system locale.
        #[arg(long)]
        lang: Option<String>,
    },
    /// Remove the stored terminal-UI credentials.
    Logout,
    /// Open the desktop GUI window (requires a gui-enabled build).
    Gui,
    /// Device service: connect this machine and serve coding tools.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Log in via Keycloak (device flow) and store the connection config.
    Enroll {
        /// Personal Agent base URL, e.g. https://pa.example.com
        #[arg(long)]
        server: String,
        /// Device id (from the Geräte tab)
        #[arg(long)]
        device: String,
        /// Keycloak issuer, e.g. https://id.example.com/realms/personal-agent
        #[arg(long)]
        issuer: String,
        /// OIDC client id for the device flow
        #[arg(long, default_value = "personal-agent-device")]
        client: String,
        /// Directory the agent may operate in
        #[arg(long, default_value = ".")]
        workspace: String,
    },
    /// Connect and serve tool calls.
    #[command(alias = "start")]
    Run,
    /// Print the available coding tools (name + description) as JSON. The desktop app uses
    /// this to render the per-device tool-exposure toggles.
    Tools,
    /// Git credential helper (invoked by git for Personal-Agent-cloned repos). Internal.
    #[command(hide = true)]
    CredentialHelper {
        /// git credential operation: get | store | erase
        #[arg(default_value = "get")]
        operation: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // The GUI owns the native (main) thread and runs its own event loop, so it must NOT be
    // launched from inside a tokio runtime. Everything else runs on a normal async runtime.
    if matches!(cli.cmd, Some(Cmd::Gui)) {
        return gui();
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    // rustls 0.23 needs an explicit process-level crypto provider for the TLS/wss handshake
    // (both libs use the no-provider reqwest/tungstenite feature). Install `ring` once.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Pick the language up front; the tui commands refine it per invocation.
    pa_tui::init_i18n(None);

    match cli.cmd {
        None => pa_tui::run().await,
        Some(Cmd::Login {
            server,
            issuer,
            client,
            org,
            lang,
        }) => pa_tui::login(server, issuer, client, org, lang).await,
        Some(Cmd::Logout) => pa_tui::logout().await,
        Some(Cmd::Gui) => unreachable!("gui is dispatched before the async runtime"),
        Some(Cmd::Service { cmd }) => match cmd {
            ServiceCmd::Enroll {
                server,
                device,
                issuer,
                client,
                workspace,
            } => pa_agent::enroll(server, device, issuer, client, workspace).await,
            ServiceCmd::Run => pa_agent::run().await,
            ServiceCmd::Tools => pa_agent::tools().await,
            ServiceCmd::CredentialHelper { operation } => {
                pa_agent::credential_helper(&operation).await
            }
        },
    }
}

#[cfg(feature = "gui")]
fn gui() -> Result<()> {
    // Tauri runs its own event loop and takes over this (main) thread. The context is
    // generated HERE (this crate holds tauri.conf.json + the tauri-build codegen), so the
    // bundler and the macro agree on one config location.
    pa_gui::run(tauri::generate_context!());
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn gui() -> Result<()> {
    eprintln!("the GUI is not included in this build (rebuild with --features gui)");
    Ok(())
}
