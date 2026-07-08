//! Consume a run's Server-Sent-Events stream and forward decoded AG-UI events to the UI.
//!
//! `POST /chats/{id}/runs` responds with `text/event-stream`; the `X-Run-Id` header names
//! the run (so we can show/cancel it) and each SSE `data:` frame is a `BusRecord`. We tail
//! the byte stream, cut frames at line boundaries (so multibyte UTF-8 — e.g. umlauts — is
//! never split mid-character), and translate each event into a `StreamMsg`.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use crate::agui;
use crate::api::ApiClient;
use crate::app::AppMsg;

/// A decoded streaming event, ready for the app state to fold into the live message.
#[derive(Debug)]
pub enum StreamMsg {
    RunId(String),
    Text(String),
    Thinking(String),
    ToolStart {
        id: String,
        name: String,
    },
    ToolArgs {
        id: String,
        delta: String,
    },
    ToolResult {
        id: String,
        content: String,
    },
    Usage {
        model: Option<String>,
        input: i64,
        output: i64,
        cost: Option<f64>,
    },
    /// The stream dropped and is being re-attached: clear the live turn so the server's
    /// replay (Last-Event-ID 0) rebuilds it cleanly instead of duplicating the text so far.
    Reset,
    Finished,
    Error(String),
}

/// Why `consume` stopped tailing a stream.
enum Outcome {
    /// A terminal event (or clean EOF) was seen — the turn is complete.
    Done,
    /// The byte stream errored (connection dropped) — the caller may re-attach.
    Transient,
}

/// Max re-attach attempts before giving up on a dropped run stream.
const MAX_RECONNECTS: u32 = 8;

fn backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis((500 * attempt as u64).min(3000))
}

/// Which streaming endpoint a turn POSTs to.
pub enum RunKind {
    New,
    Side,
    Rerun,
}

/// Start a turn (new run, `/btw` side query, or rerun): POST, then relay every event.
pub async fn stream_run(
    client: Arc<ApiClient>,
    kind: RunKind,
    chat_id: String,
    body: serde_json::Value,
    tx: UnboundedSender<AppMsg>,
) {
    let opened = match kind {
        RunKind::New => client.open_run_stream(&chat_id, &body).await,
        RunKind::Side => client.open_btw_stream(&chat_id, &body).await,
        RunKind::Rerun => client.open_rerun_stream(&chat_id, &body).await,
    };
    let resp = match opened {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(AppMsg::Stream(StreamMsg::Error(format!("{e:#}"))));
            return;
        }
    };
    let run_id = resp
        .headers()
        .get("X-Run-Id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if let Some(rid) = &run_id {
        let _ = tx.send(AppMsg::Stream(StreamMsg::RunId(rid.clone())));
    }
    // `/btw` side runs are ephemeral (not persisted) → not re-attachable; everything else is.
    let reconnect_id = match kind {
        RunKind::Side => None,
        _ => run_id,
    };
    pump(client, chat_id, reconnect_id, resp, &tx).await;
}

/// Attach to an EXISTING run's live stream (reconnect-on-open / background-resumed).
/// The run id is already known, so we announce it before replaying the stream.
pub async fn attach_run(
    client: Arc<ApiClient>,
    chat_id: String,
    run_id: String,
    tx: UnboundedSender<AppMsg>,
) {
    let _ = tx.send(AppMsg::Stream(StreamMsg::RunId(run_id.clone())));
    let resp = match client.attach_run_stream(&chat_id, &run_id).await {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(AppMsg::Stream(StreamMsg::Error(format!("{e:#}"))));
            return;
        }
    };
    pump(client, chat_id, Some(run_id), resp, &tx).await;
}

