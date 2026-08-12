//! The run loop: connect outbound, announce tools, serve tool calls + PTY sessions.
//!
//! The device id + token ride in the `Sec-WebSocket-Protocol` subprotocol offer
//! (`device`, `<id>`, `<token>`) — the server comma-joins + authenticates them, never
//! the query string. The socket multiplexes: one-shot tool calls (`rpc_call` → jailed,
//! `rpc_result`) AND interactive terminals (`pty_*`). The sink is owned by a writer task
//! fed via an mpsc channel so background PTY readers + concurrent tool calls can all send.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

use crate::config::Config;
use crate::jail::{tool_specs, CodingWorkspaces, HomeIndex, Workspace};
use crate::pty::PtyManager;

const AGENT_VERSION: &str = "0.4.13";

fn hello(
    disabled_tools: &[String],
    home_available: bool,
    jailed: bool,
    jail_root: Option<&str>,
) -> String {
    // `home_index` signals the read-only $HOME search/read surface (separate from the coding
    // jail). The backend exposes it via ONE generic, device-targeted search_files/read_file tool.
    let capabilities = if home_available {
        json!(["coding", "home_index"])
    } else {
        json!(["coding"])
    };
    // Drop the tools the user turned off for this device (configured from the desktop app), so the
    // backend never offers them. An empty disabled list announces everything.
    let mut tools = tool_specs();
    if !disabled_tools.is_empty() {
        if let Some(arr) = tools.as_array_mut() {
            arr.retain(|t| {
                t.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| !disabled_tools.iter().any(|d| d == n))
                    .unwrap_or(true)
            });
        }
    }
    // Jail status is DEVICE-AUTHORITATIVE (from the local config): the backend mirrors it
    // read-only and can never override it. jail_root is informational (the confinement root).
    json!({
        "type": "hello", "v": 1, "capabilities": capabilities,
        "agent_version": AGENT_VERSION, "tools": tools,
        "jailed": jailed, "jail_root": if jailed { jail_root } else { None },
    })
    .to_string()
}

