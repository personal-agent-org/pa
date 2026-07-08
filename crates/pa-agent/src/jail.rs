//! The coding capability: workspace-jailed tools the agent announces to Personal Agent.
//!
//! Every path is resolved and must stay under the workspace root — `..`/symlink escapes
//! are rejected. `run_command` runs in the workspace with a timeout and bounded output.
//! This is the agent-side safety boundary (the server also gates device tools).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

const MAX_OUTPUT: usize = 60_000; // chars returned to the model
const MAX_READ_BYTES: u64 = 5_000_000;
const READ_DEFAULT_LIMIT: usize = 2000;
const GREP_MAX_MATCHES: usize = 200;
const GREP_MAX_FILE: u64 = 1_000_000;
const GLOB_MAX_FILES: usize = 500;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

const IGNORE_DIRS: &[&str] = &[
    ".git",
    ".personal-agent-snap",
    ".personal-agent",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    ".mypy_cache",
    "dist",
    "build",
    ".next",
    ".cargo",
];

/// What the agent announces to Personal Agent (name + JSON-schema params + write flag).
pub fn tool_specs() -> Value {
    json!([
        {
            "name": "run_command", "write": true,
            "description": "Run a shell command in the workspace and return its output.",
            "parameters": {"type": "object", "properties": {
                "command": {"type": "string", "description": "The shell command to run."}
            }, "required": ["command"]}
        },
        {
            "name": "list_dir", "write": false,
            "description": "List the entries of a directory in the workspace.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Relative path (default '.')."}
            }}
        },
        {
            "name": "read_file", "write": false,
            "description": "Read a text file in the workspace. Lines are numbered. For large files \
                the result is paged: if the tail says 'Call read_file again with offset=N', call \
                it again with that offset to read the next part.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "description": "1-based start line (default 1)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)."}
            }, "required": ["path"]}
        },
        {
            "name": "write_file", "write": true,
            "description": "Create or overwrite a text file in the workspace.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"}, "content": {"type": "string"}
            }, "required": ["path", "content"]}
        },
        {
            "name": "edit_file", "write": true,
            "description": "Replace a string in a workspace file. By default the old string must be \
                UNIQUE (otherwise pass replace_all=true).",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence."}
            }, "required": ["path", "old", "new"]}
        },
        {
            "name": "multi_edit", "write": true,
            "description": "Apply several string edits to one file atomically (all or nothing), in \
                order. Each edit: {old, new, replace_all?}.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"},
                "edits": {"type": "array", "items": {"type": "object", "properties": {
                    "old": {"type": "string"}, "new": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                }, "required": ["old", "new"]}}
            }, "required": ["path", "edits"]}
        },
        {
            "name": "grep", "write": false,
            "description": "Search file contents (regex) recursively under a path. Returns \
                path:line: text (context lines use path:line- ). Skips VCS/build dirs + \
                binary/large files. Results are paged: if the tail says 'Call grep again with \
                offset=N', call it again with that offset for more matches.",
            "parameters": {"type": "object", "properties": {
                "pattern": {"type": "string", "description": "A regular expression."},
                "path": {"type": "string", "description": "Subdirectory to search (default '.')."},
                "glob": {"type": "string", "description": "Only search files matching this glob, \
                    e.g. '**/*.rs'."},
                "context": {"type": "integer", "description": "Lines of context before/after each \
                    match (like grep -C, max 10)."},
                "offset": {"type": "integer", "description": "Skip this many matches (for paging \
                    through a large result set)."}
            }, "required": ["pattern"]}
        },
        {
            "name": "glob", "write": false,
            "description": "Find files by glob pattern (e.g. '**/*.rs') under the workspace.",
            "parameters": {"type": "object", "properties": {
                "pattern": {"type": "string"}
            }, "required": ["pattern"]}
        },
        {
            "name": "run_background", "write": true,
            "description": "Start a LONG-RUNNING command in the background (a dev server, a file \
                watcher, a long build) that KEEPS RUNNING after this call returns. Returns a process \
                id. Use read_process to read its output, list_processes to see what's running, and \
                stop_process to stop it. For commands that finish quickly, use run_command instead \
                (it waits and returns the full output).",
            "parameters": {"type": "object", "properties": {
                "command": {"type": "string", "description": "The shell command to start."}
            }, "required": ["command"]}
        },
        {
            "name": "list_processes", "write": false,
            "description": "List the background processes started in this workspace, with their id, \
                status (running/exited/killed), uptime and captured-line count.",
            "parameters": {"type": "object", "properties": {}}
        },
        {
            "name": "read_process", "write": false,
            "description": "Read the captured output of a background process by id. Returns the last \
                lines by default (a tail); pass offset=N to page forward from absolute line N (the \
                tail line says when there is more).",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "string", "description": "The process id from run_background."},
                "tail": {"type": "integer", "description": "How many lines to return (default 200)."},
                "offset": {"type": "integer", "description": "Absolute start line for forward paging."}
            }, "required": ["id"]}
        },
        {
            "name": "stop_process", "write": true,
            "description": "Stop a background process by id (terminates its whole process group). \
                Its captured output stays readable with read_process.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "string", "description": "The process id from run_background."}
            }, "required": ["id"]}
        },
        {
            "name": "diagnostics", "write": false,
            "description": "Type/lint/compile errors for a file from its language server \
                (LSP). Use after editing to verify the file is clean. Returns error + \
                warning lines, or '(keine Diagnosen)'. No-op if no server is installed.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "File to check."}
            }, "required": ["path"]}
        },
        {
            "name": "lsp", "write": false,
            "description": "Semantic code intelligence via the language server. \
                'definition'/'references'/'hover'/'symbols' analyze ONE file at a {line,character} \
                (1-based). 'incoming_calls'/'outgoing_calls' give the call hierarchy at a \
                {path,line,character}: who CALLS this function vs. what it CALLS. \
                'workspace_symbols' searches the WHOLE project for a symbol by NAME ({path, query}) \
                — use it to find where a function/class/type is DEFINED (real symbols, not text). \
                Needs the server for that language installed (rust-analyzer .rs, pyright .py, \
                tsserver .ts/.js, gopls .go, clangd .c/.cpp); if missing it errors — then fall back \
                to grep. Use grep for text/regex matches.",
            "parameters": {"type": "object", "properties": {
                "op": {"type": "string", "enum": ["definition", "references", "hover", "symbols", "workspace_symbols", "incoming_calls", "outgoing_calls"]},
                "path": {"type": "string", "description": "An EXISTING file in the workspace. For \
                    'workspace_symbols' only its extension matters — pass any file of the language \
                    to search (e.g. 'backend/src/main.py' for Python)."},
                "query": {"type": "string", "description": "Symbol name to search project-wide. \
                    Required for 'workspace_symbols'; ignored by the other ops."},
                "line": {"type": "integer", "description": "1-based line (omit for 'symbols'/'workspace_symbols')."},
                "character": {"type": "integer", "description": "1-based column (omit for 'symbols'/'workspace_symbols')."}
            }, "required": ["op", "path"]}
        },
        {
            "name": "apply_patch", "write": true,
            "description": "Apply changes to SEVERAL files ATOMICALLY (all-or-nothing): every \
                change is validated first, then written together. Each change is one of \
                {path, action:'add', content}, {path, action:'update', edits:[{old,new,replace_all?}]}, \
                or {path, action:'delete'}. Prefer this for coordinated multi-file edits.",
            "parameters": {"type": "object", "properties": {
                "changes": {"type": "array", "items": {"type": "object", "properties": {
                    "path": {"type": "string"},
                    "action": {"type": "string", "enum": ["add", "update", "delete"]},
                    "content": {"type": "string"},
                    "edits": {"type": "array", "items": {"type": "object", "properties": {
                        "old": {"type": "string"}, "new": {"type": "string"},
                        "replace_all": {"type": "boolean"}
                    }, "required": ["old", "new"]}}
                }, "required": ["path", "action"]}}
            }, "required": ["changes"]}
        }
        // NOTE: home_search/home_read are deliberately NOT announced here. They are dispatchable
        // RPC ops (see HomeIndex), signalled by the `home_index` capability in the hello frame.
        // The backend exposes them through ONE generic, device-targeted search_files/read_file
        // tool (not a per-device wrapper), so multiple devices don't multiply the tool list.
    ])
}

#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    // When false, path confinement is OFF (full filesystem) — the device's local, authoritative
    // choice. `root` is then just the default working directory. Default true (jailed).
    jailed: bool,
    // Shared background-process registry (one per connection, on CodingWorkspaces). None for a
    // standalone workspace (tests / no coding session) → run_background returns an error there.
    procs: Option<Arc<crate::proc::ProcRegistry>>,
}

/// Lexically normalize a path (resolve `.`/`..` without touching the filesystem).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_ignored_dir(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| IGNORE_DIRS.contains(&n))
}

