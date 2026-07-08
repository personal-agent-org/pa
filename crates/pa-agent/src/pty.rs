//! Interactive PTY sessions for the coding-mode terminal.
//!
//! Each session is a REAL shell (NOT jailed — it's the user's own machine, an explicit
//! user action like SSH) spawned in the workspace directory. Output streams back as
//! base64 `pty_output` frames over the device WS. Driven by the browser terminal, never
//! by the LLM agent (which has no terminal tool).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use base64::Engine;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;

const READ_BUF: usize = 8192;

struct Session {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

#[derive(Clone, Default)]
pub struct PtyManager {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

fn b64encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl PtyManager {
    /// Open a shell session. The blocking reader runs on its own thread and streams
    /// `pty_output` frames into `out`; on EOF it emits `pty_exit` and drops the session.
    pub fn open(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
        cwd: &str,
        out: UnboundedSender<Message>,
    ) -> Result<()> {
        let pair = native_pty_system().openpty(pty_size(cols, rows))?;
        #[cfg(windows)]
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        #[cfg(not(windows))]
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let sid = session_id.to_string();
        let sessions = self.sessions.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let frame = json!({
                            "type": "pty_output", "v": 1,
                            "session_id": sid, "data": b64encode(&buf[..n]),
                        });
                        if out.send(Message::Text(frame.to_string())).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = out.send(Message::Text(
                json!({"type": "pty_exit", "v": 1, "session_id": sid, "code": 0}).to_string(),
            ));
            sessions.lock().unwrap().remove(&sid);
        });

        self.sessions.lock().unwrap().insert(
            session_id.to_string(),
            Session {
                writer,
                master: pair.master,
                child,
            },
        );
        Ok(())
    }

    pub fn input(&self, session_id: &str, data_b64: &str) -> Result<()> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
        let mut map = self.sessions.lock().unwrap();
        let sess = map
            .get_mut(session_id)
            .ok_or_else(|| anyhow!("no session"))?;
        sess.writer.write_all(&bytes)?;
        sess.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) {
        if let Some(sess) = self.sessions.lock().unwrap().get(session_id) {
            let _ = sess.master.resize(pty_size(cols, rows));
        }
    }

    pub fn close(&self, session_id: &str) {
        if let Some(mut sess) = self.sessions.lock().unwrap().remove(session_id) {
            let _ = sess.child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test(flavor = "multi_thread")]
    async fn pty_echoes_and_exits() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = PtyManager::default();
        mgr.open("s1", 80, 24, ".", tx).unwrap();
        mgr.input(
            "s1",
            &base64::engine::general_purpose::STANDARD.encode("echo hi_marker\n"),
        )
        .unwrap();

        let mut saw = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(Message::Text(t))) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "pty_output" {
                        let raw = base64::engine::general_purpose::STANDARD
                            .decode(v["data"].as_str().unwrap())
                            .unwrap();
                        saw.push_str(&String::from_utf8_lossy(&raw));
                        if saw.contains("hi_marker") {
                            break;
                        }
                    }
                }
                _ => continue,
            }
        }
        assert!(saw.contains("hi_marker"), "terminal never echoed: {saw:?}");
        mgr.resize("s1", 120, 40); // doesn't panic
        mgr.close("s1");
    }
}
