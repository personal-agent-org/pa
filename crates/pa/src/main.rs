//! `pa` — Personal Agent's terminal and desktop chat clients. This process consumes the
//! chat API and never exposes tools or host capabilities to the backend; that is the separate
//! `computer-service` responsibility.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pa",
    version,
    about = "Personal Agent chat client - terminal UI and desktop GUI"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in via the device flow (OIDC or the server's local login) and store the config.
    Login {
        /// Personal Agent base URL, e.g. https://pa.example.com
        #[arg(long)]
        server: String,
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
}

/// True when nobody is at a terminal, in a build that HAS a GUI to fall back on.
///
/// stdin rather than stdout: a user who pipes output (`pa | less`) is still at a terminal and
/// still wants the TUI, whereas a desktop launcher gives the process no terminal on any stream.
#[cfg(feature = "gui")]
fn launched_from_a_desktop_entry() -> bool {
    use std::io::IsTerminal;
    !std::io::stdin().is_terminal()
}

/// Without the GUI feature there is nothing to fall back to, so the TUI stays the answer.
#[cfg(not(feature = "gui"))]
fn launched_from_a_desktop_entry() -> bool {
    false
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // The GUI owns the native (main) thread and runs its own event loop, so it must NOT be
    // launched from inside a tokio runtime. Everything else runs on a normal async runtime.
    //
    // No subcommand means "open the thing the user asked for", and that depends on HOW they
    // asked. From a terminal it is the TUI, as always. Launched from a desktop entry there is
    // no terminal at all: the TUI then draws into nothing and, on a machine without
    // `pa login`, exits with "not signed in - run `pa login` first" - advice
    // that cannot be followed from a window that isn't a terminal, for a config file the GUI
    // does not even use. The desktop bundle IS this binary, so the same argv has to serve both.
    if matches!(cli.cmd, Some(Cmd::Gui)) || (cli.cmd.is_none() && launched_from_a_desktop_entry()) {
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
        Some(Cmd::Login { server, org, lang }) => pa_tui::login(server, org, lang).await,
        Some(Cmd::Logout) => pa_tui::logout().await,
        Some(Cmd::Gui) => unreachable!("gui is dispatched before the async runtime"),
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