/// Render a path for OUTPUT to the caller with POSIX (`/`) separators on every platform.
/// The agent + the tests expect `src/x.rs`, not Windows' `src\x.rs`. Only ever used on
/// DISPLAY/return values — never on a path handed to the OS or git.
fn rel_display(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

impl Workspace {
    pub fn new(root: &str) -> Result<Self> {
        let expanded = if let Some(stripped) = root.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(stripped)
        } else {
            PathBuf::from(root)
        };
        std::fs::create_dir_all(&expanded)?;
        Ok(Self {
            root: dunce::canonicalize(&expanded)?,
            jailed: true,
            procs: None,
        })
    }

    /// Build a workspace at an ALREADY-existing, canonicalized root (no create_dir_all) — used by
    /// CodingWorkspaces for a user-chosen project folder that already exists on disk.
    pub fn at(root: PathBuf) -> Self {
        Self {
            root,
            jailed: true,
            procs: None,
        }
    }

    /// Set the path-jail mode (device-authoritative; from the local config). Off → full filesystem.
    pub fn with_jail(mut self, jailed: bool) -> Self {
        self.jailed = jailed;
        self
    }

    /// Attach the shared background-process registry (set by CodingWorkspaces on every resolved
    /// workspace so `run_background` and friends share one registry across RPC calls).
    pub fn with_procs(mut self, procs: Option<Arc<crate::proc::ProcRegistry>>) -> Self {
        self.procs = procs;
        self
    }

    /// Whether path confinement is active.
    pub fn is_jailed(&self) -> bool {
        self.jailed
    }

    pub fn root_display(&self) -> String {
        self.root.display().to_string()
    }

    /// The canonical workspace root (used to bound where new projects/clones may be provisioned).
    pub fn root_path(&self) -> PathBuf {
        self.root.clone()
    }

    /// The shared background-process registry, or an error when this workspace has none (a
    /// standalone workspace built outside a coding session).
    fn procs(&self) -> Result<&Arc<crate::proc::ProcRegistry>> {
        self.procs
            .as_ref()
            .ok_or_else(|| anyhow!("background processes are not available in this workspace"))
    }

    /// An ISOLATED per-chat workspace under the base root, so concurrent chats on the same
    /// device never clobber each other's files or shadow-git snapshots. When the base is a
    /// git repo it's a real **git worktree** (own branch + working tree, shares the object
    /// store — cheap); otherwise a plain subdirectory. Lives at `.personal-agent/wt/<chat>` (under
    /// the jail, excluded from the user's repo status). Idempotent; falls back to the base
    /// root on an empty `chat`. Unknown to old agents — they ignore the `workspace` arg and
    /// keep sharing the base root (backward compatible).
    pub fn for_chat(&self, chat: &str) -> Result<Workspace> {
        let id = sanitize_chat(chat);
        if id.is_empty() {
            return Ok(self.clone());
        }
        let sub = self.root.join(".personal-agent").join("wt").join(&id);
        if sub.is_dir() {
            return Ok(Self {
                root: dunce::canonicalize(&sub)?,
                jailed: self.jailed,
                procs: self.procs.clone(),
            });
        }
        if let Some(parent) = sub.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = self.exclude_personal_agent(); // keep worktrees out of `git status` in the base repo
        let provisioned = if self.root.join(".git").exists() && git_available() {
            let branch = format!("personal-agent/chat-{id}");
            std::process::Command::new("git")
                .current_dir(&self.root)
                .args(["worktree", "add", "-B", &branch])
                .arg(&sub)
                .arg("HEAD")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        } else {
            false
        };
        if !provisioned && !sub.is_dir() {
            // No git / empty repo / lost a provisioning race → a plain isolated subdir.
            std::fs::create_dir_all(&sub)?;
        }
        Ok(Self {
            root: dunce::canonicalize(&sub)?,
            jailed: self.jailed,
            procs: self.procs.clone(),
        })
    }

    fn exclude_personal_agent(&self) -> Result<()> {
        let p = self.root.join(".git").join("info").join("exclude");
        if p.exists() {
            let cur = std::fs::read_to_string(&p).unwrap_or_default();
            if !cur.contains("/.personal-agent/") {
                std::fs::write(&p, format!("{}\n/.personal-agent/\n", cur.trim_end()))?;
            }
        }
        Ok(())
    }

    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let rel = if rel.is_empty() { "." } else { rel };
        // Unjailed (device's local choice): no confinement. An absolute path is used as-is
        // (`join` of an absolute path replaces the base); a relative path resolves under `root`
        // as the default working directory. `..`/symlinks are allowed — full filesystem access.
        if !self.jailed {
            return Ok(normalize(&self.root.join(rel)));
        }
        let norm = normalize(&self.root.join(rel));
        if norm == self.root {
            return Ok(norm);
        }
        if !norm.starts_with(&self.root) {
            bail!("path escapes workspace: {rel}");
        }
        if let Some(parent) = norm.parent() {
            if parent.exists() {
                let real = dunce::canonicalize(parent)?;
                if real != self.root && !real.starts_with(&self.root) {
                    bail!("path escapes workspace via symlink: {rel}");
                }
            }
        }
        Ok(norm)
    }

    pub async fn execute(&self, tool: &str, args: &Value) -> Result<String> {
        match tool {
            "run_command" => self.run(arg_str(args, "command")?).await,
            "list_dir" => self.list(args.get("path").and_then(Value::as_str).unwrap_or(".")),
            "read_file" => self.read(
                arg_str(args, "path")?,
                args.get("offset")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
                args.get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
            ),
            "write_file" => {
                self.write(arg_str(args, "path")?, arg_str(args, "content")?)
                    .await
            }
            "edit_file" => {
                self.edit(
                    arg_str(args, "path")?,
                    arg_str(args, "old")?,
                    arg_str(args, "new")?,
                    args.get("replace_all")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
                .await
            }
            "multi_edit" => {
                self.multi_edit(arg_str(args, "path")?, args.get("edits"))
                    .await
            }
            "grep" => self.grep(
                arg_str(args, "pattern")?,
                args.get("path").and_then(Value::as_str).unwrap_or("."),
                args.get("glob").and_then(Value::as_str),
                args.get("context").and_then(Value::as_u64).unwrap_or(0) as usize,
                args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
            ),
            "glob" => self.glob(arg_str(args, "pattern")?),
            "run_background" => {
                self.procs()?
                    .start(&self.root, arg_str(args, "command")?)
                    .await
            }
            "list_processes" => Ok(self.procs()?.list(&self.root)),
            "read_process" => self.procs()?.read(
                &self.root,
                arg_str(args, "id")?,
                args.get("tail").and_then(Value::as_u64).map(|n| n as usize),
                args.get("offset")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
            ),
            "stop_process" => self.procs()?.stop(&self.root, arg_str(args, "id")?),
            "diagnostics" => self.diagnostics(arg_str(args, "path")?).await,
            "lsp" => {
                let op = arg_str(args, "op")?;
                let line = args.get("line").and_then(Value::as_i64).unwrap_or(1);
                let character = args.get("character").and_then(Value::as_i64).unwrap_or(1);
                match op {
                    "workspace_symbols" => {
                        self.lsp_workspace_symbols(arg_str(args, "path")?, arg_str(args, "query")?)
                            .await
                    }
                    "incoming_calls" | "outgoing_calls" => {
                        self.lsp_call_hierarchy(
                            arg_str(args, "path")?,
                            line,
                            character,
                            op == "outgoing_calls",
                        )
                        .await
                    }
                    _ => {
                        self.lsp_nav(op, arg_str(args, "path")?, line, character)
                            .await
                    }
                }
            }
            "apply_patch" => self.apply_patch(args.get("changes")).await,
            // UI/backend-only: shadow-git snapshots for undo/revert of agent changes.
            "snap_create" => self.snap_create(),
            "snap_restore" => self.snap_restore(arg_str(args, "tree")?),
            // UI-only workspace methods (NOT announced in tool_specs → not LLM tools).
            // They back the coding-mode Monaco editor: raw content + structured listing.
            "fs_list" => self.fs_list(args.get("path").and_then(Value::as_str).unwrap_or(".")),
            "fs_read" => self.fs_read(arg_str(args, "path")?),
            "fs_write" => self.fs_write(arg_str(args, "path")?, arg_str(args, "content")?),
            other => bail!("unknown tool: {other}"),
        }
    }

    async fn run(&self, command: &str) -> Result<String> {
        if command.trim().is_empty() {
            bail!("empty command");
        }
        // Shell per OS: cmd.exe on Windows, sh elsewhere (Linux/macOS).
        #[cfg(windows)]
        {
            let mut cmd = tokio::process::Command::new("cmd");
            cmd.arg("/C").arg(command);
            cmd.current_dir(&self.root);
            // Kill the child if the timeout drops the output future, so a slow/hung command
            // can't outlive the call (the Unix branch kills the whole process group below).
            cmd.kill_on_drop(true);
            let output = match tokio::time::timeout(DEFAULT_TIMEOUT, cmd.output()).await {
                Ok(res) => res?,
                Err(_) => bail!("command timed out after {}s", DEFAULT_TIMEOUT.as_secs()),
            };
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            if !output.stderr.is_empty() {
                text.push_str("\n[stderr]\n");
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            let code = output.status.code().unwrap_or(-1);
            return Ok(cap(format!("(exit {code})\n{text}").trim().to_string()));
        }
        // Unix: the call completes when the SHELL exits, not when the output pipes hit
        // EOF — a backgrounded daemon (`uvicorn &`) inheriting the pipe must not hold
        // the call open for the full timeout. The command runs in its OWN process
        // group; on timeout the whole group is killed so no orphan survives to squat
        // on ports the next attempt needs.
        #[cfg(not(windows))]
        {
            use std::process::Stdio;
            use std::sync::{Arc, Mutex};

            use tokio::io::AsyncReadExt;

            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd.current_dir(&self.root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            // OS sandbox (opt-in): confine the command's WRITES to the workspace + build
            // caches via landlock. Built here in the parent; the forked child applies it
            // just before exec. None when disabled/unsupported → no confinement (the
            // path-jail still applies) — fail-safe so the agent is never bricked.
            if let Some(rs) = crate::sandbox::ruleset_for(&self.root) {
                let mut rs = Some(rs);
                // SAFETY: the closure runs in the forked child before exec; restrict_self()
                // is just prctl(2) + landlock syscalls (async-signal-safe), no allocation.
                unsafe {
                    cmd.pre_exec(move || match rs.take() {
                        Some(r) => crate::sandbox::restrict_current(r),
                        None => Ok(()),
                    });
                }
            }
            let mut child = cmd.spawn()?;
            let pid = child.id();

            fn drain<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
                mut r: R,
                buf: Arc<Mutex<Vec<u8>>>,
            ) -> tokio::task::JoinHandle<()> {
                tokio::spawn(async move {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match r.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                        }
                    }
                })
            }
            let out_buf = Arc::new(Mutex::new(Vec::new()));
            let err_buf = Arc::new(Mutex::new(Vec::new()));
            let mut out_task = drain(child.stdout.take().expect("piped stdout"), out_buf.clone());
            let mut err_task = drain(child.stderr.take().expect("piped stderr"), err_buf.clone());

            let status = match tokio::time::timeout(DEFAULT_TIMEOUT, child.wait()).await {
                Ok(res) => res?,
                Err(_) => {
                    if let Some(pid) = pid {
                        unsafe {
                            libc::killpg(pid as i32, libc::SIGKILL);
                        }
                    }
                    let _ = child.kill().await;
                    out_task.abort();
                    err_task.abort();
                    bail!(
                        "command timed out after {}s (process group killed)",
                        DEFAULT_TIMEOUT.as_secs()
                    );
                }
            };
            // Shell exited: give the pipes a short grace to flush, then take what's
            // there — never wait for EOF (a surviving background child may hold it).
            let grace = Duration::from_millis(250);
            let _ = tokio::time::timeout(grace, async {
                let _ = (&mut out_task).await;
                let _ = (&mut err_task).await;
            })
            .await;
            // Drop the read ends — an unredirected background child must not keep
            // an unbounded drain task (and buffer) alive for its whole lifetime.
            out_task.abort();
            err_task.abort();
            let stdout_bytes = out_buf.lock().unwrap().clone();
            let stderr_bytes = err_buf.lock().unwrap().clone();
            let mut text = String::from_utf8_lossy(&stdout_bytes).into_owned();
            if !stderr_bytes.is_empty() {
                text.push_str("\n[stderr]\n");
                text.push_str(&String::from_utf8_lossy(&stderr_bytes));
            }
            let code = status.code().unwrap_or(-1);
            Ok(cap(format!("(exit {code})\n{text}").trim().to_string()))
        }
    }

    fn list(&self, rel: &str) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_dir() {
            bail!("not a directory: {rel}");
        }
        let mut entries: Vec<String> = std::fs::read_dir(&target)?
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        entries.sort();
        Ok(if entries.is_empty() {
            "(empty)".into()
        } else {
            entries.join("\n")
        })
    }

    fn read(&self, rel: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        if target.metadata()?.len() > MAX_READ_BYTES {
            bail!("file too large (>5 MB) — use grep or read with offset/limit");
        }
        let text = std::fs::read_to_string(&target)?;
        let total = text.lines().count();
        let start = offset.unwrap_or(1).max(1);
        let limit = limit.unwrap_or(READ_DEFAULT_LIMIT);
        let mut out = String::new();
        let mut last = start.saturating_sub(1); // last line number actually emitted
        for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            let formatted = format!("{}\t{}\n", i + 1, line);
            // Stop at the char budget so the continuation offset stays accurate (always emit ≥1).
            if !out.is_empty() && out.len() + formatted.len() > MAX_OUTPUT {
                break;
            }
            out.push_str(&formatted);
            last = i + 1;
        }
        if out.is_empty() {
            return Ok("(no lines in range)".into());
        }
        // More lines remain (line limit or char budget reached) → tell the model the next offset.
        if last < total {
            out.push_str(&format!(
                "\n[pagination: lines {}-{} of {} shown. \
                 Call read_file again with offset={} to continue.]",
                start,
                last,
                total,
                last + 1
            ));
        }
        Ok(out)
    }

    async fn write(&self, rel: &str, content: &str) -> Result<String> {
        let target = self.resolve(rel)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
        let mut out = format!("wrote {} chars to {rel}", content.len());
        out.push_str(&self.post_write(rel, &target).await);
        Ok(out)
    }

    async fn edit(&self, rel: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        let text = std::fs::read_to_string(&target)?;
        let res = apply_edit(&text, old, new, replace_all)?;
        std::fs::write(&target, &res.text)?;
        let mut out = res.summary(rel);
        out.push_str(&self.post_write(rel, &target).await);
        Ok(out)
    }

    async fn multi_edit(&self, rel: &str, edits: Option<&Value>) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        let edits = edits
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("edits must be an array"))?;
        let mut text = std::fs::read_to_string(&target)?;
        let (mut removed, mut added) = (0usize, 0usize);
        for (i, e) in edits.iter().enumerate() {
            let old = e
                .get("old")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("edit {i}: old"))?;
            let new = e
                .get("new")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("edit {i}: new"))?;
            let all = e
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let res = apply_edit(&text, old, new, all).map_err(|err| anyhow!("edit {i}: {err}"))?;
            removed += res.removed;
            added += res.added;
            text = res.text;
        }
        std::fs::write(&target, &text)?;
        let mut out = format!(
            "applied {} edits to {rel} (-{removed}/+{added})",
            edits.len()
        );
        out.push_str(&self.post_write(rel, &target).await);
        Ok(out)
    }

    /// After an LLM write: auto-format (project-aware) then append LSP diagnostics so
    /// the model fixes its own errors in the same turn. Best-effort — never fails.
    async fn post_write(&self, rel: &str, target: &Path) -> String {
        let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
        let mut suffix = String::new();
        if let Some(tool) = self.format_file(rel, ext).await {
            suffix.push_str(&format!("\n[formatiert mit {tool}]"));
        }
        let root = self.root.to_string_lossy().to_string();
        let abs = target.to_string_lossy().to_string();
        suffix.push_str(&crate::lsp::diagnostics_feedback(&root, &abs, rel).await);
        suffix
    }

    /// Run the project's formatter for this file in place, if one applies + is present.
    async fn format_file(&self, rel: &str, ext: &str) -> Option<&'static str> {
        match ext {
            "go" => self.run_fmt("gofmt", &["-w", rel]).await.then_some("gofmt"),
            "rs" => self
                .run_fmt("rustfmt", &["--edition", "2021", rel])
                .await
                .then_some("rustfmt"),
            "py" if self.has_marker(&["pyproject.toml:[tool.ruff", "ruff.toml", ".ruff.toml"]) => {
                self.run_fmt("ruff", &["format", rel])
                    .await
                    .then_some("ruff")
            }
            "py" if self.has_marker(&["pyproject.toml:[tool.black"]) => {
                self.run_fmt("black", &["-q", rel]).await.then_some("black")
            }
            "ts" | "tsx" | "js" | "jsx" | "json" | "css" | "scss" | "md" | "yaml" | "yml"
                if self.has_prettier() =>
            {
                self.run_fmt("prettier", &["--write", rel])
                    .await
                    .then_some("prettier")
            }
            _ => None,
        }
    }

    async fn run_fmt(&self, prog: &str, args: &[&str]) -> bool {
        match tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(prog)
                .args(args)
                .current_dir(&self.root)
                .output(),
        )
        .await
        {
            Ok(Ok(o)) => o.status.success(),
            _ => false, // missing tool / timeout → just skip formatting
        }
    }

    /// True if any marker exists: a bare filename, or "file:needle" (file contains needle).
    fn has_marker(&self, markers: &[&str]) -> bool {
        for m in markers {
            if let Some((file, needle)) = m.split_once(':') {
                if let Ok(text) = std::fs::read_to_string(self.root.join(file)) {
                    if text.contains(needle) {
                        return true;
                    }
                }
            } else if self.root.join(m).exists() {
                return true;
            }
        }
        false
    }

    fn has_prettier(&self) -> bool {
        self.has_marker(&[
            ".prettierrc",
            ".prettierrc.json",
            ".prettierrc.js",
            ".prettierrc.yaml",
            ".prettierrc.yml",
            "prettier.config.js",
            "package.json:\"prettier\"",
        ])
    }

    async fn diagnostics(&self, rel: &str) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        let root = self.root.to_string_lossy().to_string();
        let abs = target.to_string_lossy().to_string();
        match crate::lsp::diagnostics(&root, &abs).await {
            Some(d) if !d.trim().is_empty() => Ok(format!("Diagnosen für {rel}:\n{d}")),
            Some(_) => Ok(format!("{rel}: keine Diagnosen (sauber).")),
            None => Ok(format!("Kein Language-Server für {rel} verfügbar.")),
        }
    }

    async fn lsp_nav(&self, op: &str, rel: &str, line: i64, character: i64) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        let root = self.root.to_string_lossy().to_string();
        let abs = target.to_string_lossy().to_string();
        crate::lsp::navigate(&root, &abs, op, line, character).await
    }

    /// Project-wide symbol search (`workspace/symbol`). `rel` is any file of the target
    /// language — its extension picks the server; `query` is the symbol name.
    async fn lsp_workspace_symbols(&self, rel: &str, query: &str) -> Result<String> {
        if query.trim().is_empty() {
            // Empty queries make rust-analyzer/gopls return nothing — fail fast & clearly.
            bail!("'query' (the symbol name to search) must not be empty");
        }
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        let root = self.root.to_string_lossy().to_string();
        let abs = target.to_string_lossy().to_string();
        crate::lsp::workspace_symbols(&root, &abs, query).await
    }

    /// Call hierarchy at a position: incoming callers (`outgoing=false`) or outgoing
    /// callees (`outgoing=true`) of the symbol under the cursor.
    async fn lsp_call_hierarchy(
        &self,
        rel: &str,
        line: i64,
        character: i64,
        outgoing: bool,
    ) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        let root = self.root.to_string_lossy().to_string();
        let abs = target.to_string_lossy().to_string();
        crate::lsp::call_hierarchy(&root, &abs, line, character, outgoing).await
    }

    /// Apply several file changes atomically: validate/compute all in memory first,
    /// then write/delete together (a single failure aborts before any write).
    async fn apply_patch(&self, changes: Option<&Value>) -> Result<String> {
        enum Op {
            Write(PathBuf, String),
            Delete(PathBuf),
        }
        let changes = changes
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("changes must be an array"))?;
        let mut ops: Vec<(String, Op)> = Vec::new();
        for (i, c) in changes.iter().enumerate() {
            let path = c
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("change {i}: path"))?;
            let action = c
                .get("action")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("change {i}: action"))?;
            let target = self.resolve(path)?;
            match action {
                "add" => {
                    let content = c
                        .get("content")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("change {i}: 'content' required for add"))?;
                    ops.push((path.into(), Op::Write(target, content.to_string())));
                }
                "update" => {
                    if !target.is_file() {
                        bail!("change {i}: not a file: {path}");
                    }
                    let mut text = std::fs::read_to_string(&target)?;
                    if let Some(content) = c.get("content").and_then(Value::as_str) {
                        text = content.to_string();
                    } else {
                        let edits = c
                            .get("edits")
                            .and_then(Value::as_array)
                            .ok_or_else(|| anyhow!("change {i}: 'edits' or 'content' required"))?;
                        for (j, e) in edits.iter().enumerate() {
                            let old = e
                                .get("old")
                                .and_then(Value::as_str)
                                .ok_or_else(|| anyhow!("change {i}.{j}: old"))?;
                            let new = e
                                .get("new")
                                .and_then(Value::as_str)
                                .ok_or_else(|| anyhow!("change {i}.{j}: new"))?;
                            let all = e
                                .get("replace_all")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            text = apply_edit(&text, old, new, all)
                                .map_err(|err| anyhow!("change {i}.{j}: {err}"))?
                                .text;
                        }
                    }
                    ops.push((path.into(), Op::Write(target, text)));
                }
                "delete" => {
                    if !target.is_file() {
                        bail!("change {i}: not a file: {path}");
                    }
                    ops.push((path.into(), Op::Delete(target)));
                }
                other => bail!("change {i}: unknown action '{other}'"),
            }
        }
        // All validated → commit.
        for (_, op) in &ops {
            match op {
                Op::Write(target, content) => {
                    if let Some(p) = target.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    std::fs::write(target, content)?;
                }
                Op::Delete(target) => {
                    let _ = std::fs::remove_file(target);
                }
            }
        }
        let names: Vec<&str> = ops.iter().map(|(r, _)| r.as_str()).collect();
        let mut out = format!(
            "apply_patch: {} change(s) → {}",
            ops.len(),
            names.join(", ")
        );
        for (rel, op) in &ops {
            if let Op::Write(target, _) = op {
                let root = self.root.to_string_lossy().to_string();
                let abs = target.to_string_lossy().to_string();
                out.push_str(&crate::lsp::diagnostics_feedback(&root, &abs, rel).await);
            }
        }
        Ok(out)
    }

    // --- Shadow-git snapshots (undo/revert of agent changes). A SEPARATE git dir
    // (.personal-agent-snap) keyed to the workspace; the user's real repo is untouched. ---

    fn snap_args(&self) -> Vec<String> {
        let gd = self.root.join(".personal-agent-snap");
        vec![
            "--git-dir".into(),
            gd.to_string_lossy().into_owned(),
            "--work-tree".into(),
            self.root.to_string_lossy().into_owned(),
        ]
    }

    fn ensure_snap(&self) -> Result<()> {
        let gd = self.root.join(".personal-agent-snap");
        if !gd.join("HEAD").exists() {
            let out = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&gd)
                .arg("init")
                .arg("-q")
                .current_dir(&self.root)
                .output()?;
            if !out.status.success() {
                bail!(
                    "snapshot init failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            let _ = std::process::Command::new("git")
                .args(self.snap_args())
                .args(["config", "core.bare", "false"])
                .current_dir(&self.root)
                .output();
            let _ = std::fs::write(
                gd.join("info").join("exclude"),
                "/.personal-agent-snap/\n/.git/\n",
            );
        }
        Ok(())
    }

    fn git_snap(&self, extra: &[&str]) -> Result<std::process::Output> {
        Ok(std::process::Command::new("git")
            .args(self.snap_args())
            .args(extra)
            .current_dir(&self.root)
            .output()?)
    }

    /// Snapshot the current workspace → returns a tree hash to revert to later.
    fn snap_create(&self) -> Result<String> {
        self.ensure_snap()?;
        let add = self.git_snap(&["add", "-A"])?;
        if !add.status.success() {
            bail!(
                "snapshot add failed: {}",
                String::from_utf8_lossy(&add.stderr)
            );
        }
        let wt = self.git_snap(&["write-tree"])?;
        if !wt.status.success() {
            bail!(
                "snapshot write-tree failed: {}",
                String::from_utf8_lossy(&wt.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&wt.stdout).trim().to_string())
    }

    /// Restore the workspace to a previous snapshot tree (reverts modifications,
    /// additions and deletions made since it was taken).
    fn snap_restore(&self, tree: &str) -> Result<String> {
        self.ensure_snap()?;
        let _ = self.git_snap(&["add", "-A"])?; // track current files so additions revert
        let rt = self.git_snap(&["read-tree", "-u", "--reset", tree])?;
        if !rt.status.success() {
            bail!(
                "snapshot restore failed: {}",
                String::from_utf8_lossy(&rt.stderr)
            );
        }
        Ok(format!("restored workspace to snapshot {tree}"))
    }

    fn grep(
        &self,
        pattern: &str,
        rel: &str,
        glob: Option<&str>,
        context: usize,
        offset: usize,
    ) -> Result<String> {
        let base = self.resolve(rel)?;
        let re = regex::Regex::new(pattern).map_err(|e| anyhow!("invalid regex: {e}"))?;
        // Optional include filter on the POSIX relative path, e.g. glob="**/*.rs" (mirrors `glob`).
        let matcher = match glob {
            Some(g) => Some(
                globset::Glob::new(g)
                    .map_err(|e| anyhow!("invalid glob: {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let ctx = context.min(10); // bound the blast radius of -C
        let mut out = String::new();
        let mut total = 0usize; // total matches across the workspace (for the count + next offset)
        let mut shown = 0usize; // matches included on THIS page
        let mut truncated = false;
        for entry in walkdir::WalkDir::new(&base)
            .into_iter()
            .filter_entry(|e| !(e.file_type().is_dir() && is_ignored_dir(e.file_name())))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if entry
                .metadata()
                .map(|m| m.len() > GREP_MAX_FILE)
                .unwrap_or(true)
            {
                continue;
            }
            let relp = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path());
            let relp_str = rel_display(relp);
            if let Some(m) = &matcher {
                if !m.is_match(&relp_str) {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue; // skip binary / non-utf8
            };
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                total += 1;
                if total <= offset {
                    continue; // paging: this match is on an earlier page
                }
                let chunk = if ctx > 0 {
                    let lo = n.saturating_sub(ctx);
                    let hi = (n + ctx).min(lines.len().saturating_sub(1));
                    let mut block = String::new();
                    for c in lo..=hi {
                        let sep = if c == n { ':' } else { '-' };
                        block.push_str(&format!(
                            "{}:{}{} {}\n",
                            relp_str,
                            c + 1,
                            sep,
                            lines[c].trim_end()
                        ));
                    }
                    block.push_str("--\n");
                    block
                } else {
                    format!("{}:{}: {}\n", relp_str, n + 1, line.trim_end())
                };
                // Stop adding once the page is full (match cap OR char budget) but keep COUNTING so
                // the marker reports the true total + an accurate next offset.
                if shown >= GREP_MAX_MATCHES
                    || (!out.is_empty() && out.len() + chunk.len() > MAX_OUTPUT)
                {
                    truncated = true;
                    continue;
                }
                out.push_str(&chunk);
                shown += 1;
            }
        }
        if out.is_empty() {
            return Ok("(no matches)".into());
        }
        if truncated || total > offset + shown {
            out.push_str(&format!(
                "\n[pagination: matches {}-{} of {}. Call grep again with offset={} to continue.]\n",
                offset + 1,
                offset + shown,
                total,
                offset + shown
            ));
        }
        Ok(out)
    }

    /// UI: structured directory listing (dirs first, then files; both alphabetical).
    /// Returns JSON `[{"name","is_dir","size"}]`.
    fn fs_list(&self, rel: &str) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_dir() {
            bail!("not a directory: {rel}");
        }
        let mut items: Vec<Value> = std::fs::read_dir(&target)?
            .filter_map(|e| e.ok())
            .map(|e| {
                let md = e.metadata().ok();
                let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                let size = md.as_ref().map(std::fs::Metadata::len).unwrap_or(0);
                json!({"name": e.file_name().to_string_lossy(), "is_dir": is_dir, "size": size})
            })
            .collect();
        items.sort_by(|a, b| {
            let (ad, bd) = (
                a["is_dir"].as_bool().unwrap(),
                b["is_dir"].as_bool().unwrap(),
            );
            bd.cmp(&ad).then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            })
        });
        Ok(json!(items).to_string())
    }

    /// UI: raw file content for the editor. Returns JSON `{"content","truncated"}`.
    fn fs_read(&self, rel: &str) -> Result<String> {
        let target = self.resolve(rel)?;
        if !target.is_file() {
            bail!("not a file: {rel}");
        }
        let len = target.metadata()?.len();
        let truncated = len > MAX_READ_BYTES;
        let bytes = std::fs::read(&target)?;
        let slice = if truncated {
            &bytes[..MAX_READ_BYTES as usize]
        } else {
            &bytes[..]
        };
        let content = String::from_utf8_lossy(slice).into_owned();
        Ok(json!({"content": content, "truncated": truncated}).to_string())
    }

    /// UI: raw file write (user save). Returns JSON `{"ok": true, "bytes": n}`.
    fn fs_write(&self, rel: &str, content: &str) -> Result<String> {
        let target = self.resolve(rel)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
        Ok(json!({"ok": true, "bytes": content.len()}).to_string())
    }

    fn glob(&self, pattern: &str) -> Result<String> {
        let matcher = globset::Glob::new(pattern)
            .map_err(|e| anyhow!("invalid glob: {e}"))?
            .compile_matcher();
        let mut out: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| !(e.file_type().is_dir() && is_ignored_dir(e.file_name())))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let relp = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path());
            // Match (and emit) on the POSIX form so `**/*.rs` works and output is
            // `src/x.rs` on every platform, not Windows' `src\x.rs`.
            let rel = rel_display(relp);
            if matcher.is_match(&rel) {
                out.push(rel);
                if out.len() >= GLOB_MAX_FILES {
                    break;
                }
            }
        }
        out.sort();
        Ok(if out.is_empty() {
            "(no files)".into()
        } else {
            out.join("\n")
        })
    }
}

