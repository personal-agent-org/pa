//! Background-process registry: long-running commands that OUTLIVE a single tool call.
//!
//! `run_command` runs to completion within a timeout — wrong for a dev server, a file watcher,
//! or a long build the agent wants to start and keep observing across turns. `run_background`
//! spawns such a command detached (its own process group), captures a bounded tail of its
//! stdout+stderr, and returns an id. `list_processes` / `read_process` / `stop_process` then
//! manage it. The registry lives on `CodingWorkspaces` (one per connection) so processes persist
//! across RPC calls; each process records the workspace `root` it was started in, and management
//! is scoped to that root (a chat only sees the processes started in its own workspace), mirroring
//! the path-jail boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MAX_PROC_LINES: usize = 4000; // captured tail per process (older lines are evicted)
const DEFAULT_TAIL: usize = 200; // lines returned by read_process when no count is given
const MAX_TRACKED: usize = 64; // hard cap on tracked processes per connection
const LINE_MAX: usize = 4000; // truncate any single captured line to this many chars

#[derive(Clone)]
enum Status {
    Running,
    Exited(i32),
    Killed,
    Failed(String),
}

impl Status {
    fn label(&self) -> String {
        match self {
            Status::Running => "running".to_string(),
            Status::Exited(c) => format!("exited({c})"),
            Status::Killed => "killed".to_string(),
            Status::Failed(e) => format!("failed: {e}"),
        }
    }
    fn is_terminal(&self) -> bool {
        !matches!(self, Status::Running)
    }
}

struct Lines {
    buf: Vec<String>,
    dropped: usize, // lines evicted off the front (so absolute indices stay meaningful)
}

struct BgProc {
    id: String,
    command: String,
    root: PathBuf,
    started: Instant,
    pid: Option<u32>,
    status: Arc<Mutex<Status>>,
    lines: Arc<Mutex<Lines>>,
}

impl BgProc {
    fn total_seen(&self) -> usize {
        let l = self.lines.lock().unwrap();
        l.dropped + l.buf.len()
    }
}

#[derive(Default)]
pub struct ProcRegistry {
    procs: Mutex<HashMap<String, Arc<BgProc>>>,
    seq: AtomicU64,
}