pub async fn run(mut cfg: Config) -> Result<()> {
    // Register as git's global credential helper so EVERY repo on this device/sandbox — existing
    // or freshly cloned — authenticates linked-account hosts (clone/fetch/push), token never on disk.
    crate::credential::ensure_global_helper();
    // OS-level command sandbox (Linux landlock write-confinement). Opt-in + fail-safe; warn
    // once if requested but the kernel can't provide it (commands then run unconfined).
    crate::sandbox::set_enabled(cfg.sandbox);
    if cfg.sandbox {
        if crate::sandbox::supported() {
            eprintln!(
                "personal-agent-device-agent: command sandbox ON (landlock write-confinement)"
            );
        } else {
            eprintln!(
                "personal-agent-device-agent: sandbox requested but landlock is unavailable on \
                 this kernel — commands run UNCONFINED (path-jail still applies)"
            );
        }
    }
    // Path jail is the device's local, authoritative choice (config `jail`, default on).
    let ws = Workspace::new(&cfg.workspace)?.with_jail(cfg.jail);
    // Read-only home index (distinct from the workspace jail). None if $HOME can't be resolved.
    let home = cfg
        .home_root_resolved()
        .and_then(|r| HomeIndex::new(&r).ok());
    // Coding workspaces: the enrolled default OR a user-chosen folder under home (per chat).
    let coding = CodingWorkspaces::new(ws, home.as_ref().map(HomeIndex::root_path));
    eprintln!(
        "personal-agent-device-agent: workspace={} home={} server={}",
        coding.default_root_display(),
        home.as_ref()
            .map(HomeIndex::root_display)
            .unwrap_or_else(|| "(none)".into()),
        cfg.ws_url(),
    );
    let mut backoff = 1u64;
    loop {
        // OIDC user devices refresh the short-lived access token before each (re)connect;
        // a backend-managed sandbox skips that — it authenticates with its injected
        // PA_SANDBOX_TOKEN over the `sandbox:` subprotocol.
        if cfg.sandbox_token.is_none() {
            match crate::oidc::refresh(&cfg).await {
                Ok(t) => {
                    cfg.access_token = t.access_token;
                    cfg.refresh_token = t.refresh_token;
                    let _ = crate::config::save(&cfg);
                }
                Err(e) => {
                    eprintln!("Token-Refresh fehlgeschlagen ({e}); bitte neu anmelden (enroll). Retry in 30s");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            }
        }
        match serve(&cfg, &coding, &home).await {
            Ok(()) => backoff = 1,
            Err(e) => {
                eprintln!("disconnected ({e}); retry in {backoff}s");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

fn sval(f: &Value, k: &str) -> String {
    f.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn uval(f: &Value, k: &str, default: u16) -> u16 {
    f.get(k)
        .and_then(Value::as_u64)
        .map(|n| n as u16)
        .unwrap_or(default)
}

/// A rustls connector on the workspace's shared trust store, with ALPN pinned to http/1.1 (so
/// Caddy doesn't negotiate h2, which the HTTP/1.1 WebSocket upgrade can't ride).
fn tls_connector() -> Connector {
    // Same trust store as every HTTP call in the workspace -- an internal CA has to
    // work for the control socket too, or the login succeeds and the stream does not.
    Connector::Rustls(Arc::new(pa_oidc::tls::ws_tls_config()))
}

async fn serve(cfg: &Config, coding: &CodingWorkspaces, home: &Option<HomeIndex>) -> Result<()> {
    let mut req = cfg.ws_url().into_client_request()?;
    let proto = match &cfg.sandbox_token {
        Some(tok) => format!("device, {}, sandbox:{}", cfg.device_id, tok),
        None => format!("device, {}, bearer:{}", cfg.device_id, cfg.access_token),
    };
    req.headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, proto.parse()?);
    // Force http/1.1 ALPN: Caddy otherwise negotiates h2, but tungstenite speaks
    // WebSocket over HTTP/1.1 — the mismatch made the server reset the handshake.
    let (stream, _resp) =
        connect_async_tls_with_config(req, None, false, Some(tls_connector())).await?;
    eprintln!("connected — announcing tools");
    let (mut write, mut read) = stream.split();
    let jail_root = coding.default_root_display();
    write
        .send(Message::Text(hello(
            &cfg.disabled_tools,
            cfg.expose_home_index && home.is_some(),
            cfg.jail,
            Some(&jail_root),
        )))
        .await?;

    // The writer task owns the sink; everything else sends frames through `tx`.
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if write.send(m).await.is_err() {
                break;
            }
        }
    });

    let pty = PtyManager::default();
    let cwd = coding.default_root_display();

    let result: Result<()> = async {
        while let Some(msg) = read.next().await {
            match msg? {
                Message::Text(txt) => {
                    let frame: Value = match serde_json::from_str(&txt) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let ftype = frame
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    match ftype.as_str() {
                        "ping" => {
                            let _ =
                                tx.send(Message::Text(json!({"type": "pong", "v": 1}).to_string()));
                        }
                        "rpc_call" => {
                            // Spawn so a long run_command never blocks PTY / other calls.
                            let coding2 = coding.clone();
                            let home2 = home.clone();
                            let tx2 = tx.clone();
                            tokio::spawn(async move {
                                let _ = tx2.send(Message::Text(
                                    handle_call(&coding2, &home2, &frame).await,
                                ));
                            });
                        }
                        "pty_open" => {
                            let sid = sval(&frame, "session_id");
                            // Root the shell in the chat's resolved workspace: a chosen folder
                            // (direct or worktree) or the default per-chat worktree.
                            let pty_cwd = coding
                                .resolve(
                                    frame.get("workspace_path").and_then(Value::as_str),
                                    frame.get("workspace").and_then(Value::as_str).unwrap_or(""),
                                )
                                .map(|w| w.root_display())
                                .unwrap_or_else(|_| cwd.clone());
                            if let Err(e) = pty.open(
                                &sid,
                                uval(&frame, "cols", 80),
                                uval(&frame, "rows", 24),
                                &pty_cwd,
                                tx.clone(),
                            ) {
                                eprintln!("pty open failed: {e}");
                                let _ = tx.send(Message::Text(
                                    json!({"type":"pty_exit","v":1,"session_id":sid,"code":1})
                                        .to_string(),
                                ));
                            }
                        }
                        "pty_input" => {
                            let _ = pty.input(&sval(&frame, "session_id"), &sval(&frame, "data"));
                        }
                        "pty_resize" => {
                            pty.resize(
                                &sval(&frame, "session_id"),
                                uval(&frame, "cols", 80),
                                uval(&frame, "rows", 24),
                            );
                        }
                        "pty_close" => pty.close(&sval(&frame, "session_id")),
                        _ => {}
                    }
                }
                Message::Ping(p) => {
                    let _ = tx.send(Message::Pong(p));
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        Ok(())
    }
    .await;

    drop(tx); // end the writer task; dropping `pty` SIGHUPs any live shells
    writer.abort();
    result
}

async fn handle_call(coding: &CodingWorkspaces, home: &Option<HomeIndex>, frame: &Value) -> String {
    let req_id = frame.get("req_id").and_then(Value::as_str).unwrap_or("");
    let tool = frame.get("tool").and_then(Value::as_str).unwrap_or("");
    let args = frame.get("args").cloned().unwrap_or_else(|| json!({}));

    // home_* ops target the read-only $HOME surface — NOT per-chat, NOT the coding jail.
    let outcome = if tool == "home_search" || tool == "home_read" || tool == "home_list" {
        match home {
            Some(h) => h.execute(tool, &args).await,
            None => Err(anyhow::anyhow!("home index not available on this device")),
        }
    } else if tool == "git_clone" {
        // Provision a new coding workspace by cloning a repo (backend /workspace/clone). Jailed to
        // an allowed root; NOT per-chat (it CREATES the folder a later run jails to).
        coding
            .git_clone(
                args.get("url").and_then(Value::as_str).unwrap_or(""),
                args.get("dest").and_then(Value::as_str).unwrap_or(""),
                args.get("branch").and_then(Value::as_str),
            )
            .await
    } else if tool == "mkproject" {
        // Provision an empty new-project folder (backend /workspace/new). Jailed; NOT per-chat.
        coding
            .mkproject(args.get("dest").and_then(Value::as_str).unwrap_or(""))
            .await
    } else {
        // Resolve the coding workspace for this chat: a chosen folder under $HOME (direct edit or
        // a concurrent-chat worktree) via `args.workspace_path`, else the default per-chat
        // worktree keyed by `args.workspace` (chat id). Absent both → the base workspace.
        match coding.resolve(
            args.get("workspace_path").and_then(Value::as_str),
            args.get("workspace").and_then(Value::as_str).unwrap_or(""),
        ) {
            Ok(w) => w.execute(tool, &args).await,
            Err(e) => Err(e),
        }
    };
    match outcome {
        Ok(result) => {
            json!({"type": "rpc_result", "v": 1, "req_id": req_id, "ok": true, "result": result})
                .to_string()
        }
        Err(e) => json!({
            "type": "rpc_result", "v": 1, "req_id": req_id, "ok": false, "error": e.to_string(),
        })
        .to_string(),
    }
}