/// Tail a run's stream and, when it drops mid-run, re-attach (replaying from the start so
/// the live turn is rebuilt) until it finishes or the retry budget is spent. `run_id` is
/// `None` for streams that can't be re-attached (ephemeral `/btw`), which then fail hard.
async fn pump(
    client: Arc<ApiClient>,
    chat_id: String,
    run_id: Option<String>,
    first: reqwest::Response,
    tx: &UnboundedSender<AppMsg>,
) {
    let mut resp = Some(first);
    let mut attempts: u32 = 0;
    loop {
        // Obtain the next response: the initial one, or a fresh re-attach.
        let r = match resp.take() {
            Some(r) => r,
            None => {
                let Some(rid) = run_id.as_deref() else {
                    return;
                };
                match client.attach_run_stream(&chat_id, rid).await {
                    Ok(r) => r,
                    Err(_) => {
                        attempts += 1;
                        if attempts > MAX_RECONNECTS {
                            let _ = tx.send(AppMsg::Stream(StreamMsg::Error(crate::i18n::t(
                                crate::i18n::Msg::StreamReconnectFailed,
                            ))));
                            return;
                        }
                        tokio::time::sleep(backoff(attempts)).await;
                        continue;
                    }
                }
            }
        };

        match consume(r, tx).await {
            Outcome::Done => return,
            Outcome::Transient => {
                // No run id → can't replay; surface the loss.
                if run_id.is_none() {
                    let _ = tx.send(AppMsg::Stream(StreamMsg::Error(crate::i18n::t(
                        crate::i18n::Msg::StreamLost,
                    ))));
                    return;
                }
                attempts += 1;
                if attempts > MAX_RECONNECTS {
                    let _ = tx.send(AppMsg::Stream(StreamMsg::Error(crate::i18n::t(
                        crate::i18n::Msg::StreamReconnectFailed,
                    ))));
                    return;
                }
                // Clear the live turn; the replay rebuilds it (resp stays None → re-attach).
                let _ = tx.send(AppMsg::Stream(StreamMsg::Reset));
                tokio::time::sleep(backoff(attempts)).await;
            }
        }
    }
}

/// Tail an SSE response, decode each `data:` frame, and relay it as a `StreamMsg`.
/// Returns `Transient` if the byte stream errors mid-run (caller may re-attach), or `Done`
/// on a terminal event / clean EOF.
async fn consume(resp: reqwest::Response, tx: &UnboundedSender<AppMsg>) -> Outcome {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut data = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            // Connection dropped mid-stream (proxy idle close, HTTP/2 reset, …). Don't fail
            // the turn — let the caller re-attach and replay.
            Err(_) => return Outcome::Transient,
        };
        buf.extend_from_slice(&chunk);

        // Process whole lines; bytes after the last '\n' stay buffered for the next chunk.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\n', '\r']);

            if line.is_empty() {
                // Blank line = event boundary; dispatch the accumulated data payload.
                if !data.is_empty() {
                    if dispatch(&data, tx) {
                        return Outcome::Done; // terminal event seen
                    }
                    data.clear();
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest);
            }
            // `id:`/`retry:`/`: comment` (heartbeats) lines are ignored.
        }
    }

    // Stream closed without an explicit terminal event — treat as done.
    let _ = tx.send(AppMsg::Stream(StreamMsg::Finished));
    Outcome::Done
}

/// Map one BusRecord's AG-UI event to a StreamMsg. Returns true on a terminal event.
fn dispatch(data: &str, tx: &UnboundedSender<AppMsg>) -> bool {
    let Some(record) = agui::parse_bus_record(data) else {
        return false;
    };
    let ev = record.ev;
    let msg = match ev.kind.as_str() {
        agui::TEXT_MESSAGE_CONTENT => ev.delta.map(StreamMsg::Text),
        agui::THINKING_CONTENT => ev.delta.map(StreamMsg::Thinking),
        agui::TOOL_CALL_START => Some(StreamMsg::ToolStart {
            id: ev.tool_call_id.unwrap_or_default(),
            name: ev.tool_call_name.unwrap_or_else(|| "tool".into()),
        }),
        agui::TOOL_CALL_ARGS => ev.delta.map(|delta| StreamMsg::ToolArgs {
            id: ev.tool_call_id.unwrap_or_default(),
            delta,
        }),
        agui::TOOL_CALL_RESULT => Some(StreamMsg::ToolResult {
            id: ev.tool_call_id.unwrap_or_default(),
            content: ev.content.unwrap_or_default(),
        }),
        agui::RUN_FINISHED => {
            let _ = tx.send(AppMsg::Stream(StreamMsg::Finished));
            return true;
        }
        agui::RUN_ERROR => {
            let m = ev.message.unwrap_or_else(|| "Run fehlgeschlagen".into());
            let _ = tx.send(AppMsg::Stream(StreamMsg::Error(m)));
            return true;
        }
        agui::CUSTOM if ev.name.as_deref() == Some(agui::CUSTOM_USAGE) => {
            ev.value.map(|v| StreamMsg::Usage {
                model: v
                    .get("model_name")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                input: v.get("input_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
                output: v.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
                cost: v.get("cost_usd").and_then(|x| x.as_f64()),
            })
        }
        _ => None,
    };
    if let Some(msg) = msg {
        let _ = tx.send(AppMsg::Stream(msg));
    }
    false
}
