//! A real Language Server Protocol client for the device agent.
//!
//! Spawns a language server per (workspace-root, language), speaks JSON-RPC over the
//! server's stdio (Content-Length framing), and exposes diagnostics + navigation
//! (definition/references/hover/document-symbols). Servers are cached and kept warm
//! across tool calls so they retain project context. A server that isn't installed
//! on the device is simply skipped (cached as unavailable) — never an error.
//!
//! This is what makes edits self-correcting: after the agent writes a file we open it
//! and feed the server's diagnostics back into the tool result so the model fixes its
//! own type/syntax errors in the same turn (mirrors OpenCode's LSP integration).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::time::{timeout, Duration, Instant};

struct ServerSpec {
    id: &'static str,
    cmd: &'static str,
    args: &'static [&'static str],
    lang: &'static str,
}

/// The language server for a file extension, or None if we don't know one.
fn server_for(ext: &str) -> Option<ServerSpec> {
    let (id, cmd, args, lang): (_, _, &[&str], _) = match ext {
        "py" | "pyi" => ("pyright", "pyright-langserver", &["--stdio"], "python"),
        "rs" => ("rust-analyzer", "rust-analyzer", &[], "rust"),
        "go" => ("gopls", "gopls", &[], "go"),
        "ts" => (
            "tsserver",
            "typescript-language-server",
            &["--stdio"],
            "typescript",
        ),
        "tsx" => (
            "tsserver",
            "typescript-language-server",
            &["--stdio"],
            "typescriptreact",
        ),
        "js" | "mjs" | "cjs" => (
            "tsserver",
            "typescript-language-server",
            &["--stdio"],
            "javascript",
        ),
        "jsx" => (
            "tsserver",
            "typescript-language-server",
            &["--stdio"],
            "javascriptreact",
        ),
        "c" | "h" => ("clangd", "clangd", &[], "c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => ("clangd", "clangd", &[], "cpp"),
        // More common stdio servers — used only when the binary is installed on the device
        // (server_for is followed by a start() that caches None on failure, so an absent
        // server just means that language has no LSP, exactly like before).
        "rb" => ("solargraph", "solargraph", &["stdio"], "ruby"),
        "php" => ("intelephense", "intelephense", &["--stdio"], "php"),
        "lua" => ("lua-ls", "lua-language-server", &[], "lua"),
        "sh" | "bash" => ("bashls", "bash-language-server", &["start"], "shellscript"),
        "yaml" | "yml" => ("yamlls", "yaml-language-server", &["--stdio"], "yaml"),
        "json" | "jsonc" => (
            "jsonls",
            "vscode-json-language-server",
            &["--stdio"],
            "json",
        ),
        "toml" => ("taplo", "taplo", &["lsp", "stdio"], "toml"),
        _ => return None,
    };
    Some(ServerSpec {
        id,
        cmd,
        args,
        lang,
    })
}

fn path_to_uri(p: &str) -> String {
    // file:///abs/path with spaces percent-encoded (enough for typical repo paths).
    // Normalise Windows backslashes to '/' and ensure a leading '/' before a drive letter
    // (file:///C:/...), so the URI is RFC 8089-valid on every platform (no-op on Unix).
    let p = p.replace('\\', "/");
    let p = if p.starts_with('/') {
        p
    } else {
        format!("/{p}")
    };
    format!("file://{}", p.replace(' ', "%20"))
}

fn uri_to_path(uri: &str) -> String {
    let p = uri
        .strip_prefix("file://")
        .unwrap_or(uri)
        .replace("%20", " ");
    // Windows: file:///C:/... → strip the leading slash before the drive + restore '\'.
    #[cfg(windows)]
    let p = p.strip_prefix('/').unwrap_or(p.as_str()).replace('/', "\\");
    p
}

struct LspClient {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    diagnostics: Mutex<HashMap<String, Vec<Value>>>, // keyed by decoded path
    diag_notify: Notify,
    opened: Mutex<HashSet<String>>,
    version: AtomicI64,
    _child: Mutex<Child>,
}

impl LspClient {
    async fn send_raw(&self, msg: Value) -> Result<()> {
        let body = serde_json::to_vec(&msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(&body).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_raw(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send_raw(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(v)) => Ok(v),
            _ => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("lsp request timed out: {method}"))
            }
        }
    }

    async fn handle_incoming(&self, msg: Value) {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();
        match (method, &id) {
            (Some("textDocument/publishDiagnostics"), _) => {
                if let Some(p) = msg.get("params") {
                    let path = uri_to_path(p.get("uri").and_then(Value::as_str).unwrap_or(""));
                    let diags = p
                        .get("diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    self.diagnostics.lock().await.insert(path, diags);
                    self.diag_notify.notify_waiters();
                }
            }
            // Server → client REQUEST (has both method and id): reply so it doesn't block.
            (Some(_), Some(reqid)) if !reqid.is_null() => {
                let _ = self
                    .send_raw(json!({"jsonrpc": "2.0", "id": reqid.clone(), "result": null}))
                    .await;
            }
            // A RESPONSE to one of our requests (id, no method).
            (None, Some(reqid)) => {
                if let Some(idn) = reqid.as_i64() {
                    if let Some(tx) = self.pending.lock().await.remove(&idn) {
                        let _ = tx.send(msg.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
            }
            _ => {} // server notification we ignore
        }
    }

    /// Open the file (or re-sync if already open) and clear its diagnostics so the
    /// next `await_diagnostics` reflects this version.
    async fn sync_file(&self, abs_path: &str, lang: &str, text: &str) -> Result<()> {
        let uri = path_to_uri(abs_path);
        self.diagnostics.lock().await.remove(abs_path);
        let mut opened = self.opened.lock().await;
        if opened.contains(abs_path) {
            let v = self.version.fetch_add(1, Ordering::SeqCst) + 1;
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri, "version": v},
                    "contentChanges": [{"text": text}],
                }),
            )
            .await
        } else {
            opened.insert(abs_path.to_string());
            self.notify(
                "textDocument/didOpen",
                json!({"textDocument": {"uri": uri, "languageId": lang, "version": 1, "text": text}}),
            )
            .await
        }
    }

    /// Wait until diagnostics for `abs_path` are published (or the timeout elapses,
    /// treated as "clean"). Returns error+warning-severity items.
    async fn await_diagnostics(&self, abs_path: &str, dur: Duration) -> Vec<Value> {
        let deadline = Instant::now() + dur;
        loop {
            if let Some(v) = self.diagnostics.lock().await.get(abs_path) {
                return v.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero()
                || timeout(remaining, self.diag_notify.notified())
                    .await
                    .is_err()
            {
                return Vec::new();
            }
        }
    }
}

async fn reader_loop(stdout: ChildStdout, client: Arc<LspClient>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut content_len = 0usize;
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => return, // EOF / broken pipe → server gone
                Ok(_) => {}
            }
            let s = String::from_utf8_lossy(&line);
            let t = s.trim_end();
            if t.is_empty() {
                break; // end of headers
            }
            if let Some(v) = t.strip_prefix("Content-Length:") {
                content_len = v.trim().parse().unwrap_or(0);
            }
        }
        if content_len == 0 || content_len > 16_000_000 {
            continue;
        }
        let mut buf = vec![0u8; content_len];
        if reader.read_exact(&mut buf).await.is_err() {
            return;
        }
        if let Ok(msg) = serde_json::from_slice::<Value>(&buf) {
            client.handle_incoming(msg).await;
        }
    }
}