/// Replace `old` with `new` in `text`. Unless `replace_all`, `old` must be unique.
/// The outcome of one edit: the new text + a diff summary surfaced to the model.
struct EditResult {
    text: String,
    strategy: &'static str,
    removed: usize,
    added: usize,
}

impl EditResult {
    fn summary(&self, rel: &str) -> String {
        format!(
            "edited {rel} ({}; -{}/+{})",
            self.strategy, self.removed, self.added
        )
    }
}

/// Apply a string edit with a multi-strategy fallback cascade so near-miss matches
/// (the model's `old` drifting in whitespace/indentation) still apply instead of
/// hard-failing — the #1 cause of wasted edit turns. Strategies, in order:
///   1. exact byte match (unique unless `replace_all`)
///   2. line-trimmed (each line equal after trimming)
///   3. whitespace-normalized (internal runs of whitespace collapsed)
///   4. block-anchor (first+last line match; middle may have drifted), ≥3 lines
fn apply_edit(text: &str, old: &str, new: &str, replace_all: bool) -> Result<EditResult> {
    if old == new {
        bail!("old and new are identical");
    }
    // Stage 1: exact match, preserving the unique-or-replace_all contract.
    let count = text.matches(old).count();
    if count >= 1 {
        if count > 1 && !replace_all {
            bail!(
                "old string is not unique ({count}×) — add surrounding context or set replace_all"
            );
        }
        let out = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        return Ok(make_result(out, "exact", old, new));
    }
    if old.is_empty() {
        bail!("old string not found");
    }
    // Stages 2-3: line-based fuzzy matching under a normalizer.
    for (name, norm) in [
        ("line-trimmed", normalize_trim as fn(&str) -> String),
        ("whitespace", normalize_ws as fn(&str) -> String),
    ] {
        if let Some(out) = replace_by_lines(text, old, new, replace_all, norm)? {
            return Ok(make_result(out, name, old, new));
        }
    }
    // Stage 4: block-anchor (multi-line; first & last lines anchor the span).
    if let Some(out) = replace_by_anchor(text, old, new, replace_all)? {
        return Ok(make_result(out, "anchor", old, new));
    }
    bail!("old string not found (tried exact + line-trimmed + whitespace + anchor match)")
}

