//! Control WebSocket — the long-lived socket at `/api/v1/ws` (Frozen Contract #10).
//!
//! Outbound: client frames (`cancel`/`status`/`ping`). Inbound: server pushes (chat-title
//! changes, tool-approval + agent-question prompts, background-resumed runs, sub-agent
//! updates, …). Auth rides the `Sec-WebSocket-Protocol: bearer, <token>` subprotocol — the
//! same scheme as the device-agent (and never the query string). The task auto-reconnects
//! with backoff, refreshing the token first since an expired token is the usual cause.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

use crate::api::ApiClient;
use crate::app::AppMsg;
use crate::i18n::{t, Msg};

/// One sub-question of an agent question (a single-question frame becomes a 1-element list).
#[derive(Debug, Clone)]
pub struct WsSubQuestion {
    pub question: String,
    pub options: Vec<String>,
    pub multi_select: bool,
    pub allow_custom: bool,
}

/// A decoded server push we act on. Some fields (e.g. `run_id`) are decoded for
/// completeness — they'll scope actions to the active run in a later slice.
#[allow(dead_code)]
#[derive(Debug)]
pub enum ServerFrame {
    Connected(bool),
    ChatTitle {
        chat_id: String,
        title: String,
    },
    ToolApproval {
        run_id: String,
        approval_id: String,
        device_name: String,
        tool: String,
        command: String,
    },
    /// `multi` mirrors the server's `questions` field: when true the run was answered via
    /// the `answers` (list-per-question) shape, else the single `answer` shape.
    AgentQuestion {
        chat_id: String,
        run_id: String,
        question_id: String,
        subs: Vec<WsSubQuestion>,
        multi: bool,
    },
    BackgroundResumed {
        chat_id: String,
        run_id: String,
    },
    SubagentUpdate {
        chat_id: String,
        run_id: String,
        status: String,
        kind: String,
        label: String,
        background: bool,
        error: Option<String>,
        input_tokens: i64,
        output_tokens: i64,
        cost_usd: Option<f64>,
        tool_calls: i64,
        started_at: Option<String>,
    },
    InboxChanged,
    /// The chat LIST changed (a chat was created/renamed/deleted on another client or by the
    /// agent) → reload the sidebar so new chats appear live across all clients.
    ChatsChanged,
    /// The mid-run follow-up queue count for a chat changed (any client) → update the chip.
    FollowupsChanged {
        chat_id: String,
        count: usize,
    },
    /// A run started/finished on a chat (any client or server-side, e.g. an auto-drained
    /// follow-up or a goal's next turn). ``run_id`` (on start) lets us attach + stream it live.
    ChatRun {
        chat_id: String,
        active: bool,
        run_id: Option<String>,
    },
    Note(String),
}

/// Send-side handle to the control socket. Cloneable; frames are queued and survive a
/// transient reconnect (the writer drains the queue on the next live connection).
#[derive(Clone)]
pub struct WsHandle {
    tx: UnboundedSender<String>,
}

impl WsHandle {
    fn send(&self, frame: Value) {
        let _ = self.tx.send(frame.to_string());
    }

    pub fn cancel(&self, run_id: &str) {
        self.send(json!({ "v": 1, "type": "cancel", "run_id": run_id }));
    }

    #[allow(dead_code)] // status query; wired into the UI in a later slice
    pub fn status(&self, run_id: &str) {
        self.send(json!({ "v": 1, "type": "status", "run_id": run_id }));
    }
}

/// Spawn the control-WS task and return a handle for sending client frames.
pub fn spawn(client: Arc<ApiClient>, app_tx: UnboundedSender<AppMsg>) -> WsHandle {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(manager(client, app_tx, out_rx));
    WsHandle { tx: out_tx }
}

