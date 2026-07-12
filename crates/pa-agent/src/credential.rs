//! Git credential helper. `git` runs `<agent> credential-helper <op>` for repos cloned via Personal
//! Agent (the agent registers itself as `credential.helper` at clone time). For `get` we resolve the
//! user's token for the repo's host LIVE from the backend (refreshing the OIDC token first), so the
//! provider token is NEVER written to the device's disk — and clone/fetch/push all authenticate.
//! `store`/`erase` are no-ops (nothing is persisted).

use anyhow::Result;
use std::io::{BufRead, Write};

use crate::config;

/// Make THIS agent git's GLOBAL credential helper — idempotent + non-destructive (`--add`, so any
/// existing helper still runs). Then every repo (existing OR freshly cloned) authenticates against
/// linked-account hosts on demand; for unlinked hosts the helper emits nothing and git falls
/// through. SSH remotes are unaffected (credential.helper is HTTPS-only).
pub fn ensure_global_helper() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let helper = format!("!\"{}\" credential-helper", exe.display());
    if let Ok(out) = std::process::Command::new("git")
        .args(["config", "--global", "--get-all", "credential.helper"])
        .output()
    {
        if String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == helper)
        {
            return; // already registered
        }
    }
    let _ = std::process::Command::new("git")
        .args(["config", "--global", "--add", "credential.helper", &helper])
        .status();
}

pub async fn run(operation: &str) -> Result<()> {
    if operation != "get" {
        return Ok(());
    }
    // Parse git's key=value attributes from stdin (terminated by a blank line); we need `host`.
    let mut host = String::new();
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("host=") {
            host = v.trim().to_string();
        }
    }
    if host.is_empty() {
        return Ok(());
    }
    // A cloud sandbox is configured via env (PA_SANDBOX_TOKEN/PA_SERVER), a user device via the
    // config file — try env first so the helper works in BOTH.
    let cfg = match config::Config::from_env() {
        Some(c) => c,
        None => match config::load() {
            Ok(c) => c,
            Err(_) => return Ok(()),
        },
    };
    // On any failure (no linked account / unreachable / unauthorized) emit nothing → git treats it
    // as "no credential" and proceeds (public) or fails cleanly (private), never blocking.
    if let Ok(Some((user, pass))) = resolve(&cfg, &host).await {
        let mut out = std::io::stdout();
        writeln!(out, "username={user}")?;
        writeln!(out, "password={pass}")?;
    }
    Ok(())
}

async fn resolve(cfg: &config::Config, host: &str) -> Result<Option<(String, String)>> {
    let http = reqwest::Client::new();
    let base = cfg.server.trim_end_matches('/');
    let resp = if let Some(sandbox_token) = cfg.sandbox_token.as_deref() {
        // Cloud sandbox: no OIDC — authenticate with the sandbox token + device id.
        http.get(format!("{base}/api/v1/git/credential/sandbox"))
            .query(&[("host", host)])
            .header("X-PA-Sandbox-Token", sandbox_token)
            .header("X-PA-Device-Id", cfg.device_id.as_str())
            .send()
            .await?
    } else {
        // User device: refresh the short-lived access token (persist the rotation), then bearer it.
        let access = match crate::oidc::refresh(cfg).await {
            Ok(t) => {
                let mut c = cfg.clone();
                c.access_token = t.access_token.clone();
                c.refresh_token = t.refresh_token;
                let _ = config::save(&c);
                t.access_token
            }
            Err(_) => cfg.access_token.clone(),
        };
        http.get(format!("{base}/api/v1/git/credential"))
            .query(&[("host", host)])
            .bearer_auth(&access)
            .send()
            .await?
    };
    if !resp.status().is_success() {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    struct Cred {
        username: String,
        password: String,
    }
    let c: Cred = resp.json().await?;
    Ok(Some((c.username, c.password)))
}