fn make_result(text: String, strategy: &'static str, old: &str, new: &str) -> EditResult {
    EditResult {
        text,
        strategy,
        removed: old.split('\n').count(),
        added: new.split('\n').count(),
    }
}

fn normalize_trim(l: &str) -> String {
    l.trim().to_string()
}

fn normalize_ws(l: &str) -> String {
    l.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Replace the line span(s) of `text` matching `old` under `norm` with `new`.
fn replace_by_lines(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    norm: fn(&str) -> String,
) -> Result<Option<String>> {
    let t: Vec<&str> = text.split('\n').collect();
    let o: Vec<&str> = old.split('\n').collect();
    if o.is_empty() || o.len() > t.len() {
        return Ok(None);
    }
    let on: Vec<String> = o.iter().map(|l| norm(l)).collect();
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + o.len() <= t.len() {
        if (0..o.len()).all(|j| norm(t[i + j]) == on[j]) {
            starts.push(i);
            i += o.len(); // non-overlapping
        } else {
            i += 1;
        }
    }
    finish_line_replace(&t, &starts, o.len(), new, replace_all)
}

/// Block-anchor: a window whose first & last (trimmed) lines match `old`'s, even if
/// the middle drifted. Only for `old` of ≥3 lines with non-empty anchors.
fn replace_by_anchor(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<Option<String>> {
    let o: Vec<&str> = old.split('\n').collect();
    if o.len() < 3 {
        return Ok(None);
    }
    let (first, last) = (o[0].trim(), o[o.len() - 1].trim());
    if first.is_empty() || last.is_empty() {
        return Ok(None);
    }
    let t: Vec<&str> = text.split('\n').collect();
    if o.len() > t.len() {
        return Ok(None);
    }
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + o.len() <= t.len() {
        if t[i].trim() == first && t[i + o.len() - 1].trim() == last {
            starts.push(i);
            i += o.len();
        } else {
            i += 1;
        }
    }
    finish_line_replace(&t, &starts, o.len(), new, replace_all)
}

fn finish_line_replace(
    t: &[&str],
    starts: &[usize],
    span: usize,
    new: &str,
    replace_all: bool,
) -> Result<Option<String>> {
    if starts.is_empty() {
        return Ok(None);
    }
    if starts.len() > 1 && !replace_all {
        bail!(
            "fuzzy match is not unique ({}×) — add surrounding context or set replace_all",
            starts.len()
        );
    }
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut out: Vec<&str> = Vec::with_capacity(t.len());
    let mut i = 0;
    while i < t.len() {
        if starts.contains(&i) {
            out.extend_from_slice(&new_lines);
            i += span;
        } else {
            out.push(t[i]);
            i += 1;
        }
    }
    Ok(Some(out.join("\n")))
}

fn cap(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        s.truncate(MAX_OUTPUT);
        s.push_str("\n… (output truncated)");
    }
    s
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string arg: {key}"))
}

/// A filesystem-safe per-chat workspace id (lowercase alphanumerics + dashes, bounded).
fn sanitize_chat(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(40)
        .collect::<String>()
        .to_lowercase()
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// CodingWorkspaces — resolves the coding-mode workspace for a chat. A user-chosen folder
// (under $HOME) is edited DIRECTLY by the first chat to claim it; a CONCURRENT second chat
// on the same folder gets an isolated git worktree. A stale direct claim (idle >10 min) is
// taken over. No folder → the enrolled default workspace + per-chat worktree (legacy).
// ---------------------------------------------------------------------------

/// A direct-edit claim on a folder is taken over by another chat after this idle period, so a
/// finished session doesn't force every later chat into a worktree forever.
const WORKSPACE_CLAIM_IDLE: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct CodingWorkspaces {
    default: Workspace,
    home_root: Option<PathBuf>,
    // folder -> (chat_id holding it for DIRECT edit, last touched)
    claims: Arc<Mutex<HashMap<PathBuf, (String, Instant)>>>,
    // Background processes, shared across RPC calls for this connection's lifetime.
    procs: Arc<crate::proc::ProcRegistry>,
}

impl CodingWorkspaces {
    pub fn new(default: Workspace, home_root: Option<PathBuf>) -> Self {
        Self {
            default,
            home_root,
            claims: Arc::new(Mutex::new(HashMap::new())),
            procs: Arc::new(crate::proc::ProcRegistry::new()),
        }
    }

    pub fn default_root_display(&self) -> String {
        self.default.root_display()
    }

    /// Resolve the workspace for a chat. `folder` (when set) is a user-chosen directory under
    /// $HOME; the first/idle-takeover claimant edits it directly, a concurrent chat gets a
    /// worktree. No folder → the enrolled default workspace with the per-chat worktree.
    pub fn resolve(&self, folder: Option<&str>, chat_id: &str) -> Result<Workspace> {
        Ok(self
            .resolve_inner(folder, chat_id)?
            .with_procs(Some(self.procs.clone())))
    }

    fn resolve_inner(&self, folder: Option<&str>, chat_id: &str) -> Result<Workspace> {
        let folder = match folder {
            Some(p) if !p.trim().is_empty() => p.trim(),
            _ => return self.default.for_chat(chat_id),
        };
        let expanded = if let Some(rest) = folder.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(rest)
        } else if folder == "~" {
            dirs::home_dir().unwrap_or_default()
        } else {
            PathBuf::from(folder)
        };
        let canon =
            dunce::canonicalize(&expanded).map_err(|_| anyhow!("folder not found: {folder}"))?;
        if !canon.is_dir() {
            bail!("not a directory: {folder}");
        }
        // Bound: the chosen folder must be inside an allowed root — the user's home (a local
        // device's $HOME, which the picker browses) OR the enrolled default workspace base
        // (a cloud sandbox is rooted at /workspace, which is NOT under $HOME).
        if !self.is_under_allowed_root(&canon) {
            bail!("folder is outside the allowed workspace roots: {folder}");
        }
        let direct = {
            let mut claims = self.claims.lock().unwrap();
            let now = Instant::now();
            let take = match claims.get(&canon) {
                None => true,
                Some((holder, seen)) => {
                    holder == chat_id || now.duration_since(*seen) > WORKSPACE_CLAIM_IDLE
                }
            };
            if take {
                claims.insert(canon.clone(), (chat_id.to_string(), now));
            }
            take
        };
        // The chosen folder inherits the device's jail mode (device-authoritative).
        let ws = Workspace::at(canon).with_jail(self.default.is_jailed());
        if direct {
            Ok(ws)
        } else {
            ws.for_chat(chat_id) // concurrent chat on the same folder → isolated git worktree
        }
    }

    /// The roots a workspace folder / new-project / clone target may live under: the enrolled
    /// default workspace base (always) plus the user's home (local devices). Both are already
    /// canonical.
    fn allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.default.root_path()];
        if let Some(h) = &self.home_root {
            if !roots.contains(h) {
                roots.push(h.clone());
            }
        }
        roots
    }

    fn is_under_allowed_root(&self, canon: &Path) -> bool {
        self.allowed_roots()
            .iter()
            .any(|r| canon == r || canon.starts_with(r))
    }

    /// Validate an absolute destination for a NEW directory (mkproject / git_clone): its PARENT
    /// must already exist and resolve inside an allowed root, blocking `..`/symlink escapes and
    /// option-injection. Returns the absolute target path (parent-canonical + the new leaf name).
    fn resolve_new(&self, dest: &str) -> Result<PathBuf> {
        let dest = dest.trim();
        if dest.is_empty() || dest.starts_with('-') {
            bail!("invalid destination path");
        }
        let expanded = expand_home(dest);
        if !expanded.is_absolute() {
            bail!("destination must be an absolute path");
        }
        let norm = normalize(&expanded);
        let name = norm
            .file_name()
            .ok_or_else(|| anyhow!("destination has no final path component"))?
            .to_owned();
        let parent = norm
            .parent()
            .ok_or_else(|| anyhow!("destination has no parent"))?;
        let real_parent = dunce::canonicalize(parent)
            .map_err(|_| anyhow!("parent directory does not exist: {}", parent.display()))?;
        if !self.is_under_allowed_root(&real_parent) {
            bail!("destination is outside the allowed workspace roots: {dest}");
        }
        Ok(real_parent.join(name))
    }

    /// Create an empty new-project directory at `dest` (jailed to an allowed root). An existing
    /// EMPTY dir is accepted; a non-empty one is rejected. Best-effort `git init` so per-chat
    /// worktrees + shadow-git snapshots work naturally. Returns JSON {path, output}.
    pub async fn mkproject(&self, dest: &str) -> Result<String> {
        let target = self.resolve_new(dest)?;
        if target.exists() {
            if dir_has_entries(&target) {
                bail!(
                    "destination already exists and is not empty: {}",
                    target.display()
                );
            }
        } else {
            std::fs::create_dir_all(&target)?;
        }
        let canon = dunce::canonicalize(&target)?;
        if git_available() && !canon.join(".git").exists() {
            let _ = std::process::Command::new("git")
                .arg("init")
                .arg("-q")
                .current_dir(&canon)
                .output();
        }
        Ok(json!({"path": rel_display(&canon), "output": "created"}).to_string())
    }

    /// Clone a git repo into `dest` (jailed to an allowed root) on demand. The repo URL is the
    /// user's own input (they typed it in the frontend); it is passed after `--` and rejected if
    /// it looks like an option. `dest` must be new/empty. Returns JSON {path, output}.
    pub async fn git_clone(&self, url: &str, dest: &str, branch: Option<&str>) -> Result<String> {
        let url = url.trim();
        if url.is_empty() || url.starts_with('-') {
            bail!("invalid repository URL");
        }
        let branch = branch.map(str::trim).filter(|b| !b.is_empty());
        if let Some(b) = branch {
            if b.starts_with('-') {
                bail!("invalid branch name");
            }
        }
        if !git_available() {
            bail!("git is not installed on this device");
        }
        let target = self.resolve_new(dest)?;
        if target.exists() && dir_has_entries(&target) {
            bail!(
                "destination already exists and is not empty: {}",
                target.display()
            );
        }
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("clone");
        if let Some(b) = branch {
            cmd.arg("--branch").arg(b);
        }
        // Register THIS agent as the repo's credential helper (persisted to the clone's .git/config
        // — the HELPER command, never a token). git invokes it on demand (clone of a private repo,
        // and later fetch/push), fetching the user's token live from the backend, so the token is
        // never written to disk. Only fires when git actually needs auth (public clones skip it).
        if let Ok(exe) = std::env::current_exe() {
            cmd.arg("--config").arg(format!(
                "credential.helper=!\"{}\" credential-helper",
                exe.display()
            ));
        }
        cmd.arg("--").arg(url).arg(&target);
        cmd.env("GIT_TERMINAL_PROMPT", "0"); // never block on an interactive credential prompt
        cmd.kill_on_drop(true);
        let out = match tokio::time::timeout(GIT_CLONE_TIMEOUT, cmd.output()).await {
            Ok(res) => res?,
            Err(_) => bail!("git clone timed out after {}s", GIT_CLONE_TIMEOUT.as_secs()),
        };
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("git clone failed: {}", cap(err.trim().to_string()));
        }
        let canon = dunce::canonicalize(&target)?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(
            json!({"path": rel_display(&canon), "output": cap(text.trim().to_string())})
                .to_string(),
        )
    }
}

