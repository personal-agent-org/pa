//! `pa` chat-client config — stored at ~/.config/personal-agent-pa/config.toml (mode 0600).
//!
//! The OIDC device-flow logs the user in from a headless terminal, and the access/refresh
//! tokens are persisted so a later
//! `run` starts straight into the chat UI without a fresh browser round-trip.

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    /// Personal Agent base URL, e.g. https://pa.example.com (no trailing /api/v1).
    pub server: String,
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    // Token endpoint (device_code + refresh_token grants), discovered from the backend.
    pub token_endpoint: String,
    // Optional active-org override (X-Personal-Agent-Org); otherwise the token default is used.
    #[serde(default)]
    pub org: Option<String>,
    // Preferred UI language ("de" | "en"); None falls back to PA_LANG / system locale.
    #[serde(default)]
    pub lang: Option<String>,
}

impl Config {
    /// The `/api/v1` REST + SSE base (server with a single trailing slash trimmed).
    pub fn api_base(&self) -> String {
        format!("{}/api/v1", self.server.trim_end_matches('/'))
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("personal-agent-pa")
        .join("config.toml")
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string(cfg)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        bail!(
            "{}",
            crate::i18n::t(crate::i18n::Msg::ConfigNotEnrolled(
                &path.display().to_string()
            ))
        );
    }
    Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
}
