//! One-shot shell execution in a chat's coding workspace (the composer's `!cmd` escape).
//!
//! The backend only exposes an *interactive* PTY WebSocket (`/api/v1/ws/terminal/{chat_id}`),
//! so we drive it as a one-shot: open the socket, send `cmd; echo <MARK>$?`, then read the
//! PTY output until the marker line appears — that brackets the command's output and yields
//! its exit code. The shell echoes our input, so we strip everything up to (and including)
//! the echoed command line, drop ANSI escapes, and stop at the marker. Good for
//! non-interactive commands (ls, git, cargo); not a substitute for a live terminal.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::Message;

use crate::api::ApiClient;
use crate::app::AppMsg;

/// Marker echoed after the command so we can find the end of its output + the exit code.
const MARK: &str = "__PA_TUI_SHELL_DONE__";
/// Overall wall-clock budget for a one-shot command before we give up waiting for the marker.
const DEADLINE: Duration = Duration::from_secs(60);

/// Run `command` in the chat's workspace and report the cleaned output + exit code back as an
/// `AppMsg::ShellOutput`. Errors (no workspace, auth, timeout) are reported in the same message.
pub async fn run_command(
    client: Arc<ApiClient>,
    chat_id: String,
    command: String,
    tx: UnboundedSender<AppMsg>,
) {
    let result = exec(&client, &chat_id, &command).await;
    let (output, code) = match result {
        Ok((out, code)) => (out, code),
        Err(e) => (e, None),
    };
    let _ = tx.send(AppMsg::ShellOutput { output, code });
}

async fn exec(
    client: &ApiClient,
    chat_id: &str,
    command: &str,
) -> Result<(String, Option<i32>), String> {
    let token = client.access_token().await;
    let url = client.terminal_ws_url(chat_id, 200, 50);
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("bad terminal URL: {e}"))?;
    req.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        format!("bearer, {token}")
            .parse()
            .map_err(|e| format!("bad auth header: {e}"))?,
    );

    // A 4404 close here means: no bound/online workspace device for this (coding) chat.
    let (stream, _resp) =
        connect_async_tls_with_config(req, None, false, Some(crate::ws::tls_connector()))
            .await
            .map_err(|e| format!("no workspace terminal available ({e})"))?;
    let (mut write, mut read) = stream.split();

    // Give the device a moment to open the PTY, then send the command + the marker echo.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let line = format!("{command}; echo \"{MARK}$?\"\n");
    let frame = json!({ "type": "pty_input", "data": b64(line.as_bytes()) });
    write
        .send(Message::Text(frame.to_string()))
        .await
        .map_err(|e| format!("send failed: {e}"))?;

    let mut buf: Vec<u8> = Vec::new();
    let deadline = tokio::time::sleep(DEADLINE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                let _ = write.send(close_frame()).await;
                let (out, _) = clean_output(&String::from_utf8_lossy(&buf));
                return Ok((format!("{out}\n[timed out]"), None));
            }
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(txt))) => {
                    if let Some(bytes) = decode_pty_output(&txt) {
                        buf.extend_from_slice(&bytes);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(code) = marker_code(&text) {
                            let _ = write.send(close_frame()).await;
                            let (out, _) = clean_output(&text);
                            return Ok((out, Some(code)));
                        }
                    } else if is_pty_exit(&txt) {
                        let (out, _) = clean_output(&String::from_utf8_lossy(&buf));
                        return Ok((out, None));
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    let (out, _) = clean_output(&String::from_utf8_lossy(&buf));
                    if out.is_empty() {
                        return Err("workspace terminal closed (no bound coding device?)".into());
                    }
                    return Ok((out, None));
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("terminal error: {e}")),
            },
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn close_frame() -> Message {
    Message::Text(json!({ "type": "pty_close" }).to_string())
}

/// Decode a `pty_output` frame's base64 payload into raw bytes (None for other frame types).
fn decode_pty_output(txt: &str) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_str(txt).ok()?;
    if v.get("type").and_then(Value::as_str)? != "pty_output" {
        return None;
    }
    let data = v.get("data").and_then(Value::as_str)?;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

fn is_pty_exit(txt: &str) -> bool {
    serde_json::from_str::<Value>(txt)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(Value::as_str)
                .map(|t| t == "pty_exit")
        })
        .unwrap_or(false)
}

/// The exit code from the `<MARK><digits>` result line, once it has been emitted. The echoed
/// command contains `<MARK>$?` (a literal `$?`), so we only match `<MARK>` followed by digits.
fn marker_code(text: &str) -> Option<i32> {
    let idx = find_marker_result(text)?;
    let after = &text[idx + MARK.len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Byte index of the `<MARK><digit>` occurrence (the result line, not the echoed command).
fn find_marker_result(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = text[from..].find(MARK) {
        let idx = from + rel;
        let next = text[idx + MARK.len()..].chars().next();
        if next.is_some_and(|c| c.is_ascii_digit()) {
            return Some(idx);
        }
        from = idx + MARK.len();
    }
    None
}

/// Strip ANSI escapes + the shell's echo of our command and the marker, leaving just the
/// command's output. Returns (output, found_marker).
fn clean_output(raw: &str) -> (String, bool) {
    let stripped = strip_ansi(raw);
    // Output lives between the echoed command line (first line containing MARK) and the
    // result line (MARK followed by digits). Fall back to the whole text if not found.
    let start = match stripped.find(MARK) {
        Some(i) => stripped[i..]
            .find('\n')
            .map(|n| i + n + 1)
            .unwrap_or(stripped.len()),
        None => 0,
    };
    let end = find_marker_result(&stripped).unwrap_or(stripped.len());
    let found = end < stripped.len();
    let slice = if start <= end {
        &stripped[start..end]
    } else {
        &stripped[..end.min(stripped.len())]
    };
    (slice.trim_matches(['\r', '\n']).to_string(), found)
}

/// Remove ANSI/VT escape sequences (CSI, OSC) and carriage returns for plain-text display.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: ESC [ … <final @-~>
                Some('[') => {
                    for d in chars.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] … BEL (or ST); consume to the terminator.
                Some(']') => {
                    while let Some(d) = chars.next() {
                        if d == '\x07' {
                            break;
                        }
                        if d == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Other 2-char escapes: drop the following byte.
                _ => {}
            },
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_output_between_echo_and_marker() {
        // Shell echoes the command (with literal $?), prints output, then the marker line.
        let raw = format!(
            "user@box:~$ ls; echo \"{MARK}$?\"\r\nfoo.txt\r\nbar.txt\r\n{MARK}0\r\nuser@box:~$ "
        );
        let (out, found) = clean_output(&raw);
        assert!(found);
        assert_eq!(out, "foo.txt\nbar.txt");
        assert_eq!(marker_code(&raw), Some(0));
    }

    #[test]
    fn captures_nonzero_exit_code() {
        let raw = format!("$ false; echo \"{MARK}$?\"\r\n{MARK}1\r\n$ ");
        assert_eq!(marker_code(&raw), Some(1));
        let (out, _) = clean_output(&raw);
        assert_eq!(out, "");
    }

    #[test]
    fn strips_ansi_color_codes() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
    }

    #[test]
    fn marker_ignores_the_echoed_command() {
        // Only the `$?` echo present (command still running) → no code yet.
        let raw = format!("$ sleep 1; echo \"{MARK}$?\"\r\n");
        assert_eq!(marker_code(&raw), None);
    }
}