/// Expand a leading `~`/`~/` to the home directory; otherwise verbatim.
fn expand_home(p: &str) -> PathBuf {
    if p == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(p))
    } else if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(p))
    } else {
        PathBuf::from(p)
    }
}

fn dir_has_entries(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut d| d.next().is_some())
        .unwrap_or(true)
}

/// Cap a `git clone` (full history) below the backend's 180s RPC timeout so the device returns a
/// clear error first.
const GIT_CLONE_TIMEOUT: Duration = Duration::from_secs(170);

// ---------------------------------------------------------------------------
// HomeIndex — a SEPARATE, read-only file surface rooted at $HOME (distinct from the
// coding Workspace jail). Backs the generic, device-targeted search_files/read_file the
// backend exposes in non-coding chats. No writes, no dir creation, no per-chat worktrees.
// ---------------------------------------------------------------------------

const HOME_SEARCH_DEFAULT_LIMIT: usize = 200;
const HOME_SEARCH_MAX_LIMIT: usize = 1000;
const HOME_WALK_TIMEOUT: Duration = Duration::from_secs(8);
const HOME_INDEX_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct HomeIndex {
    root: PathBuf,
}

impl HomeIndex {
    /// Build the read-only home surface. Unlike `Workspace::new` this NEVER creates the
    /// directory — it must already exist (it's the user's home). Errors if missing.
    pub fn new(root: &str) -> Result<Self> {
        let expanded = if let Some(stripped) = root.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(stripped)
        } else {
            PathBuf::from(root)
        };
        Ok(Self {
            root: dunce::canonicalize(&expanded)?,
        })
    }

    pub fn root_display(&self) -> String {
        self.root.display().to_string()
    }

    /// The canonical home root (bounds the coding folder picker).
    pub fn root_path(&self) -> PathBuf {
        self.root.clone()
    }

    /// Resolve a user-supplied RELATIVE path, rejecting `..`/symlink escapes (same 3 checks as
    /// `Workspace::resolve`).
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let rel = if rel.is_empty() { "." } else { rel };
        let norm = normalize(&self.root.join(rel));
        if norm == self.root {
            return Ok(norm);
        }
        if !norm.starts_with(&self.root) {
            bail!("path escapes home: {rel}");
        }
        if let Some(parent) = norm.parent() {
            if parent.exists() {
                let real = dunce::canonicalize(parent)?;
                if real != self.root && !real.starts_with(&self.root) {
                    bail!("path escapes home via symlink: {rel}");
                }
            }
        }
        Ok(norm)
    }

    /// Re-validate an ALREADY-ABSOLUTE path (from the OS index or the walker): it must be a real
    /// path inside the home root after symlink resolution. Applied to every search result + every
    /// absolute read so an index that covers paths outside `$HOME` (or a symlink) can't leak out.
    fn contains(&self, abs: &Path) -> bool {
        match dunce::canonicalize(abs) {
            Ok(real) => real == self.root || real.starts_with(&self.root),
            Err(_) => false,
        }
    }

    pub async fn execute(&self, tool: &str, args: &Value) -> Result<String> {
        match tool {
            "home_search" => {
                self.search(
                    arg_str(args, "pattern")?,
                    args.get("limit")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize),
                )
                .await
            }
            "home_read" => self.read(arg_str(args, "path")?),
            "home_list" => self.list(args.get("dir").and_then(Value::as_str).unwrap_or("")),
            other => bail!("unknown home tool: {other}"),
        }
    }

    /// Read-only directory listing under home — backs the coding-mode folder picker. `dir` empty
    /// → home root. Returns JSON {dir, parent (null at home root), entries:[{name,path,is_dir}]}.
    /// Dotfiles/dirs are hidden; `parent` never points above the home root.
    fn list(&self, dir: &str) -> Result<String> {
        let target = if dir.is_empty() {
            self.root.clone()
        } else if Path::new(dir).is_absolute() {
            // Canonicalize ONCE and validate the result — no second resolution (avoids a
            // TOCTOU window where a swapped symlink could land `target` outside home).
            let canon = dunce::canonicalize(dir).map_err(|_| anyhow!("not found: {dir}"))?;
            if canon != self.root && !canon.starts_with(&self.root) {
                bail!("path is outside home: {dir}");
            }
            canon
        } else {
            self.resolve(dir)?
        };
        if !target.is_dir() {
            bail!("not a directory: {dir}");
        }
        let mut entries: Vec<Value> = std::fs::read_dir(&target)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    return None; // hide dotfiles/dirs in the picker
                }
                let md = e.metadata().ok()?;
                Some(json!({
                    "name": name,
                    "path": rel_display(&e.path()),
                    "is_dir": md.is_dir(),
                }))
            })
            .collect();
        entries.sort_by(|a, b| {
            let (ad, bd) = (
                a["is_dir"].as_bool().unwrap_or(false),
                b["is_dir"].as_bool().unwrap_or(false),
            );
            bd.cmp(&ad).then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            })
        });
        let parent = if target == self.root {
            Value::Null
        } else {
            match target.parent() {
                Some(p) if p == self.root || p.starts_with(&self.root) => {
                    json!(rel_display(p))
                }
                _ => Value::Null,
            }
        };
        Ok(json!({
            "dir": rel_display(&target),
            "parent": parent,
            "entries": entries,
        })
        .to_string())
    }

    /// Read a text file under home — absolute (re-validated by `contains`) or relative
    /// (`resolve`). Read-only, size-capped (mirrors `fs_read`).
    fn read(&self, path: &str) -> Result<String> {
        let target = if Path::new(path).is_absolute() {
            let p = PathBuf::from(path);
            if !self.contains(&p) {
                bail!("path is outside home: {path}");
            }
            p
        } else {
            self.resolve(path)?
        };
        if !target.is_file() {
            bail!("not a file: {path}");
        }
        let len = target.metadata()?.len();
        let truncated = len > MAX_READ_BYTES;
        let bytes = std::fs::read(&target)?;
        let slice = if truncated {
            &bytes[..MAX_READ_BYTES as usize]
        } else {
            &bytes[..]
        };
        let content = String::from_utf8_lossy(slice).into_owned();
        Ok(json!({
            "content": content,
            "truncated": truncated,
            "path": rel_display(&target),
        })
        .to_string())
    }

    async fn search(&self, pattern: &str, limit: Option<usize>) -> Result<String> {
        let limit = limit
            .unwrap_or(HOME_SEARCH_DEFAULT_LIMIT)
            .clamp(1, HOME_SEARCH_MAX_LIMIT);
        let candidates = self.collect_candidates(pattern, limit).await;
        let mut items: Vec<Value> = Vec::new();
        for path in candidates {
            if !self.contains(&path) {
                continue; // trust gate — every returned path must be real + inside home
            }
            if let Some(entry) = entry_json(&path) {
                items.push(entry);
            }
            if items.len() >= limit {
                break;
            }
        }
        Ok(json!(items).to_string())
    }

    /// Live OS index first (plocate/locate/mdfind); fall back to a bounded walk if it's
    /// unavailable or yields nothing inside home.
    async fn collect_candidates(&self, pattern: &str, limit: usize) -> Vec<PathBuf> {
        let native: Vec<PathBuf> = self
            .os_index_search(pattern, limit)
            .await
            .into_iter()
            .filter(|p| self.contains(p))
            .collect();
        if !native.is_empty() {
            return native;
        }
        self.walk_search(pattern, limit).await
    }

    async fn os_index_search(&self, pattern: &str, limit: usize) -> Vec<PathBuf> {
        let glob = format!("*{pattern}*");
        let lim = limit.to_string();
        let out = if cfg!(target_os = "linux") {
            let mut r = run_index(&["plocate", "-i", "-l", &lim, "--", &glob]).await;
            if r.is_none() {
                r = run_index(&["locate", "-i", "-l", &lim, "--", &glob]).await;
            }
            r
        } else if cfg!(target_os = "macos") {
            let root = self.root.display().to_string();
            run_index(&[
                "mdfind",
                "-onlyin",
                &root,
                &format!("kMDItemFSName == '{glob}'c"),
            ])
            .await
        } else {
            None // Windows: go straight to the bounded walk
        };
        match out {
            Some(text) => text
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .take(limit)
                .collect(),
            None => Vec::new(),
        }
    }

    async fn walk_search(&self, pattern: &str, limit: usize) -> Vec<PathBuf> {
        let root = self.root.clone();
        let needle = pattern.to_lowercase();
        tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + HOME_WALK_TIMEOUT;
            let mut out: Vec<PathBuf> = Vec::new();
            let walker = walkdir::WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    // depth>0 so the root itself is never pruned (e.g. a $HOME that happens to be a
                    // dotdir, or a `.tmp…` test dir).
                    if e.depth() > 0 && e.file_type().is_dir() {
                        let name = e.file_name();
                        if is_ignored_dir(name) {
                            return false;
                        }
                        // skip dotdirs/caches (.cache, .config, .mozilla, …)
                        if name.to_str().is_some_and(|n| n.starts_with('.')) {
                            return false;
                        }
                    }
                    true
                });
            for entry in walker.flatten() {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.to_lowercase().contains(&needle))
                {
                    out.push(entry.into_path());
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default()
    }
}