async fn start(spec: &ServerSpec, root: &str) -> Result<Arc<LspClient>> {
    let mut child = tokio::process::Command::new(spec.cmd)
        .args(spec.args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?; // Err (NotFound) if the server isn't installed
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let client = Arc::new(LspClient {
        stdin: Mutex::new(stdin),
        next_id: AtomicI64::new(1),
        pending: Mutex::new(HashMap::new()),
        diagnostics: Mutex::new(HashMap::new()),
        diag_notify: Notify::new(),
        opened: Mutex::new(HashSet::new()),
        version: AtomicI64::new(1),
        _child: Mutex::new(child),
    });
    tokio::spawn(reader_loop(stdout, client.clone()));
    let root_uri = path_to_uri(root);
    client
        .request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
                "capabilities": {
                    "textDocument": {
                        "synchronization": {"didSave": false, "dynamicRegistration": false},
                        "publishDiagnostics": {"relatedInformation": false},
                        "hover": {"contentFormat": ["plaintext", "markdown"]},
                        "definition": {}, "references": {}, "documentSymbol": {},
                        "callHierarchy": {"dynamicRegistration": false}
                    },
                    "workspace": {
                        "workspaceFolders": true,
                        "symbol": {"dynamicRegistration": false}
                    }
                }
            }),
        )
        .await?;
    client.notify("initialized", json!({})).await?;
    Ok(client)
}

