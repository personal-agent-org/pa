//! `pa`, Personal Agent's terminal chat client. It consumes the chat API and never exposes
//! tools or host capabilities to the backend. Those belong exclusively to the separate `pacs`
//! Computer Service.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pa", version, about = "Personal Agent terminal chat client")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in via the device flow and store the terminal client's credentials.
    Login {
        /// Personal Agent base URL, for example https://pa.example.com.
        #[arg(long)]
        server: String,
        /// Optional active organization; omit to use the token default.
        #[arg(long)]
        org: Option<String>,
        /// UI language (de or en); omit to use the system locale.
        #[arg(long)]
        lang: Option<String>,
    },
    /// Remove the stored terminal-client credentials.
    Logout,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    pa_tui::init_i18n(None);

    match cli.cmd {
        None => pa_tui::run().await,
        Some(Cmd::Login { server, org, lang }) => pa_tui::login(server, org, lang).await,
        Some(Cmd::Logout) => pa_tui::logout().await,
    }
}