fn entry_json(abs: &Path) -> Option<Value> {
    let md = std::fs::symlink_metadata(abs).ok()?;
    let modified = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Some(json!({
        "path": rel_display(abs),
        "size": md.len(),
        "modified": modified,
        "is_dir": md.is_dir(),
    }))
}

/// Run an OS index binary with a timeout; `None` if the binary is missing / fails / times out.
async fn run_index(argv: &[&str]) -> Option<String> {
    let (bin, rest) = argv.split_first()?;
    let fut = tokio::process::Command::new(bin).args(rest).output();
    match tokio::time::timeout(HOME_INDEX_TIMEOUT, fut).await {
        Ok(Ok(out)) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (Workspace, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Workspace::new(dir.path().to_str().unwrap()).unwrap(), dir)
    }

    fn home() -> (HomeIndex, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (HomeIndex::new(dir.path().to_str().unwrap()).unwrap(), dir)
    }

    #[test]
    fn unjailed_reads_outside_root_while_jailed_blocks() {
        // A file OUTSIDE the workspace root.
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();
        let abs = dunce::canonicalize(&secret).unwrap();
        let abs_str = abs.to_str().unwrap();

        // Jailed (default): an absolute path outside the root is rejected.
        let (jailed, _d1) = ws();
        assert!(jailed.is_jailed());
        assert!(jailed.read(abs_str, None, None).is_err());

        // Unjailed: the same absolute path reads fine (full filesystem).
        let unjailed = jailed.clone().with_jail(false);
        assert!(!unjailed.is_jailed());
        let got = unjailed.read(abs_str, None, None).unwrap();
        assert!(got.contains("top secret"));
        // for_chat propagates the unjailed flag.
        assert!(!unjailed.for_chat("chatZ").unwrap().is_jailed());
    }

    #[test]
    fn coding_direct_then_worktree_and_home_bound() {
        let dir = tempfile::tempdir().unwrap();
        let home = dunce::canonicalize(dir.path()).unwrap();
        let default = Workspace::new(home.join("projects").to_str().unwrap()).unwrap();
        let cw = CodingWorkspaces::new(default, Some(home.clone()));
        std::fs::create_dir_all(home.join("myproj")).unwrap();
        let folder = home.join("myproj");
        let fstr = folder.to_str().unwrap();

        // first chat → DIRECT (root == the folder itself)
        let a = cw.resolve(Some(fstr), "chatA").unwrap();
        assert_eq!(
            a.root_display(),
            dunce::canonicalize(&folder).unwrap().display().to_string()
        );
        // same chat again → still direct (claim held)
        assert_eq!(
            cw.resolve(Some(fstr), "chatA").unwrap().root_display(),
            a.root_display()
        );
        // concurrent OTHER chat → isolated worktree UNDER the folder
        let b = cw.resolve(Some(fstr), "chatB").unwrap();
        assert_ne!(b.root_display(), a.root_display());
        assert!(b.root_display().starts_with(&a.root_display()));
        // a folder OUTSIDE home is rejected
        assert!(cw.resolve(Some("/etc"), "chatC").is_err());
        // None → the enrolled default workspace (base, empty chat id)
        let d = cw.resolve(None, "").unwrap();
        assert_eq!(
            d.root_display(),
            dunce::canonicalize(home.join("projects"))
                .unwrap()
                .display()
                .to_string()
        );
    }

    #[tokio::test]
    async fn mkproject_creates_jailed_dir_and_rejects_outside() {
        let dir = tempfile::tempdir().unwrap();
        let home = dunce::canonicalize(dir.path()).unwrap();
        let default = Workspace::new(home.join("projects").to_str().unwrap()).unwrap();
        let cw = CodingWorkspaces::new(default, Some(home.clone()));

        // creates an empty dir under home; reports its canonical path
        let out = cw
            .mkproject(home.join("fresh").to_str().unwrap())
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(home.join("fresh").is_dir());
        assert!(v["path"].as_str().unwrap().ends_with("fresh"));
        // it becomes a resolvable coding workspace (bound check passes)
        assert!(cw
            .resolve(Some(home.join("fresh").to_str().unwrap()), "chatX")
            .is_ok());
        // a non-empty existing dir is refused
        std::fs::write(home.join("fresh").join("x"), "y").unwrap();
        assert!(cw
            .mkproject(home.join("fresh").to_str().unwrap())
            .await
            .is_err());
        // outside every allowed root → rejected; parent that doesn't exist → rejected
        assert!(cw.mkproject("/etc/should_not").await.is_err());
        assert!(cw
            .mkproject(home.join("missing/deep").to_str().unwrap())
            .await
            .is_err());
        // option-injection guarded
        assert!(cw.mkproject("--help").await.is_err());
    }

    #[tokio::test]
    async fn git_clone_into_jailed_dir_offline() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // git not available
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dunce::canonicalize(dir.path()).unwrap();
        let default = Workspace::new(home.join("projects").to_str().unwrap()).unwrap();
        let cw = CodingWorkspaces::new(default, Some(home.clone()));

        // a local source repo to clone (its location is UNbounded; only dest is jailed)
        let src = dir.path().join("source-repo");
        std::fs::create_dir_all(&src).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&src)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(&src)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&src)
            .output()
            .unwrap();
        std::fs::write(src.join("readme.md"), "hi").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&src)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "seed"])
            .current_dir(&src)
            .output()
            .unwrap();

        let dest = home.join("cloned");
        let out = cw
            .git_clone(src.to_str().unwrap(), dest.to_str().unwrap(), None)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(dest.join("readme.md").is_file(), "clone did not land files");
        assert!(v["path"].as_str().unwrap().ends_with("cloned"));
        // dest outside the allowed roots → rejected
        assert!(cw
            .git_clone(src.to_str().unwrap(), "/etc/x", None)
            .await
            .is_err());
        // url option-injection guarded (checked before the dest)
        assert!(cw
            .git_clone("--upload-pack=evil", home.join("z").to_str().unwrap(), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn home_list_browses_and_bounds_parent() {
        let (h, d) = home();
        std::fs::create_dir_all(d.path().join("projects")).unwrap();
        std::fs::create_dir_all(d.path().join(".hidden")).unwrap();
        let out = h.execute("home_list", &json!({})).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["parent"].is_null(), "home root has no parent");
        let names: Vec<&str> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"projects"));
        assert!(!names.iter().any(|n| n.starts_with('.')), "dotdirs hidden");
        // a subdir lists with a non-null parent that stays inside home
        let sub = h
            .execute(
                "home_list",
                &json!({"dir": d.path().join("projects").to_str().unwrap()}),
            )
            .await
            .unwrap();
        let sv: serde_json::Value = serde_json::from_str(&sub).unwrap();
        assert!(sv["parent"].is_string());
        // a dir outside home is rejected
        assert!(h
            .execute("home_list", &json!({"dir": "/etc"}))
            .await
            .is_err());
    }

    #[test]
    fn home_read_blocks_relative_escape() {
        let (h, _d) = home();
        for bad in ["../escape", "../../etc/passwd", ".."] {
            assert!(h.read(bad).is_err(), "{bad} should be blocked");
        }
    }

    #[test]
    fn home_contains_rejects_outside_and_symlink_escape() {
        let (h, d) = home();
        std::fs::write(d.path().join("inside.txt"), "x").unwrap();
        assert!(h.contains(&d.path().join("inside.txt")));
        assert!(!h.contains(Path::new("/etc/passwd")));
        #[cfg(unix)]
        {
            // a symlink inside home pointing OUT must be rejected (canonicalize resolves it out).
            let link = d.path().join("escape");
            std::os::unix::fs::symlink("/etc", &link).unwrap();
            assert!(!h.contains(&link.join("passwd")));
        }
    }

    #[tokio::test]
    async fn home_search_walk_finds_and_filters_dotdirs() {
        let (h, d) = home();
        std::fs::write(d.path().join("report.txt"), "x").unwrap();
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/report.log"), "x").unwrap();
        std::fs::create_dir_all(d.path().join(".cache")).unwrap();
        std::fs::write(d.path().join(".cache/report_secret.txt"), "x").unwrap();
        let out = h
            .execute("home_search", &json!({"pattern": "report"}))
            .await
            .unwrap();
        let items: Vec<Value> = serde_json::from_str(&out).unwrap();
        let paths: Vec<&str> = items.iter().map(|i| i["path"].as_str().unwrap()).collect();
        assert!(paths.iter().any(|p| p.ends_with("report.txt")));
        assert!(paths.iter().any(|p| p.ends_with("sub/report.log")));
        // dotdir contents are skipped by the walk
        assert!(
            !paths.iter().any(|p| p.contains(".cache")),
            "got: {paths:?}"
        );
        // every result is a real path inside home + has the expected shape
        for i in &items {
            assert!(i["size"].is_u64() && i["is_dir"].is_boolean());
            assert!(h.contains(Path::new(i["path"].as_str().unwrap())));
        }
    }

    #[tokio::test]
    async fn home_search_respects_limit() {
        let (h, d) = home();
        for n in 0..6 {
            std::fs::write(d.path().join(format!("file{n}.dat")), "x").unwrap();
        }
        let out = h
            .execute("home_search", &json!({"pattern": "file", "limit": 2}))
            .await
            .unwrap();
        let items: Vec<Value> = serde_json::from_str(&out).unwrap();
        assert!(items.len() <= 2, "limit not respected: {}", items.len());
    }

    #[tokio::test]
    async fn home_read_roundtrips_absolute_and_relative() {
        let (h, d) = home();
        std::fs::write(d.path().join("note.md"), "hello home").unwrap();
        // relative
        let rel = h
            .execute("home_read", &json!({"path": "note.md"}))
            .await
            .unwrap();
        assert!(rel.contains("hello home"));
        // absolute (as returned by home_search)
        let abs = d.path().join("note.md");
        let got = h
            .execute("home_read", &json!({"path": abs.to_str().unwrap()}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["content"].as_str().unwrap(), "hello home");
        assert_eq!(v["truncated"], json!(false));
        // an absolute path outside home is rejected
        assert!(h
            .execute("home_read", &json!({"path": "/etc/hostname"}))
            .await
            .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_command_returns_on_shell_exit_despite_background_child() {
        let (w, _d) = ws();
        // An unredirected background child inherits the output pipes — the call must
        // complete when the SHELL exits, not when the pipe hits EOF (old behavior:
        // hangs for the full timeout, leaving an orphan daemon behind).
        let started = std::time::Instant::now();
        let out = w
            .execute(
                "run_command",
                &json!({"command": "sleep 30 & echo started"}),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "call held by background child"
        );
        assert!(
            out.contains("(exit 0)") && out.contains("started"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn write_read_edit_list_within_workspace() {
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "a/b.txt", "content": "hello\nworld"}),
        )
        .await
        .unwrap();
        let read = w
            .execute("read_file", &json!({"path": "a/b.txt"}))
            .await
            .unwrap();
        assert!(read.contains("1\thello") && read.contains("2\tworld"));
        w.execute(
            "edit_file",
            &json!({"path": "a/b.txt", "old": "hello", "new": "bye"}),
        )
        .await
        .unwrap();
        assert!(w
            .execute("read_file", &json!({"path": "a/b.txt"}))
            .await
            .unwrap()
            .contains("bye"));
        assert!(w
            .execute("list_dir", &json!({"path": "."}))
            .await
            .unwrap()
            .contains("a/"));
    }

    #[tokio::test]
    async fn fuzzy_edit_applies_despite_whitespace_drift() {
        let (w, _d) = ws();
        // File is indented with 4 spaces; the model's `old` uses 2 + trailing space.
        w.execute(
            "write_file",
            &json!({"path": "m.py", "content": "def f():\n    return  1\n"}),
        )
        .await
        .unwrap();
        let out = w
            .execute(
                "edit_file",
                &json!({"path": "m.py", "old": "  return 1", "new": "    return 2"}),
            )
            .await
            .unwrap();
        assert!(out.contains("edited"), "got: {out}");
        let read = w
            .execute("read_file", &json!({"path": "m.py"}))
            .await
            .unwrap();
        assert!(read.contains("return 2"), "file: {read}");
    }

    #[tokio::test]
    async fn fuzzy_anchor_edit_replaces_drifted_block() {
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "b.txt", "content": "start\n  middle here\nfinish\n"}),
        )
        .await
        .unwrap();
        // old's middle line differs, but first+last anchor the 3-line block.
        let out = w
            .execute(
                "edit_file",
                &json!({"path": "b.txt",
                "old": "start\nMIDDLE\nfinish", "new": "start\nnew middle\nfinish"}),
            )
            .await
            .unwrap();
        assert!(
            out.contains("anchor") || out.contains("edited"),
            "got: {out}"
        );
        assert!(w
            .execute("read_file", &json!({"path": "b.txt"}))
            .await
            .unwrap()
            .contains("new middle"));
    }

    #[tokio::test]
    async fn lsp_diagnostics_via_pyright_when_available() {
        // Real LSP round-trip: only asserts when pyright-langserver is installed.
        if std::process::Command::new("pyright-langserver")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // server not installed in this environment → skip
        }
        let (w, _d) = ws();
        // `undefined_name` is an undefined symbol → pyright reports an error.
        w.execute(
            "write_file",
            &json!({"path": "bad.py", "content": "x = undefined_name + 1\n"}),
        )
        .await
        .unwrap();
        let diag = w
            .execute("diagnostics", &json!({"path": "bad.py"}))
            .await
            .unwrap();
        assert!(
            diag.to_lowercase().contains("undefined") || diag.contains("error"),
            "diag: {diag}"
        );
    }

    #[tokio::test]
    async fn apply_patch_is_atomic_multifile() {
        let (w, _d) = ws();
        w.execute("write_file", &json!({"path": "a.txt", "content": "one\n"}))
            .await
            .unwrap();
        // add b.txt, update a.txt — together.
        w.execute(
            "apply_patch",
            &json!({"changes": [
                {"path": "a.txt", "action": "update", "edits": [{"old": "one", "new": "ONE"}]},
                {"path": "b.txt", "action": "add", "content": "bee\n"}
            ]}),
        )
        .await
        .unwrap();
        assert!(w
            .execute("read_file", &json!({"path": "a.txt"}))
            .await
            .unwrap()
            .contains("ONE"));
        assert!(w
            .execute("read_file", &json!({"path": "b.txt"}))
            .await
            .unwrap()
            .contains("bee"));
        // A failing change aborts the whole patch (c.txt must NOT appear).
        let err = w
            .execute(
                "apply_patch",
                &json!({"changes": [
                    {"path": "c.txt", "action": "add", "content": "see\n"},
                    {"path": "a.txt", "action": "update", "edits": [{"old": "NOPE", "new": "x"}]}
                ]}),
            )
            .await;
        assert!(err.is_err(), "patch should have failed");
        assert!(
            w.execute("list_dir", &json!({"path": "."}))
                .await
                .unwrap()
                .matches("c.txt")
                .count()
                == 0
        );
    }

    #[tokio::test]
    async fn snapshot_reverts_edits_adds_and_deletes() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // git not available
        }
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "keep.txt", "content": "v1\n"}),
        )
        .await
        .unwrap();
        let tree = w.execute("snap_create", &json!({})).await.unwrap();
        assert!(!tree.trim().is_empty(), "got tree: {tree}");
        // Mutate: edit keep.txt + add new.txt.
        w.execute(
            "write_file",
            &json!({"path": "keep.txt", "content": "v2\n"}),
        )
        .await
        .unwrap();
        w.execute(
            "write_file",
            &json!({"path": "new.txt", "content": "added\n"}),
        )
        .await
        .unwrap();
        // Revert.
        w.execute("snap_restore", &json!({"tree": tree.trim()}))
            .await
            .unwrap();
        assert!(w
            .execute("read_file", &json!({"path": "keep.txt"}))
            .await
            .unwrap()
            .contains("v1"));
        let listing = w.execute("list_dir", &json!({"path": "."})).await.unwrap();
        assert!(
            !listing.contains("new.txt"),
            "new.txt should have been reverted: {listing}"
        );
    }

    #[tokio::test]
    async fn for_chat_isolates_concurrent_chats() {
        let (w, _d) = ws();
        // Two chats edit the SAME relative path → must NOT clobber each other.
        let a = w.for_chat("chatAAAA").unwrap();
        let b = w.for_chat("chatBBBB").unwrap();
        a.execute("write_file", &json!({"path": "f.txt", "content": "A"}))
            .await
            .unwrap();
        b.execute("write_file", &json!({"path": "f.txt", "content": "B"}))
            .await
            .unwrap();
        let ra = a
            .execute("read_file", &json!({"path": "f.txt"}))
            .await
            .unwrap();
        let rb = b
            .execute("read_file", &json!({"path": "f.txt"}))
            .await
            .unwrap();
        assert!(ra.contains("A") && !ra.contains("B"), "chat A leaked: {ra}");
        assert!(rb.contains("B") && !rb.contains("A"), "chat B leaked: {rb}");
        // Idempotent + empty id falls back to the base root.
        assert_eq!(
            w.for_chat("chatAAAA").unwrap().root_display(),
            a.root_display()
        );
        assert_eq!(w.for_chat("").unwrap().root_display(), w.root_display());
        // The base root does not recurse into the per-chat worktrees (.personal-agent ignored).
        let g = w.execute("grep", &json!({"pattern": "A|B"})).await.unwrap();
        assert!(
            !g.contains(".personal-agent"),
            "base grep leaked into worktrees: {g}"
        );
    }

    #[tokio::test]
    async fn for_chat_worktree_isolates_shadow_git() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let (w, _d) = ws();
        // A git repo with one commit → per-chat worktrees are real git worktrees.
        for a in [["init", "-q"].as_slice()] {
            std::process::Command::new("git")
                .args(a)
                .current_dir(&w.root)
                .output()
                .unwrap();
        }
        std::process::Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(&w.root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&w.root)
            .output()
            .unwrap();
        std::fs::write(w.root.join("base.txt"), "seed").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&w.root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "seed"])
            .current_dir(&w.root)
            .output()
            .unwrap();
        let a = w.for_chat("chatAAAA").unwrap();
        // The worktree starts from HEAD (sees the committed file) but is a separate dir.
        assert!(a
            .execute("read_file", &json!({"path": "base.txt"}))
            .await
            .unwrap()
            .contains("seed"));
        assert_ne!(a.root_display(), w.root_display());
        // snapshot + mutate + restore stays within the chat's worktree.
        let tree = a.execute("snap_create", &json!({})).await.unwrap();
        a.execute(
            "write_file",
            &json!({"path": "base.txt", "content": "mutated"}),
        )
        .await
        .unwrap();
        a.execute("snap_restore", &json!({"tree": tree.trim()}))
            .await
            .unwrap();
        assert!(a
            .execute("read_file", &json!({"path": "base.txt"}))
            .await
            .unwrap()
            .contains("seed"));
        // The base working tree was untouched by the chat's mutation+restore.
        assert_eq!(
            std::fs::read_to_string(w.root.join("base.txt")).unwrap(),
            "seed"
        );
    }

    #[tokio::test]
    async fn read_offset_and_limit() {
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "f.txt", "content": "l1\nl2\nl3\nl4"}),
        )
        .await
        .unwrap();
        let out = w
            .execute(
                "read_file",
                &json!({"path": "f.txt", "offset": 2, "limit": 2}),
            )
            .await
            .unwrap();
        assert!(out.contains("2\tl2") && out.contains("3\tl3"));
        assert!(!out.contains("1\tl1") && !out.contains("4\tl4"));
    }

    #[tokio::test]
    async fn edit_requires_unique_unless_replace_all() {
        let (w, _d) = ws();
        w.execute("write_file", &json!({"path": "f.txt", "content": "x x x"}))
            .await
            .unwrap();
        assert!(w
            .execute(
                "edit_file",
                &json!({"path": "f.txt", "old": "x", "new": "y"})
            )
            .await
            .is_err());
        let out = w
            .execute(
                "edit_file",
                &json!({"path": "f.txt", "old": "x", "new": "y", "replace_all": true}),
            )
            .await
            .unwrap();
        assert!(out.contains("edited"));
        assert_eq!(
            w.execute("read_file", &json!({"path": "f.txt"}))
                .await
                .unwrap(),
            "1\ty y y\n"
        );
    }

    #[tokio::test]
    async fn multi_edit_is_atomic_sequence() {
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "f.txt", "content": "alpha beta"}),
        )
        .await
        .unwrap();
        w.execute(
            "multi_edit",
            &json!({"path": "f.txt", "edits": [
                {"old": "alpha", "new": "A"}, {"old": "beta", "new": "B"}
            ]}),
        )
        .await
        .unwrap();
        assert_eq!(
            w.execute("read_file", &json!({"path": "f.txt"}))
                .await
                .unwrap(),
            "1\tA B\n"
        );
    }

    #[tokio::test]
    async fn grep_finds_matches_and_skips_ignored() {
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "src/a.rs", "content": "fn foo() {}\nfn bar() {}"}),
        )
        .await
        .unwrap();
        w.execute(
            "write_file",
            &json!({"path": "target/junk.rs", "content": "fn foo() {}"}),
        )
        .await
        .unwrap();
        let out = w
            .execute("grep", &json!({"pattern": "fn foo"}))
            .await
            .unwrap();
        assert!(out.contains("src/a.rs:1: fn foo"));
        assert!(!out.contains("target/"), "ignored dir must be skipped");
    }

    #[tokio::test]
    async fn read_paginates_with_marker_and_offset() {
        let (w, _d) = ws();
        let body = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        w.execute("write_file", &json!({"path": "f.txt", "content": body}))
            .await
            .unwrap();
        let p1 = w
            .execute("read_file", &json!({"path": "f.txt", "limit": 4}))
            .await
            .unwrap();
        assert!(p1.contains("1\tline 1") && p1.contains("4\tline 4"));
        assert!(!p1.contains("5\tline 5"));
        assert!(
            p1.contains("offset=5"),
            "marker must point at the next line: {p1}"
        );
        // Following the marker reads the next page.
        let p2 = w
            .execute(
                "read_file",
                &json!({"path": "f.txt", "offset": 5, "limit": 4}),
            )
            .await
            .unwrap();
        assert!(p2.contains("5\tline 5") && p2.contains("8\tline 8"));
    }

    #[tokio::test]
    async fn read_no_marker_when_whole_file_fits() {
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "f.txt", "content": "a\nb\nc"}),
        )
        .await
        .unwrap();
        let out = w
            .execute("read_file", &json!({"path": "f.txt"}))
            .await
            .unwrap();
        assert!(
            !out.contains("pagination"),
            "no marker for a fully-shown file"
        );
    }

    #[tokio::test]
    async fn grep_paginates_with_offset() {
        let (w, _d) = ws();
        let body = (0..5).map(|_| "needle").collect::<Vec<_>>().join("\n");
        w.execute("write_file", &json!({"path": "f.txt", "content": body}))
            .await
            .unwrap();
        // GREP_MAX_MATCHES is large, so force paging by reading from an offset.
        let out = w
            .execute("grep", &json!({"pattern": "needle", "offset": 2}))
            .await
            .unwrap();
        // Offset skips the first two matches → lines 3,4,5 shown.
        assert!(out.contains("f.txt:3:") && out.contains("f.txt:5:"));
        assert!(!out.contains("f.txt:1:") && !out.contains("f.txt:2:"));
    }

    #[tokio::test]
    async fn grep_context_includes_surrounding_lines() {
        let (w, _d) = ws();
        w.execute(
            "write_file",
            &json!({"path": "f.txt", "content": "a\nb\nMATCH\nd\ne"}),
        )
        .await
        .unwrap();
        let out = w
            .execute("grep", &json!({"pattern": "MATCH", "context": 1}))
            .await
            .unwrap();
        assert!(out.contains("f.txt:2- b"), "context line before: {out}");
        assert!(
            out.contains("f.txt:3: MATCH"),
            "the match keeps the ':' separator"
        );
        assert!(out.contains("f.txt:4- d"), "context line after");
    }

    #[tokio::test]
    async fn grep_glob_filters_files() {
        let (w, _d) = ws();
        w.execute("write_file", &json!({"path": "a.rs", "content": "needle"}))
            .await
            .unwrap();
        w.execute("write_file", &json!({"path": "b.txt", "content": "needle"}))
            .await
            .unwrap();
        let out = w
            .execute("grep", &json!({"pattern": "needle", "glob": "**/*.rs"}))
            .await
            .unwrap();
        assert!(out.contains("a.rs:1:"));
        assert!(
            !out.contains("b.txt"),
            "glob must exclude non-matching files"
        );
    }

    #[tokio::test]
    async fn glob_matches_by_pattern() {
        let (w, _d) = ws();
        w.execute("write_file", &json!({"path": "src/x.rs", "content": "1"}))
            .await
            .unwrap();
        w.execute("write_file", &json!({"path": "src/y.txt", "content": "1"}))
            .await
            .unwrap();
        let out = w
            .execute("glob", &json!({"pattern": "**/*.rs"}))
            .await
            .unwrap();
        assert!(out.contains("src/x.rs") && !out.contains("y.txt"));
    }

    #[tokio::test]
    async fn path_escape_is_blocked() {
        let (w, _d) = ws();
        for (tool, args) in [
            ("read_file", json!({"path": "../../etc/passwd"})),
            (
                "write_file",
                json!({"path": "../escape.txt", "content": "x"}),
            ),
            ("list_dir", json!({"path": "../.."})),
        ] {
            assert!(
                w.execute(tool, &args).await.is_err(),
                "{tool} should be blocked"
            );
        }
    }

    #[tokio::test]
    async fn fs_methods_roundtrip_raw_and_are_jailed() {
        let (w, _d) = ws();
        // fs_write saves raw, fs_read returns it verbatim (no line numbers).
        w.execute(
            "fs_write",
            &json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
        )
        .await
        .unwrap();
        let read = w
            .execute("fs_read", &json!({"path": "src/main.rs"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&read).unwrap();
        assert_eq!(v["content"], "fn main() {}\n");
        assert_eq!(v["truncated"], false);
        // fs_list returns structured entries, dirs first.
        let list = w.execute("fs_list", &json!({"path": "."})).await.unwrap();
        let items: Value = serde_json::from_str(&list).unwrap();
        assert_eq!(items[0]["name"], "src");
        assert_eq!(items[0]["is_dir"], true);
        // Still jailed.
        assert!(w
            .execute("fs_read", &json!({"path": "../../etc/passwd"}))
            .await
            .is_err());
        // fs_* are UI-only: never announced as LLM tools.
        let specs = tool_specs();
        let names: Vec<&str> = specs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with("fs_")));
    }

    #[test]
    fn tool_specs_announce_all_tools() {
        let specs = tool_specs();
        let names: Vec<&str> = specs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 15);
        for t in [
            "run_command",
            "read_file",
            "edit_file",
            "multi_edit",
            "grep",
            "glob",
            "run_background",
            "list_processes",
            "read_process",
            "stop_process",
            "diagnostics",
            "lsp",
            "apply_patch",
        ] {
            assert!(names.contains(&t), "missing {t}");
        }
    }
}
