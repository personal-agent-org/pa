//! Agent config — stored at ~/.config/personal-agent-device/config.toml (dir kept for enrolled-agent continuity) (token chmod 600).

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub server: String,
    pub device_id: String,
    pub workspace: String,
    // OIDC (Keycloak device-flow) — the agent authenticates AS the user, no secret.
    pub issuer: String,
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    // Backend-managed cloud sandbox: when set, the agent authenticates with this
    // short-lived internal token over the `sandbox:` subprotocol (no OIDC/refresh).
    #[serde(default)]
    pub sandbox_token: Option<String>,
    // Read-only home-file index root (a SEPARATE surface from `workspace`). Default $HOME.
    // Optional + serde(default) so already-enrolled config.toml files load unchanged.
    #[serde(default)]
    pub home_root: Option<String>,
    // OS-level command sandbox (Linux landlock): confine `run_command` WRITES to the
    // workspace + build caches. Opt-in, fail-safe; default off so existing configs load
    // unchanged and the toolchain is never surprised. See sandbox.rs.
    #[serde(default)]
    pub sandbox: bool,
    // Tool exposure: coding-tool names the user turned OFF for this device (configured from the
    // desktop app). The hello announcement drops them so the backend never offers them. Empty =
    // everything exposed. serde(default) so already-enrolled configs load unchanged.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    // Whether to expose the read-only $HOME search/read surface (home_index capability). Default
    // true so existing configs keep it; the desktop app can turn it off.
    #[serde(default = "default_true")]
    pub expose_home_index: bool,
    // Path jail (DEVICE-AUTHORITATIVE): when true, every tool path is confined under `workspace`
    // (the default). When false, the agent operates UNJAILED on the full filesystem — the user's
    // explicit local choice. Reported to the backend on connect; the backend mirrors it read-only
    // and can never override it. Default true so an existing config.toml loads as jailed (safe).
    #[serde(default = "default_true")]
    pub jail: bool,
}

fn default_true() -> bool {
    true
}

impl Config {
    /// Sandbox mode — config injected via env by the backend that spawned this container.
    /// No file, no OIDC; the credential is PA_SANDBOX_TOKEN.
    pub fn from_env() -> Option<Config> {
        let token = std::env::var("PA_SANDBOX_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(Config {
            server: std::env::var("PA_SERVER").ok()?,
            device_id: std::env::var("PA_DEVICE_ID").ok()?,
            workspace: std::env::var("PA_WORKSPACE").unwrap_or_else(|_| "/workspace".into()),
            issuer: String::new(),
            client_id: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
            sandbox_token: Some(token),
            home_root: std::env::var("PA_HOME_ROOT").ok().filter(|s| !s.is_empty()),
            sandbox: matches!(std::env::var("PA_SANDBOX").as_deref(), Ok("1") | Ok("true")),
            disabled_tools: Vec::new(),
            expose_home_index: true,
            // A managed cloud sandbox is already an isolated container → run UNJAILED by default
            // (full FS inside the sandbox). PA_JAIL=1 forces jailing if ever wanted.
            jail: matches!(std::env::var("PA_JAIL").as_deref(), Ok("1") | Ok("true")),
        })
    }

    /// The home-index root: the configured value (expanding a leading `~/`), else $HOME.
    /// `None` only on exotic platforms where the home dir can't be determined.
    pub fn home_root_resolved(&self) -> Option<String> {
        match self.home_root.as_deref().filter(|s| !s.is_empty()) {
            Some(raw) if raw == "~" => dirs::home_dir().map(|h| h.to_string_lossy().into_owned()),
            Some(raw) => {
                if let Some(rest) = raw.strip_prefix("~/") {
                    dirs::home_dir().map(|h| h.join(rest).to_string_lossy().into_owned())
                } else {
                    Some(raw.to_string())
                }
            }
            None => dirs::home_dir().map(|h| h.to_string_lossy().into_owned()),
        }
    }
}

impl Config {
    /// The device WebSocket URL (http(s) → ws(s)).
    pub fn ws_url(&self) -> String {
        let base = self.server.trim_end_matches('/');
        let base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}/api/v1/ws/device")
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("personal-agent-device")
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
            "not enrolled — run `personal-agent-device-agent enroll` first ({})",
            path.display()
        );
    }
    Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
}
