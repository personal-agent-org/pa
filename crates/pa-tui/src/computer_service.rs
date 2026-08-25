//! TUI entry point for installing the separate Computer Service.
//!
//! The TUI uses its user credential only to reserve a computer device. It then downloads the
//! standalone service and launches its own OAuth enrollment. The chat access/refresh token is
//! never passed to or stored by Computer Service.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::api::ApiClient;
use crate::i18n::{t, Msg};

fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x64"),
        ("windows", "x86_64") => Ok("windows-x64"),
        ("macos", "aarch64") => Ok("macos-arm64"),
        (os, arch) => bail!(t(Msg::ComputerServiceUnsupported(os, arch))),
    }
}

fn binary_path() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base =
            std::env::var_os("LOCALAPPDATA").context(t(Msg::ComputerServiceNoLocalAppData))?;
        return Ok(PathBuf::from(base)
            .join("PersonalAgent")
            .join("bin")
            .join("computer-service.exe"));
    }
    #[cfg(not(windows))]
    {
        let home = dirs::home_dir().context(t(Msg::ComputerServiceNoHome))?;
        Ok(home.join(".local").join("bin").join("computer-service"))
    }
}

pub async fn install(client: &ApiClient, name: &str) -> Result<()> {
    println!("{}", t(Msg::ComputerServiceReserve(name)));
    let device = client.create_computer_device(name).await?;
    let server = client.server_origin().trim_end_matches('/');
    let url = format!(
        "{server}/api/v1/devices/computer-service/bin/{}",
        platform()?
    );
    println!("{}", t(Msg::ComputerServiceDownload));
    let bytes = pa_oidc::tls::http_client()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let binary = binary_path()?;
    if let Some(parent) = binary.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&binary, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;
    }

    println!("{}", t(Msg::ComputerServiceEnroll));
    let workspace = dirs::home_dir()
        .map(|home| home.join("projects"))
        .unwrap_or_else(|| PathBuf::from("projects"));
    let status = Command::new(&binary)
        .env(
            "PA_LANG",
            if crate::i18n::lang() == crate::i18n::Lang::En {
                "en"
            } else {
                "de"
            },
        )
        .args([
            "enroll",
            "--server",
            server,
            "--device",
            device.id.as_str(),
            "--workspace",
        ])
        .arg(workspace)
        .status()
        .context(t(Msg::ComputerServiceLaunchFailed))?;
    if !status.success() {
        bail!(t(Msg::ComputerServiceEnrollFailed));
    }
    println!(
        "{}",
        t(Msg::ComputerServiceInstalled(&binary.display().to_string()))
    );
    Ok(())
}