impl ProcRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start `command` in `root` as a background process; returns a short confirmation with its id.
    pub async fn start(&self, root: &Path, command: &str) -> anyhow::Result<String> {
        let command = command.trim();
        if command.is_empty() {
            anyhow::bail!("empty command");
        }
        self.reap();
        if self.live_count() >= MAX_TRACKED {
            anyhow::bail!(
                "too many background processes ({MAX_TRACKED}); stop some with stop_process first"
            );
        }
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("bg{n}");
        let status = Arc::new(Mutex::new(Status::Running));
        let lines = Arc::new(Mutex::new(Lines {
            buf: Vec::new(),
            dropped: 0,
        }));

        let pid = spawn(root, command, status.clone(), lines.clone())?;
        let proc = Arc::new(BgProc {
            id: id.clone(),
            command: command.to_string(),
            root: root.to_path_buf(),
            started: Instant::now(),
            pid,
            status,
            lines,
        });
        self.procs.lock().unwrap().insert(id.clone(), proc);
        Ok(format!(
            "Started background process {id} (pid {}). It keeps running after this call.\n\
             Use read_process id={id} to read its output, stop_process id={id} to stop it.",
            pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
        ))
    }

    /// One line per process started in `root`: id, status, uptime, captured-line count, command.
    pub fn list(&self, root: &Path) -> String {
        self.reap();
        let procs = self.procs.lock().unwrap();
        let mut rows: Vec<&Arc<BgProc>> = procs.values().filter(|p| p.root == root).collect();
        if rows.is_empty() {
            return "(no background processes)".to_string();
        }
        rows.sort_by_key(|p| p.id.clone());
        let mut out = String::new();
        for p in rows {
            let st = p.status.lock().unwrap().label();
            let age = p.started.elapsed().as_secs();
            out.push_str(&format!(
                "{}  [{}]  up {}s  {} lines  $ {}\n",
                p.id,
                st,
                age,
                p.total_seen(),
                truncate(&p.command, 200),
            ));
        }
        out.trim_end().to_string()
    }

    /// Captured output of process `id` (must belong to `root`). Returns the last `tail` lines
    /// (default 200), or a forward page from absolute line `offset` when given.
    pub fn read(
        &self,
        root: &Path,
        id: &str,
        tail: Option<usize>,
        offset: Option<usize>,
    ) -> anyhow::Result<String> {
        let proc = self.get(root, id)?;
        let status = proc.status.lock().unwrap().label();
        let l = proc.lines.lock().unwrap();
        let total = l.dropped + l.buf.len();
        let (slice, start): (&[String], usize) = if let Some(off) = offset {
            // Forward page: absolute line `off` onward (skip lines already evicted).
            let local = off.saturating_sub(l.dropped).min(l.buf.len());
            let want = tail.unwrap_or(DEFAULT_TAIL).max(1);
            let end = (local + want).min(l.buf.len());
            (&l.buf[local..end], l.dropped + local)
        } else {
            let want = tail.unwrap_or(DEFAULT_TAIL).max(1);
            let from = l.buf.len().saturating_sub(want);
            (&l.buf[from..], l.dropped + from)
        };
        let shown_end = start + slice.len();
        let mut out = format!("[{id}] status={status} lines={total}");
        if l.dropped > 0 {
            out.push_str(&format!(" (first {} lines evicted)", l.dropped));
        }
        out.push('\n');
        if slice.is_empty() {
            out.push_str("(no output captured yet)");
        } else {
            out.push_str(&slice.join("\n"));
        }
        if shown_end < total {
            out.push_str(&format!(
                "\n... {} more line(s); read_process id={id} offset={shown_end} for more",
                total - shown_end
            ));
        }
        Ok(out)
    }

    /// Stop process `id` (must belong to `root`): terminate its whole process group.
    pub fn stop(&self, root: &Path, id: &str) -> anyhow::Result<String> {
        let proc = self.get(root, id)?;
        {
            let st = proc.status.lock().unwrap();
            if st.is_terminal() {
                return Ok(format!("{id} already {}", st.label()));
            }
        }
        if let Some(pid) = proc.pid {
            kill_group(pid);
        }
        *proc.status.lock().unwrap() = Status::Killed;
        Ok(format!("Stopped background process {id}."))
    }

    fn get(&self, root: &Path, id: &str) -> anyhow::Result<Arc<BgProc>> {
        let procs = self.procs.lock().unwrap();
        match procs.get(id) {
            Some(p) if p.root == root => Ok(p.clone()),
            // Don't leak the existence of another workspace's process.
            _ => anyhow::bail!("no such background process: {id}"),
        }
    }

    fn live_count(&self) -> usize {
        self.procs
            .lock()
            .unwrap()
            .values()
            .filter(|p| !p.status.lock().unwrap().is_terminal())
            .count()
    }

    /// Forget terminated processes once their output has aged out, so a long-lived connection
    /// doesn't accumulate them unboundedly. Keeps the most recent terminated ones for inspection.
    fn reap(&self) {
        let mut procs = self.procs.lock().unwrap();
        if procs.len() <= MAX_TRACKED {
            return;
        }
        let mut terminated: Vec<(String, u64)> = procs
            .iter()
            .filter(|(_, p)| p.status.lock().unwrap().is_terminal())
            .map(|(k, p)| (k.clone(), p.started.elapsed().as_secs()))
            .collect();
        // Oldest terminated first.
        terminated.sort_by_key(|(_, age)| std::cmp::Reverse(*age));
        for (k, _) in terminated {
            if procs.len() <= MAX_TRACKED {
                break;
            }
            procs.remove(&k);
        }
    }
}

fn push_line(lines: &Arc<Mutex<Lines>>, line: String) {
    let line = truncate(&line, LINE_MAX);
    let mut l = lines.lock().unwrap();
    l.buf.push(line);
    if l.buf.len() > MAX_PROC_LINES {
        let overflow = l.buf.len() - MAX_PROC_LINES;
        l.buf.drain(0..overflow);
        l.dropped += overflow;
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(not(windows))]
fn spawn(
    root: &Path,
    command: &str,
    status: Arc<Mutex<Status>>,
    lines: Arc<Mutex<Lines>>,
) -> anyhow::Result<Option<u32>> {
    use std::process::Stdio;

    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0); // own group → stop_process can kill the whole tree
    if let Some(rs) = crate::sandbox::ruleset_for(root) {
        let mut rs = Some(rs);
        // SAFETY: runs in the forked child before exec; landlock/prctl syscalls only.
        unsafe {
            cmd.pre_exec(move || match rs.take() {
                Some(r) => crate::sandbox::restrict_current(r),
                None => Ok(()),
            });
        }
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if let Some(out) = stdout {
        let lines = lines.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(out).lines();
            while let Ok(Some(line)) = r.next_line().await {
                push_line(&lines, line);
            }
        });
    }
    if let Some(err) = stderr {
        let lines = lines.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(err).lines();
            while let Ok(Some(line)) = r.next_line().await {
                push_line(&lines, line);
            }
        });
    }
    tokio::spawn(async move {
        match child.wait().await {
            Ok(s) => {
                let mut st = status.lock().unwrap();
                if !st.is_terminal() {
                    *st = Status::Exited(s.code().unwrap_or(-1));
                }
            }
            Err(e) => *status.lock().unwrap() = Status::Failed(e.to_string()),
        }
    });
    Ok(pid)
}