async fn manager(
    client: Arc<ApiClient>,
    app_tx: UnboundedSender<AppMsg>,
    mut out_rx: UnboundedReceiver<String>,
) {
    let mut backoff = 1u64;
    loop {
        match connect_and_run(&client, &app_tx, &mut out_rx).await {
            Ok(true) => return, // handle dropped → app exiting
            Ok(false) => {
                // Clean close (e.g. server drain); reconnect promptly.
                let _ = app_tx.send(AppMsg::Server(ServerFrame::Connected(false)));
                backoff = 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(_) => {
                let _ = app_tx.send(AppMsg::Server(ServerFrame::Connected(false)));
                // An expired token is the common failure; refresh before backing off.
                let _ = client.force_refresh().await;
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

/// One connection lifecycle. Returns `Ok(true)` when the send handle was dropped (stop),
/// `Ok(false)` on a clean close (reconnect), or `Err` on a network/handshake failure.
async fn connect_and_run(
    client: &ApiClient,
    app_tx: &UnboundedSender<AppMsg>,
    out_rx: &mut UnboundedReceiver<String>,
) -> Result<bool> {
    let token = client.access_token().await;
    let mut req = client.ws_url().into_client_request()?;
    req.headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, format!("bearer, {token}").parse()?);

    let (stream, _resp) =
        connect_async_tls_with_config(req, None, false, Some(tls_connector())).await?;
    let _ = app_tx.send(AppMsg::Server(ServerFrame::Connected(true)));

    let (mut write, mut read) = stream.split();
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await; // consume the immediate first tick

    // Proactively refresh the bearer a minute before it expires and push a `reauth` frame,
    // so a long session never reconnects on token expiry (and REST/SSE stay authenticated
    // too, since force_refresh updates the shared token). Falls back to ~4 min if the exp
    // can't be read.
    let period = token_ttl(&token)
        .map(|t| t.saturating_sub(60).max(30))
        .unwrap_or(240);
    let mut reauth = tokio::time::interval(Duration::from_secs(period));
    reauth.tick().await;

    loop {
        tokio::select! {
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(txt))) => {
                    if let Some(frame) = parse_server_frame(&txt) {
                        let _ = app_tx.send(AppMsg::Server(frame));
                    }
                }
                Some(Ok(Message::Ping(p))) => write.send(Message::Pong(p)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(false),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            outgoing = out_rx.recv() => match outgoing {
                Some(frame) => write.send(Message::Text(frame)).await?,
                None => return Ok(true), // all handles dropped
            },
            _ = ping.tick() => {
                write.send(Message::Text(json!({ "v": 1, "type": "ping" }).to_string())).await?;
            }
            _ = reauth.tick() => {
                match client.force_refresh().await {
                    Ok(()) => {
                        let fresh = client.access_token().await;
                        let frame = json!({ "v": 1, "type": "reauth", "token": fresh });
                        write.send(Message::Text(frame.to_string())).await?;
                    }
                    Err(_) => {
                        // Refresh token exhausted/revoked — surface it; the reconnect path
                        // will retry and ultimately the user must `login` again.
                        let _ = app_tx.send(AppMsg::Server(ServerFrame::Note(t(Msg::ApiRefreshFailed))));
                    }
                }
            }
        }
    }
}

/// A rustls connector on the workspace's shared trust store, with ALPN pinned to http/1.1 (so
/// a reverse proxy doesn't negotiate h2, which the HTTP/1.1 WebSocket upgrade can't ride).
pub(crate) fn tls_connector() -> Connector {
    // Same trust store as every HTTP call in the workspace -- an internal CA has to
    // work for the control socket too, or the login succeeds and the stream does not.
    Connector::Rustls(Arc::new(pa_oidc::tls::ws_tls_config()))
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Seconds until the JWT's `exp`, decoded from its (unverified) payload. None if the token
/// isn't a readable JWT — the caller then uses a conservative fixed refresh interval.
fn token_ttl(token: &str) -> Option<u64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = v.get("exp")?.as_u64()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(exp.saturating_sub(now))
}

fn str_list(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the agent-question sub-questions: the `questions` array (multi) or, when absent,
/// a single sub-question synthesised from the top-level `question`/`options` fields.
fn parse_subquestions(v: &Value) -> (Vec<WsSubQuestion>, bool) {
    if let Some(items) = v
        .get("questions")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        let subs = items
            .iter()
            .map(|q| WsSubQuestion {
                question: s(q, "question"),
                options: str_list(q, "options"),
                multi_select: q
                    .get("multi_select")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                allow_custom: q
                    .get("allow_custom")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            })
            .collect();
        return (subs, true);
    }
    let sub = WsSubQuestion {
        question: s(v, "question"),
        options: str_list(v, "options"),
        multi_select: false,
        allow_custom: v
            .get("allow_custom")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    };
    (vec![sub], false)
}

/// Decode a server control frame by its `type` discriminator (mirrors `ServerControl`).
pub fn parse_server_frame(txt: &str) -> Option<ServerFrame> {
    let v: Value = serde_json::from_str(txt).ok()?;
    Some(match v.get("type").and_then(Value::as_str)? {
        "chat_title" => ServerFrame::ChatTitle {
            chat_id: s(&v, "chat_id"),
            title: s(&v, "title"),
        },
        "tool_approval" => ServerFrame::ToolApproval {
            run_id: s(&v, "run_id"),
            approval_id: s(&v, "approval_id"),
            device_name: s(&v, "device_name"),
            tool: s(&v, "tool"),
            command: s(&v, "command"),
        },
        "agent_question" => {
            let (subs, multi) = parse_subquestions(&v);
            ServerFrame::AgentQuestion {
                chat_id: s(&v, "chat_id"),
                run_id: s(&v, "run_id"),
                question_id: s(&v, "question_id"),
                subs,
                multi,
            }
        }
        "background_resumed" => ServerFrame::BackgroundResumed {
            chat_id: s(&v, "chat_id"),
            run_id: s(&v, "run_id"),
        },
        "subagent_update" => ServerFrame::SubagentUpdate {
            chat_id: s(&v, "chat_id"),
            run_id: s(&v, "run_id"),
            status: s(&v, "status"),
            kind: s(&v, "kind"),
            label: s(&v, "label"),
            background: v
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            error: v.get("error").and_then(Value::as_str).map(String::from),
            input_tokens: v.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
            output_tokens: v.get("output_tokens").and_then(Value::as_i64).unwrap_or(0),
            cost_usd: v.get("cost_usd").and_then(Value::as_f64),
            tool_calls: v.get("tool_calls").and_then(Value::as_i64).unwrap_or(0),
            started_at: v
                .get("started_at")
                .and_then(Value::as_str)
                .map(String::from),
        },
        "draft_pending" => ServerFrame::Note(t(Msg::NoteDraftPending)),
        "inbox_changed" => ServerFrame::InboxChanged,
        "chats_changed" => ServerFrame::ChatsChanged,
        "followups_changed" => ServerFrame::FollowupsChanged {
            chat_id: s(&v, "chat_id"),
            count: v.get("count").and_then(Value::as_u64).unwrap_or(0) as usize,
        },
        "chat_run" => ServerFrame::ChatRun {
            chat_id: s(&v, "chat_id"),
            active: v.get("active").and_then(Value::as_bool).unwrap_or(false),
            run_id: v.get("run_id").and_then(Value::as_str).map(String::from),
        },
        "memory_committed" => ServerFrame::Note(t(Msg::NoteMemory)),
        // ack/status_reply/pong/error and anything unknown: not surfaced here.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_single_agent_question() {
        let txt = r#"{"v":1,"type":"agent_question","chat_id":"c","run_id":"r","question_id":"q","question":"Weiter?","options":["Ja","Nein"],"allow_custom":false}"#;
        match parse_server_frame(txt) {
            Some(ServerFrame::AgentQuestion { subs, multi, .. }) => {
                assert!(!multi);
                assert_eq!(subs.len(), 1);
                assert_eq!(subs[0].question, "Weiter?");
                assert_eq!(subs[0].options, vec!["Ja", "Nein"]);
                assert!(!subs[0].allow_custom);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decodes_multi_agent_question() {
        let txt = r#"{"type":"agent_question","chat_id":"c","run_id":"r","question_id":"q","question":"?","questions":[{"question":"a","options":["x","y"],"multi_select":true},{"question":"b"}]}"#;
        match parse_server_frame(txt) {
            Some(ServerFrame::AgentQuestion { subs, multi, .. }) => {
                assert!(multi);
                assert_eq!(subs.len(), 2);
                assert_eq!(subs[0].question, "a");
                assert!(subs[0].multi_select);
                assert_eq!(subs[0].options, vec!["x", "y"]);
                assert!(!subs[1].multi_select);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decodes_subagent_update() {
        let txt = r#"{"type":"subagent_update","chat_id":"c","parent_run_id":"p","run_id":"r","status":"completed","kind":"explore","label":"Suche Doku","background":true,"input_tokens":120,"output_tokens":45,"cost_usd":0.002,"tool_calls":3}"#;
        match parse_server_frame(txt) {
            Some(ServerFrame::SubagentUpdate {
                chat_id,
                run_id,
                status,
                kind,
                background,
                input_tokens,
                output_tokens,
                tool_calls,
                ..
            }) => {
                assert_eq!(chat_id, "c");
                assert_eq!(run_id, "r");
                assert_eq!(status, "completed");
                assert_eq!(kind, "explore");
                assert!(background);
                assert_eq!(input_tokens, 120);
                assert_eq!(output_tokens, 45);
                assert_eq!(tool_calls, 3);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decodes_tool_approval() {
        let txt = r#"{"type":"tool_approval","run_id":"r","approval_id":"a","device_id":"d","device_name":"Laptop","tool":"bash","command":"ls -la"}"#;
        match parse_server_frame(txt) {
            Some(ServerFrame::ToolApproval {
                device_name,
                command,
                ..
            }) => {
                assert_eq!(device_name, "Laptop");
                assert_eq!(command, "ls -la");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn token_ttl_reads_exp() {
        use base64::Engine as _;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let payload = serde_json::json!({ "exp": now + 120 }).to_string();
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let token = format!("header.{b}.sig");
        let ttl = token_ttl(&token).expect("ttl");
        assert!(ttl > 100 && ttl <= 120, "ttl was {ttl}");
        assert!(token_ttl("not-a-jwt").is_none());
    }

    #[test]
    fn ignores_acks_and_unknown() {
        assert!(parse_server_frame(r#"{"type":"ack","run_id":"r","of":"cancel"}"#).is_none());
        assert!(parse_server_frame(r#"{"type":"pong"}"#).is_none());
        assert!(parse_server_frame("not json").is_none());
    }
}