struct Manager {
    // key "serverid\0root" → Some(client) warm, or None = tried & unavailable
    clients: Mutex<HashMap<String, Option<Arc<LspClient>>>>,
}

static MANAGER: OnceLock<Manager> = OnceLock::new();

fn manager() -> &'static Manager {
    MANAGER.get_or_init(|| Manager {
        clients: Mutex::new(HashMap::new()),
    })
}

async fn client_for(ext: &str, root: &str) -> Option<Arc<LspClient>> {
    let spec = server_for(ext)?;
    let key = format!("{}\0{}", spec.id, root);
    let mut map = manager().clients.lock().await;
    if let Some(slot) = map.get(&key) {
        return slot.clone();
    }
    let started = start(&spec, root).await.ok();
    if started.is_none() {
        eprintln!(
            "personal-agent-device-agent: LSP server '{}' unavailable",
            spec.cmd
        );
    }
    map.insert(key, started.clone());
    started
}

fn lang_for(ext: &str) -> &'static str {
    server_for(ext).map(|s| s.lang).unwrap_or("plaintext")
}

fn format_diagnostics(diags: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for d in diags {
        let sev = d.get("severity").and_then(Value::as_i64).unwrap_or(1);
        if sev > 2 {
            continue; // keep only Error(1) + Warning(2)
        }
        let label = if sev == 1 { "error" } else { "warning" };
        let line = d
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        let col = d
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("character"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        let msg = d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");
        lines.push(format!("  {line}:{col} {label}: {msg}"));
        if lines.len() >= 30 {
            lines.push("  … (weitere Diagnosen abgeschnitten)".into());
            break;
        }
    }
    lines.join("\n")
}

/// Diagnostics for a freshly written file, formatted for the model. None when no
/// server is available for the language; Some("") when the file is clean.
pub async fn diagnostics(root: &str, abs_path: &str) -> Option<String> {
    let ext = Path::new(abs_path).extension()?.to_str()?;
    let client = client_for(ext, root).await?;
    let text = tokio::fs::read_to_string(abs_path).await.ok()?;
    client
        .sync_file(abs_path, lang_for(ext), &text)
        .await
        .ok()?;
    let diags = client
        .await_diagnostics(abs_path, Duration::from_secs(6))
        .await;
    Some(format_diagnostics(&diags))
}

/// Append a "fix your errors" block after an LLM edit, or "" if clean / no server.
pub async fn diagnostics_feedback(root: &str, abs_path: &str, rel: &str) -> String {
    match diagnostics(root, abs_path).await {
        Some(d) if !d.trim().is_empty() => {
            format!("\n⚠ Diagnose-Fehler in {rel} (bitte beheben):\n{d}")
        }
        _ => String::new(),
    }
}

fn fmt_locations(res: &Value) -> String {
    let arr = match res {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        other => vec![other.clone()],
    };
    let mut out = Vec::new();
    for loc in arr.iter().take(50) {
        // Location or LocationLink
        let uri = loc
            .get("uri")
            .or_else(|| loc.get("targetUri"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let range = loc.get("range").or_else(|| loc.get("targetSelectionRange"));
        let line = range
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        out.push(format!("{}:{}", uri_to_path(uri), line));
    }
    if out.is_empty() {
        "(keine Treffer)".into()
    } else {
        out.join("\n")
    }
}

fn fmt_symbols(res: &Value) -> String {
    let arr = res.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    fn walk(items: &[Value], depth: usize, out: &mut Vec<String>) {
        for s in items {
            let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
            let line = s
                .get("range")
                .or_else(|| s.get("location").and_then(|l| l.get("range")))
                .and_then(|r| r.get("start"))
                .and_then(|p| p.get("line"))
                .and_then(Value::as_i64)
                .unwrap_or(0)
                + 1;
            out.push(format!("{}{} (L{})", "  ".repeat(depth), name, line));
            if out.len() >= 200 {
                return;
            }
            if let Some(children) = s.get("children").and_then(Value::as_array) {
                walk(children, depth + 1, out);
            }
        }
    }
    walk(&arr, 0, &mut out);
    if out.is_empty() {
        "(keine Symbole)".into()
    } else {
        out.join("\n")
    }
}

/// LSP SymbolKind (1-26) → a short label for the workspace-symbol listing.
fn symbol_kind_name(k: i64) -> &'static str {
    match k {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        22 => "enum-member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-param",
        _ => "symbol",
    }
}

/// Format a `workspace/symbol` response. SymbolInformation[] items carry `name`, `kind`,
/// optional `containerName` + `location`; LSP 3.17 WorkspaceSymbol[] items carry `name`,
/// `kind` + `location` (no `containerName`), and the `location.range` may be absent (then
/// only the file is shown). Capped at 100 results with a truncation notice.
fn fmt_workspace_symbols(res: &Value) -> String {
    let arr = res.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for s in arr.iter().take(100) {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = s
            .get("kind")
            .and_then(Value::as_i64)
            .map(symbol_kind_name)
            .unwrap_or("symbol");
        let loc = s.get("location");
        let uri = loc
            .and_then(|l| l.get("uri"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let line = loc
            .and_then(|l| l.get("range"))
            .and_then(|r| r.get("start"))
            .and_then(|p| p.get("line"))
            .and_then(Value::as_i64)
            .map(|l| l + 1);
        let loc_str = match line {
            Some(l) => format!("{}:{}", uri_to_path(uri), l),
            None => uri_to_path(uri),
        };
        let container = s
            .get("containerName")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(|c| format!("  [{c}]"))
            .unwrap_or_default();
        out.push(format!("{loc_str}  {kind} {name}{container}"));
    }
    if arr.len() > 100 {
        out.push(format!(
            "… ({} weitere Symbole abgeschnitten)",
            arr.len() - 100
        ));
    }
    if out.is_empty() {
        "(keine Symbole gefunden — Query zu unspezifisch, oder der Language-Server indexiert noch)"
            .into()
    } else {
        out.join("\n")
    }
}

/// Project-wide symbol search (`workspace/symbol`): find symbols by name across the whole
/// workspace, not just one file. `ext_path` is any file of the target language — its
/// extension selects the language server to query.
pub async fn workspace_symbols(root: &str, ext_path: &str, query: &str) -> Result<String> {
    let ext = Path::new(ext_path)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| anyhow!("no extension"))?;
    let client = client_for(ext, root)
        .await
        .ok_or_else(|| anyhow!("kein Language-Server für .{ext} verfügbar"))?;
    let res = client
        .request("workspace/symbol", json!({ "query": query }))
        .await?;
    Ok(fmt_workspace_symbols(&res))
}

/// Format a `callHierarchy/incomingCalls` (callers) or `callHierarchy/outgoingCalls`
/// (callees) response. Each entry wraps a CallHierarchyItem under `from` (incoming) or
/// `to` (outgoing) with `name`, `kind`, `uri`, and a `selectionRange`/`range`.
fn fmt_call_hierarchy(res: &Value, outgoing: bool) -> String {
    let arr = res.as_array().cloned().unwrap_or_default();
    let edge = if outgoing { "to" } else { "from" };
    let mut out = Vec::new();
    for call in arr.iter().take(100) {
        let item = match call.get(edge) {
            Some(i) => i,
            None => continue,
        };
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = item
            .get("kind")
            .and_then(Value::as_i64)
            .map(symbol_kind_name)
            .unwrap_or("symbol");
        let uri = item.get("uri").and_then(Value::as_str).unwrap_or("");
        let line = item
            .get("selectionRange")
            .or_else(|| item.get("range"))
            .and_then(|r| r.get("start"))
            .and_then(|p| p.get("line"))
            .and_then(Value::as_i64)
            .map(|l| l + 1);
        let loc = match line {
            Some(l) => format!("{}:{}", uri_to_path(uri), l),
            None => uri_to_path(uri),
        };
        let n_sites = call
            .get("fromRanges")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        let sites = if n_sites > 1 {
            format!("  ({n_sites}×)")
        } else {
            String::new()
        };
        out.push(format!("{loc}  {kind} {name}{sites}"));
    }
    if arr.len() > 100 {
        out.push(format!("… ({} weitere abgeschnitten)", arr.len() - 100));
    }
    if out.is_empty() {
        let what = if outgoing {
            "Aufrufe von hier"
        } else {
            "Aufrufer"
        };
        format!("(keine {what} gefunden)")
    } else {
        out.join("\n")
    }
}

/// Call hierarchy at a position: who CALLS this function (`outgoing=false`, incomingCalls)
/// or what this function CALLS (`outgoing=true`, outgoingCalls). Two LSP steps:
/// prepareCallHierarchy → the symbol's item, then incoming/outgoingCalls on that item.
pub async fn call_hierarchy(
    root: &str,
    abs_path: &str,
    line: i64,
    character: i64,
    outgoing: bool,
) -> Result<String> {
    let ext = Path::new(abs_path)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| anyhow!("no extension"))?;
    let client = client_for(ext, root)
        .await
        .ok_or_else(|| anyhow!("kein Language-Server für .{ext} verfügbar"))?;
    let text = tokio::fs::read_to_string(abs_path).await?;
    client.sync_file(abs_path, lang_for(ext), &text).await?;
    let uri = path_to_uri(abs_path);
    let pos = json!({"line": (line - 1).max(0), "character": (character - 1).max(0)});
    let prepared = client
        .request(
            "textDocument/prepareCallHierarchy",
            json!({"textDocument": {"uri": uri}, "position": pos}),
        )
        .await?;
    // A `null` result means the server doesn't support call hierarchy (e.g. yaml/json/
    // bash/toml servers) — distinguish that from an empty array (no symbol at the cursor).
    if prepared.is_null() {
        return Ok(format!(
            "(call hierarchy nicht unterstützt vom .{ext}-Language-Server)"
        ));
    }
    let item = match prepared.as_array().and_then(|a| a.first()).cloned() {
        Some(i) => i,
        None => return Ok("(kein aufrufbares Symbol an dieser Position)".into()),
    };
    let method = if outgoing {
        "callHierarchy/outgoingCalls"
    } else {
        "callHierarchy/incomingCalls"
    };
    let res = client.request(method, json!({ "item": item })).await?;
    Ok(fmt_call_hierarchy(&res, outgoing))
}

fn fmt_hover(res: &Value) -> String {
    let c = res.get("contents");
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("(kein Hover)")
            .to_string(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.get("value").and_then(Value::as_str).map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "(kein Hover)".into(),
    }
}

/// Semantic navigation: op ∈ definition|references|hover|symbols. `line`/`character`
/// are 1-based as the agent sees them; LSP is 0-based.
pub async fn navigate(
    root: &str,
    abs_path: &str,
    op: &str,
    line: i64,
    character: i64,
) -> Result<String> {
    let ext = Path::new(abs_path)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| anyhow!("no extension"))?;
    let client = client_for(ext, root)
        .await
        .ok_or_else(|| anyhow!("kein Language-Server für .{ext} verfügbar"))?;
    let text = tokio::fs::read_to_string(abs_path).await?;
    client.sync_file(abs_path, lang_for(ext), &text).await?;
    let uri = path_to_uri(abs_path);
    let pos = json!({"line": (line - 1).max(0), "character": (character - 1).max(0)});
    let (method, params) = match op {
        "definition" => (
            "textDocument/definition",
            json!({"textDocument": {"uri": uri}, "position": pos}),
        ),
        "references" => (
            "textDocument/references",
            json!({"textDocument": {"uri": uri}, "position": pos, "context": {"includeDeclaration": true}}),
        ),
        "hover" => (
            "textDocument/hover",
            json!({"textDocument": {"uri": uri}, "position": pos}),
        ),
        "symbols" => (
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
        ),
        other => return Err(anyhow!("unbekannte Operation: {other}")),
    };
    let res = client.request(method, params).await?;
    Ok(match op {
        "symbols" => fmt_symbols(&res),
        "hover" => fmt_hover(&res),
        _ => fmt_locations(&res),
    })
}