#[cfg(windows)]
fn spawn(
    root: &Path,
    command: &str,
    status: Arc<Mutex<Status>>,
    lines: Arc<Mutex<Lines>>,
) -> anyhow::Result<Option<u32>> {
    use std::process::Stdio;

    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Some(out) = stdout {
        let lines = lines.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(out).lines();
            while let Ok(Some(line)) = r.next_line().await {
                push_line(&lines, line);
            }
        });
    }
    if let Some(err) = stderr {
        let lines = lines.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(err).lines();
            while let Ok(Some(line)) = r.next_line().await {
                push_line(&lines, line);
            }
        });
    }
    tokio::spawn(async move {
        match child.wait().await {
            Ok(s) => {
                let mut st = status.lock().unwrap();
                if !st.is_terminal() {
                    *st = Status::Exited(s.code().unwrap_or(-1));
                }
            }
            Err(e) => *status.lock().unwrap() = Status::Failed(e.to_string()),
        }
    });
    Ok(pid)
}

#[cfg(not(windows))]
fn kill_group(pid: u32) {
    // Negative pid → the whole process group (the child is a group leader via process_group(0)).
    unsafe {
        libc::killpg(pid as i32, libc::SIGTERM);
    }
    // Give it a moment, then force-kill anything still alive.
    let g = pid as i32;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe {
            libc::killpg(g, libc::SIGKILL);
        }
    });
}

#[cfg(windows)]
fn kill_group(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pa-proc-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        dunce::canonicalize(&d).unwrap()
    }

    async fn wait_terminal(reg: &ProcRegistry, root: &Path, id: &str) {
        for _ in 0..100 {
            let s = reg.read(root, id, None, None).unwrap();
            if !s.contains("status=running") {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn captures_output_and_exit() {
        let reg = ProcRegistry::new();
        let root = tmp();
        let msg = reg
            .start(&root, "echo hello-bg; echo second")
            .await
            .unwrap();
        assert!(msg.contains("bg1"));
        wait_terminal(&reg, &root, "bg1").await;
        let out = reg.read(&root, "bg1", None, None).unwrap();
        assert!(out.contains("hello-bg"), "output was: {out}");
        assert!(out.contains("second"));
        assert!(out.contains("status=exited(0)"), "status was: {out}");
    }

    #[tokio::test]
    async fn lists_only_its_own_root() {
        let reg = ProcRegistry::new();
        let a = tmp().join("a");
        let b = tmp().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (a, b) = (
            dunce::canonicalize(&a).unwrap(),
            dunce::canonicalize(&b).unwrap(),
        );
        reg.start(&a, "echo in-a").await.unwrap();
        wait_terminal(&reg, &a, "bg1").await;
        // b sees nothing; b cannot read a's process.
        assert!(reg.list(&b).contains("no background"));
        assert!(reg.read(&b, "bg1", None, None).is_err());
        assert!(reg.list(&a).contains("bg1"));
    }

    #[tokio::test]
    async fn stop_terminates_long_runner() {
        let reg = ProcRegistry::new();
        let root = tmp();
        reg.start(&root, "sleep 30").await.unwrap();
        let listed = reg.list(&root);
        assert!(listed.contains("running"), "listed: {listed}");
        let msg = reg.stop(&root, "bg1").unwrap();
        assert!(msg.contains("Stopped") || msg.contains("already"));
        wait_terminal(&reg, &root, "bg1").await;
        let out = reg.read(&root, "bg1", None, None).unwrap();
        assert!(
            out.contains("killed") || out.contains("exited"),
            "status was: {out}"
        );
    }
}
