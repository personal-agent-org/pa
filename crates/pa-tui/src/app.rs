//! TUI application state + the async event loop.
//!
//! Three input sources are multiplexed with `tokio::select!`: terminal key events
//! (crossterm `EventStream`), background work results + live run events (an mpsc of
//! `AppMsg`), and a redraw tick (spinner animation). All network work happens in spawned
//! tasks that report back as `AppMsg`, so the render loop never blocks.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::api::{self, ApiClient};
use crate::composer;
use crate::i18n::{t, Msg, Op};
use crate::picker::FilterList;
use crate::sse::{attach_run, stream_run, RunKind, StreamMsg};
use crate::ui;
use crate::ws::{self, ServerFrame, WsHandle, WsSubQuestion};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Chat,
    Inbox,
    /// A sub-agent's transcript, opened from the agents drawer — uses the full chat view.
    Transcript,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InboxFocus {
    List,
    Composer,
}

/// Status filters cycled with `f` in the inbox (None = all).
const INBOX_FILTERS: [Option<&str>; 6] = [
    None,
    Some("new"),
    Some("needs_reply"),
    Some("answered"),
    Some("seen"),
    Some("open"),
];

/// The status key for the inbox filter at `idx` ("all" for the None/unfiltered slot).
pub fn filter_label(idx: usize) -> &'static str {
    INBOX_FILTERS.get(idx).copied().flatten().unwrap_or("all")
}

/// Best-effort MIME type from a filename extension (the backend re-sniffs; this is a hint).
pub fn guess_media_type(filename: &str) -> String {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// A compact glyph for a chat's mode, mirroring the web frontend's per-mode icon
/// (`CHAT_MODES`: standard→`chat`, coding→`code`). Unknown/custom modes get a neutral mark.
pub fn mode_icon(mode: &str) -> &'static str {
    match mode {
        "standard" => "💬",
        "coding" => "💻",
        _ => "✦",
    }
}

/// Parse a `#rrggbb` (or `rrggbb`) hex colour into RGB; None for empty/malformed input.
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

/// Format an ISO-8601 timestamp (UTC) as a compact `MM-DD HH:MM` for the session list.
/// Returns "" if it can't be parsed (so callers can just skip an empty string).
pub fn short_time(iso: &str) -> String {
    let Some((date, rest)) = iso.split_once('T') else {
        return String::new();
    };
    let hm: String = rest.chars().take(5).collect(); // "HH:MM"
    let md = date.get(5..).unwrap_or(date); // drop "YYYY-"
    if hm.len() == 5 {
        format!("{md} {hm}")
    } else {
        md.to_string()
    }
}

/// Indices into `chats` whose title matches `query` (case-insensitive substring);
/// the full list, in order, when the query is blank.
pub fn filter_sessions(chats: &[api::Chat], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    chats
        .iter()
        .enumerate()
        .filter(|(_, c)| q.is_empty() || c.title.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    None,
    Models,
    Sessions,
    Agents,
    Help,
    Approval,
    Question,
    Attach,
    Security,
    Integrations,
    Memory,
}

/// World-memory domains a `scoped` policy can include (mirrors the backend `ALL_DOMAINS`).
pub const MEM_DOMAINS: [&str; 4] = ["people", "work", "places_devices", "notes_topics"];
/// World-memory sources a `scoped` policy can include (mirrors the backend `ALL_SOURCES`).
pub const MEM_SOURCES: [&str; 4] = ["preferences", "stated", "inferred", "observed"];

/// A row in the memory-access picker (F9). Mode rows are radio (row 0 = "(default)" = inherit
/// the user policy); domain/source rows are checkboxes, shown only in `scoped` mode.
#[derive(Clone, Copy)]
pub enum MemRow {
    /// 0 = default (inherit) · 1 = full · 2 = none · 3 = scoped.
    Mode(usize),
    Domain(usize),
    Source(usize),
}

/// One visible row in the integrations picker: a group header, or a tool under a group.
/// Both carry indices into `App::integrations` (and the group's `tools`), so the row list is a
/// cheap, `Copy` projection recomputed each frame from the filter.
#[derive(Clone, Copy)]
pub enum IntRow {
    /// A group header (index into `integrations`); toggling flips the whole group.
    Header(usize),
    /// A tool: (group index, tool index within that group's `tools`).
    Tool(usize, usize),
}

/// Tool-call security modes (parity with the web SecurityPicker / backend SECURITY_MODES).
/// Row 0 in the picker is "(default)" = inherit the chat/user default; these follow.
pub const SECURITY_MODES: [&str; 3] = ["autonomous", "approve_each", "judge"];

/// Max inline attachments per run (the backend caps RunCreate.attachments at 6).
const MAX_ATTACHMENTS: usize = 6;

/// Built-in client-side actions (UI ops, not prompts). Prompt-style commands are NOT
/// hardcoded here — they come from the server as custom commands (`GET /commands`).
const COMMANDS: [&str; 20] = [
    "/btw",
    "/retry",
    "/steer",
    "/model",
    "/think",
    "/tools",
    "/integrations",
    "/memory",
    "/security",
    "/attach",
    "/agents",
    "/new",
    "/main",
    "/rename",
    "/summarize",
    "/proofread",
    "/inbox",
    "/computer-service",
    "/logout",
    "/help",
];

/// Built-in commands whose `/name…` prefix matches the input (custom commands are merged in
/// by `App::palette`, which also applies mode filtering).
pub fn command_matches(input: &str) -> Vec<&'static str> {
    let head = input.split_whitespace().next().unwrap_or(input);
    COMMANDS
        .iter()
        .copied()
        .filter(|c| c.starts_with(head))
        .collect()
}

/// One entry in the composer's slash palette: the `/name` plus a one-line description.
pub struct PaletteItem {
    pub name: String,
    pub desc: String,
}

/// Whether a custom command's `mode` ("standard"|"coding"|null) is visible in `coding` mode.
fn custom_visible(mode: Option<&str>, coding: bool) -> bool {
    match mode {
        Some("coding") => coding,
        Some("standard") => !coding,
        _ => true, // null / unknown → all modes
    }
}

/// Expand a custom command's template with its arguments (mirrors the web's `expandTemplate`).
fn expand_template(template: &str, args: &str) -> String {
    if template.contains("$ARGUMENTS") {
        template.replace("$ARGUMENTS", args).trim().to_string()
    } else if template.contains("{args}") {
        template.replace("{args}", args).trim().to_string()
    } else if args.is_empty() {
        template.to_string()
    } else {
        format!("{template} {args}").trim().to_string()
    }
}

/// Build a `MemoryAccess` policy with the given mode + axis lists (a small constructor so the
/// picker's mode branches stay terse).
fn mem_policy(
    mode: &str,
    domains: Option<Vec<String>>,
    sources: Option<Vec<String>>,
) -> api::MemoryAccess {
    api::MemoryAccess {
        mode: mode.to_string(),
        domains,
        sources,
    }
}

/// A file staged for the next message, sent inline as base64 (multimodal input).
#[derive(Clone)]
pub struct Attachment {
    pub filename: String,
    pub media_type: String,
    pub data: String, // base64
}

/// A pending device tool-call awaiting the user's HITL decision (from a `tool_approval` push).
pub struct ApprovalCard {
    pub approval_id: String,
    pub device_name: String,
    pub tool: String,
    pub command: String,
    /// 0 = allow once · 1 = allow + remember · 2 = reject.
    pub sel: usize,
}

/// One sub-question being answered: per-option selection state + an optional custom answer.
pub struct SubQuestion {
    pub question: String,
    pub options: Vec<String>,
    pub multi_select: bool,
    pub allow_custom: bool,
    pub selected: Vec<bool>,
    pub custom: String,
}

impl SubQuestion {
    fn from_ws(q: WsSubQuestion) -> SubQuestion {
        let selected = vec![false; q.options.len()];
        SubQuestion {
            question: q.question,
            options: q.options,
            multi_select: q.multi_select,
            allow_custom: q.allow_custom,
            selected,
            custom: String::new(),
        }
    }

    /// The chosen answers for this sub-question (selected options, or the custom text).
    fn picks(&self) -> Vec<String> {
        let custom = self.custom.trim();
        let chosen: Vec<String> = self
            .options
            .iter()
            .zip(&self.selected)
            .filter(|(_, &on)| on)
            .map(|(o, _)| o.clone())
            .collect();
        if self.multi_select {
            let mut picks = chosen;
            if !custom.is_empty() {
                picks.push(custom.to_string());
            }
            picks
        } else if !custom.is_empty() {
            vec![custom.to_string()]
        } else {
            chosen.into_iter().take(1).collect()
        }
    }

    /// Toggle (multi-select) or exclusively set (single-select) the option at `idx`.
    fn toggle(&mut self, idx: usize) {
        if idx >= self.selected.len() {
            return;
        }
        if self.multi_select {
            self.selected[idx] = !self.selected[idx];
        } else {
            self.selected.iter_mut().for_each(|s| *s = false);
            self.selected[idx] = true;
            self.custom.clear(); // a picked option supersedes a half-typed custom answer
        }
    }
}

/// A deferred agent question (from an `agent_question` push); one or more sub-questions.
pub struct QuestionCard {
    pub chat_id: String,
    pub question_id: String,
    pub multi: bool,
    pub subs: Vec<SubQuestion>,
    pub q_idx: usize,
    pub opt_idx: usize,
}

/// A sub-agent / background task spawned within a chat, tracked from `subagent_update`
/// pushes and shown in the agents drawer (F5). Keyed by `run_id`; updated in place as it
/// moves running → completed/failed.
pub struct Subagent {
    pub run_id: String,
    pub kind: String,
    pub label: String,
    pub status: String, // running | completed | failed
    pub background: bool,
    pub error: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: Option<f64>,
    pub tool_calls: i64,
    /// ISO timestamp (sortable) for newest-first ordering in the drawer.
    pub started_at: Option<String>,
}

pub struct UiTool {
    pub id: String,
    pub name: String,
    pub args: String,
    pub result: Option<String>,
}

pub struct UiMessage {
    pub role: String,
    pub text: String,
    pub thinking: String,
    pub tools: Vec<UiTool>,
    pub usage: Option<api::MsgUsage>,
    pub pending: bool,
}

impl UiMessage {
    fn assistant_pending() -> UiMessage {
        UiMessage {
            role: "assistant".into(),
            text: String::new(),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: true,
        }
    }
}

/// Everything the render loop and background tasks exchange.
pub enum AppMsg {
    Me(api::Me),
    Chats(Vec<api::Chat>),
    Models(Vec<api::Model>),
    Commands(Vec<api::CustomCommand>),
    Messages {
        chat_id: String,
        msgs: Vec<UiMessage>,
    },
    ChatCreated(api::Chat),
    Stream(StreamMsg),
    Server(ServerFrame),
    Conversations(api::ConversationList),
    Conversation(api::ConversationDetail),
    AgentRuns {
        chat_id: String,
        items: Vec<api::AgentRun>,
    },
    /// Output (+ exit code) of a one-shot `!cmd` shell run in the coding workspace.
    ShellOutput {
        output: String,
        code: Option<i32>,
    },
    /// A sub-agent transcript opened from the agents drawer.
    Transcript {
        title: String,
        msgs: Vec<UiMessage>,
    },
    Suggested(String),
    Attached(Attachment),
    /// The per-chat integrations catalog + the chat's current `disabled_tools` deny-list.
    Integrations {
        groups: Vec<api::IntegrationGroup>,
        disabled: Vec<String>,
    },
    /// The chat's context-fill + cumulative usage for the status-bar telemetry.
    Context {
        chat_id: String,
        ctx: api::ChatContext,
    },
    /// The chat's per-chat memory-access policy (None = inherit the user default).
    Memory {
        chat_id: String,
        access: Option<api::MemoryAccess>,
    },
    OpError(Op, String),
}

pub struct App {
    client: Arc<ApiClient>,
    tx: UnboundedSender<AppMsg>,
    ws: Option<WsHandle>,
    pub live: bool,
    pub view: View,

    pub me: Option<api::Me>,
    /// User accent colour (RGB) from the web `ui.accent` pref; None = built-in default.
    pub accent: Option<(u8, u8, u8)>,
    pub chats: Vec<api::Chat>,
    /// Selected row *within the filtered session results* (see `session_results`).
    pub sel_chat: usize,
    /// Live filter typed into the sessions popup (matches chat titles, case-insensitive).
    pub session_query: String,
    pub current_chat: Option<String>,
    pub messages: Vec<UiMessage>,

    /// Status-bar telemetry for the open chat: context-fill + cumulative usage (refreshed on
    /// chat-open and after each run). `run_started` times the active run for the elapsed display.
    pub context: Option<api::ChatContext>,
    run_started: Option<std::time::Instant>,

    pub input: String,
    /// Cursor as a BYTE index into `input` (always on a char boundary). Drives editing + render.
    pub cursor: usize,
    /// Sent prompts, oldest→newest, for ↑/↓ recall in the composer.
    pub input_history: Vec<String>,
    /// None = editing live; Some(i) = viewing input_history[i]. `hist_draft` holds the live
    /// line saved when history navigation began, restored when stepping back past the newest.
    pub hist_idx: Option<usize>,
    pub hist_draft: String,
    pub popup: Popup,
    pub attachments: Vec<Attachment>,
    pub attach_input: String,

    /// User-authored slash commands (prompt templates) fetched from the server.
    pub commands: Vec<api::CustomCommand>,

    /// Per-chat integrations picker (F8): the assembled tool catalog, the live deny-list, and
    /// the shared filter/cursor primitive. Tools NOT in `disabled_tools` are enabled for the chat.
    pub integrations: Vec<api::IntegrationGroup>,
    pub disabled_tools: std::collections::HashSet<String>,
    pub int_filter: FilterList,

    /// Per-chat memory-access picker (F9): the live policy (None = inherit user default) + the
    /// highlighted row within `mem_rows()`.
    pub mem_access: Option<api::MemoryAccess>,
    pub mem_sel: usize,

    pub models: Vec<api::Model>,
    pub sel_model_row: usize,
    /// None = use the chat default; Some("auto"|"provider:model") = explicit override.
    pub model: Option<String>,
    pub tools_enabled: bool,
    /// Reasoning effort for the next runs: None = model default, Some("off"|level).
    pub thinking: Option<String>,
    /// Tool-call security mode for the next runs: None = chat/user default, Some(mode).
    pub security_mode: Option<String>,
    /// Highlighted row in the security picker (0 = "(default)", then SECURITY_MODES).
    pub sel_security_row: usize,
    /// Highlighted row in the slash-command menu (while the composer holds a `/…`).
    pub cmd_sel: usize,

    /// Index of the in-flight `!cmd` shell message awaiting its output (independent of a
    /// streaming chat run, so a shell command never blocks the composer or a turn).
    shell_idx: Option<usize>,

    pub streaming: bool,
    pub active_run_id: Option<String>,
    /// Messages typed while a run streams, queued server-side and drained when it settles
    /// (local mirror of the count, for the composer chip).
    pub followup_count: usize,
    /// Set when Ctrl+M was pressed but the main chat wasn't loaded yet — open it on arrival.
    pending_main: bool,
    /// False until the first chat list lands and we auto-open the main chat (startup default).
    initial_chat_opened: bool,
    pending_idx: Option<usize>,
    /// A live run to attach to once this chat's history has loaded (reconnect on open).
    pending_attach: Option<String>,

    /// Pending device tool-call approvals, oldest first. The front card is the one shown;
    /// each decision pops it and reveals the next, so a burst (e.g. several web-searches)
    /// is all answered instead of only the first (the rest would hang the run otherwise).
    pub approvals: std::collections::VecDeque<ApprovalCard>,
    pub question: Option<QuestionCard>,

    /// Sub-agents / background tasks per chat (`chat_id` → list), tracked live for the
    /// agents drawer (F5). `sel_agent` is the highlighted row in that popup.
    pub subagents: std::collections::HashMap<String, Vec<Subagent>>,
    pub sel_agent: usize,
    /// Live filter typed into the agents drawer (matches kind / task label / status).
    pub agent_query: String,
    /// A sub-agent transcript opened from the drawer (Popup::Transcript): rendered messages,
    /// a title, and a scroll offset (lines from the bottom).
    pub transcript: Vec<UiMessage>,
    pub transcript_title: String,
    pub transcript_scroll: usize,

    // inbox / conversations
    pub convs: Vec<api::ConversationSummary>,
    pub conv_counts: std::collections::HashMap<String, i64>,
    pub sel_conv: usize,
    pub conv_filter: usize, // index into INBOX_FILTERS
    pub conv_detail: Option<api::ConversationDetail>,
    pub inbox_focus: InboxFocus,
    pub reply_input: String,
    pub conv_scroll: usize,

    pub status: String,
    pub scroll: usize, // lines scrolled up from the bottom (0 = stuck to bottom)
    pub spinner: usize,
    pub should_quit: bool,
    /// Set by `/computer-service`; installation starts only after raw mode has been restored.
    pub computer_service_request: Option<String>,
}

impl App {
    fn new(client: Arc<ApiClient>, tx: UnboundedSender<AppMsg>) -> App {
        App {
            client,
            tx,
            ws: None,
            live: false,
            view: View::Chat,
            me: None,
            accent: None,
            chats: Vec::new(),
            sel_chat: 0,
            session_query: String::new(),
            current_chat: None,
            messages: Vec::new(),
            context: None,
            run_started: None,
            input: String::new(),
            cursor: 0,
            input_history: Vec::new(),
            hist_idx: None,
            hist_draft: String::new(),
            popup: Popup::None,
            attachments: Vec::new(),
            attach_input: String::new(),
            commands: Vec::new(),
            integrations: Vec::new(),
            disabled_tools: std::collections::HashSet::new(),
            int_filter: FilterList::new(),
            mem_access: None,
            mem_sel: 0,
            models: Vec::new(),
            sel_model_row: 0,
            model: None,
            tools_enabled: true,
            thinking: None,
            security_mode: None,
            sel_security_row: 0,
            cmd_sel: 0,
            shell_idx: None,
            streaming: false,
            active_run_id: None,
            followup_count: 0,
            pending_main: false,
            initial_chat_opened: false,
            pending_idx: None,
            pending_attach: None,
            approvals: std::collections::VecDeque::new(),
            question: None,
            subagents: std::collections::HashMap::new(),
            sel_agent: 0,
            agent_query: String::new(),
            transcript: Vec::new(),
            transcript_title: String::new(),
            transcript_scroll: 0,
            convs: Vec::new(),
            conv_counts: std::collections::HashMap::new(),
            sel_conv: 0,
            conv_filter: 0,
            conv_detail: None,
            inbox_focus: InboxFocus::List,
            reply_input: String::new(),
            conv_scroll: 0,
            status: t(Msg::Ready),
            scroll: 0,
            spinner: 0,
            should_quit: false,
            computer_service_request: None,
        }
    }

    // ── background work ──────────────────────────────────────────────────────

    fn bootstrap(&self) {
        self.spawn_me();
        self.spawn_chats();
        self.spawn_models();
        self.spawn_commands();
    }

    fn spawn_commands(&self) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.list_commands().await {
                Ok(cmds) => {
                    let _ = tx.send(AppMsg::Commands(cmds));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Commands, format!("{e:#}")));
                }
            }
        });
    }

    /// Build the slash palette for the current input + chat mode: built-in actions plus the
    /// user's custom commands (mode-filtered). The single source of truth for render + nav.
    pub fn palette(&self) -> Vec<PaletteItem> {
        let head = self.input.split_whitespace().next().unwrap_or(&self.input);
        let coding = self.is_mode("coding");
        let mut out: Vec<PaletteItem> = command_matches(head)
            .into_iter()
            .map(|name| PaletteItem {
                name: name.to_string(),
                desc: t(Msg::CmdDesc(name)),
            })
            .collect();
        for c in &self.commands {
            let name = format!("/{}", c.name);
            if custom_visible(c.mode.as_deref(), coding) && name.starts_with(head) {
                out.push(PaletteItem {
                    name,
                    desc: c.description.clone().unwrap_or_default(),
                });
            }
        }
        out
    }

    fn spawn_me(&self) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.me().await {
                Ok(me) => {
                    let _ = tx.send(AppMsg::Me(me));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Me, format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_chats(&self) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.list_chats().await {
                Ok(chats) => {
                    let _ = tx.send(AppMsg::Chats(chats));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Chats, format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_models(&self) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.list_models().await {
                Ok(models) => {
                    let _ = tx.send(AppMsg::Models(models));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Models, format!("{e:#}")));
                }
            }
        });
    }

    fn open_chat(&mut self, chat_id: String) {
        self.current_chat = Some(chat_id.clone());
        self.messages.clear();
        self.scroll = 0;
        self.context = None; // drop the previous chat's telemetry until this one's lands
        self.mem_access = None; // and the previous chat's memory policy (reloaded on picker open)
        self.status = t(Msg::LoadingHistory);
        // Load this chat's sub-agent history so the drawer + running-count chip are accurate.
        self.spawn_agent_runs(chat_id.clone());
        // Seed the status-bar telemetry (context fill + cumulative tokens/cost).
        self.spawn_context(chat_id.clone());
        // If a run is in flight on this chat, reconnect to its live stream once history loads.
        self.pending_attach = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .filter(|c| c.active)
            .and_then(|c| c.active_run_id.clone());
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.list_messages(&chat_id).await {
                Ok(msgs) => {
                    let ui = msgs.into_iter().map(convert_message).collect();
                    let _ = tx.send(AppMsg::Messages { chat_id, msgs: ui });
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::History, format!("{e:#}")));
                }
            }
        });
    }

    fn new_chat(&self) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        let title = t(Msg::NewChatTitle);
        tokio::spawn(async move {
            match c.create_chat(&title).await {
                Ok(chat) => {
                    let _ = tx.send(AppMsg::ChatCreated(chat));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::CreateChat, format!("{e:#}")));
                }
            }
        });
    }

    /// Begin (or resume) streaming an existing run into a fresh pending assistant turn.
    fn start_attach(&mut self, chat_id: String, run_id: String) {
        self.messages.push(UiMessage::assistant_pending());
        self.pending_idx = Some(self.messages.len() - 1);
        self.streaming = true;
        self.active_run_id = Some(run_id.clone());
        self.scroll = 0;
        self.status = t(Msg::ConnectingRun);
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move { attach_run(c, chat_id, run_id, tx).await });
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(chat_id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        self.remember_prompt(&text);
        self.clear_input();
        // A run is already streaming → queue this as a mid-run follow-up instead of blocking.
        if self.streaming {
            self.enqueue_followup(chat_id, text);
            return;
        }
        // Stage attachments for this turn (cleared from the composer once sent).
        let attachments = std::mem::take(&mut self.attachments);
        self.start_run(chat_id, text, attachments);
    }

    /// Start a fresh run for `text` (+ optional attachments) and stream it into a new turn.
    fn start_run(&mut self, chat_id: String, text: String, attachments: Vec<Attachment>) {
        // Show the attached filenames under the user's message so the turn is self-documenting.
        let mut display = text.clone();
        if !attachments.is_empty() {
            let names: Vec<&str> = attachments.iter().map(|a| a.filename.as_str()).collect();
            display.push_str(&format!("\n📎 {}", names.join(", ")));
        }

        self.messages.push(UiMessage {
            role: "user".into(),
            text: display,
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        });
        self.messages.push(UiMessage::assistant_pending());
        self.pending_idx = Some(self.messages.len() - 1);
        self.streaming = true;
        self.scroll = 0;
        self.status = t(Msg::AgentWorking);

        let mut body = serde_json::json!({
            "prompt": text,
            "tools_enabled": self.tools_enabled,
        });
        self.apply_run_opts(&mut body);
        if !attachments.is_empty() {
            body["attachments"] = serde_json::Value::Array(
                attachments
                    .iter()
                    .map(|a| {
                        serde_json::json!({
                            "filename": a.filename,
                            "media_type": a.media_type,
                            "data": a.data,
                        })
                    })
                    .collect(),
            );
        }

        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move { stream_run(c, RunKind::New, chat_id, body, tx).await });
    }

    /// Layer the current model + thinking selection onto a run body.
    fn apply_run_opts(&self, body: &mut serde_json::Value) {
        if let Some(m) = &self.model {
            body["model"] = serde_json::Value::String(m.clone());
        }
        match self.thinking.as_deref() {
            Some("off") => body["thinking"] = serde_json::Value::Bool(false),
            Some(level) => body["thinking"] = serde_json::Value::String(level.to_string()),
            None => {}
        }
        if let Some(mode) = &self.security_mode {
            body["security_mode"] = serde_json::Value::String(mode.clone());
        }
    }

    /// `!cmd` — run a shell command one-shot in the chat's coding workspace (PTY terminal).
    /// Only in coding mode; output streams back into a dedicated transcript message.
    fn run_shell(&mut self) {
        let cmd = self.input.trim_start_matches('!').trim().to_string();
        self.remember_prompt(&self.input.clone());
        self.clear_input();
        if cmd.is_empty() {
            return;
        }
        let Some(chat_id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        // Workspace shell is a coding-mode affordance only.
        if !self.is_mode("coding") {
            self.status = t(Msg::ShellCodingOnly);
            return;
        }
        // One shell command at a time (keeps the single fill-index simple).
        if self.shell_idx.is_some() {
            self.status = t(Msg::ShellBusy);
            return;
        }
        self.messages.push(UiMessage {
            role: "shell".into(),
            text: format!("$ {cmd}"),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: true,
        });
        self.shell_idx = Some(self.messages.len() - 1);
        self.scroll = 0;
        self.status = t(Msg::ShellRunning);
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move { crate::terminal::run_command(c, chat_id, cmd, tx).await });
    }

    /// Queue a message typed during a live run (server-side), to be drained when it settles.
    fn enqueue_followup(&mut self, chat_id: String, text: String) {
        self.followup_count += 1;
        self.status = t(Msg::FollowupQueued(self.followup_count));
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Err(e) = c.enqueue_followup(&chat_id, &text).await {
                let _ = tx.send(AppMsg::OpError(Op::Followup, format!("{e:#}")));
            }
        });
    }

    /// Drop all queued follow-ups (server + local mirror) — e.g. on cancel.
    fn clear_followups(&mut self) {
        if self.followup_count == 0 {
            return;
        }
        self.followup_count = 0;
        let Some(chat_id) = self.current_chat.clone() else {
            return;
        };
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Err(e) = c.clear_followups(&chat_id).await {
                let _ = tx.send(AppMsg::OpError(Op::Followup, format!("{e:#}")));
            }
        });
    }

    /// `/steer <text>` — a mid-run course-correction. While a run streams, the text is enqueued
    /// onto the run's follow-up queue, which the backend injects at the next tool-call boundary
    /// (durable runs) so the agent adjusts course WITHOUT a restart. The steer is echoed inline
    /// (a queued follow-up is otherwise invisible — just a counter), so the user sees what was
    /// injected. With no run in flight there's nothing to steer → it's sent as a normal turn.
    fn steer(&mut self, text: String) {
        let Some(chat_id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        if !self.streaming {
            self.start_run(chat_id, text, Vec::new());
            return;
        }
        // Show the steer inline (below the in-progress answer it corrects).
        self.messages.push(UiMessage {
            role: "steer".into(),
            text,
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        });
        self.scroll = 0;
        let injected = self
            .messages
            .last()
            .map(|m| m.text.clone())
            .unwrap_or_default();
        self.enqueue_followup(chat_id, injected);
        // `enqueue_followup` set a generic "queued" status; a steer gets a clearer one.
        self.status = t(Msg::SteerSent);
    }

    /// Send a templated prompt (a coding slash command) as a normal turn — or queue it as a
    /// follow-up if a run is already streaming.
    fn run_prompt(&mut self, prompt: String) {
        let Some(chat_id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        if self.streaming {
            self.enqueue_followup(chat_id, prompt);
        } else {
            self.start_run(chat_id, prompt, Vec::new());
        }
    }

    /// `/btw` — an ephemeral side question over the chat's context (not persisted server-side).
    fn start_side_run(&mut self, prompt: String) {
        let Some(chat_id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        self.messages.push(UiMessage {
            role: "user".into(),
            text: format!("/btw {prompt}"),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        });
        let mut pending = UiMessage::assistant_pending();
        pending.role = "side".into();
        self.messages.push(pending);
        self.pending_idx = Some(self.messages.len() - 1);
        self.streaming = true;
        self.scroll = 0;
        self.status = t(Msg::AgentWorking);

        let mut body = serde_json::json!({ "prompt": prompt });
        if let Some(m) = &self.model {
            body["model"] = serde_json::Value::String(m.clone());
        }
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move { stream_run(c, RunKind::Side, chat_id, body, tx).await });
    }

    /// `/retry` — regenerate the last answer (server truncates at the last user turn).
    fn start_rerun(&mut self) {
        let Some(chat_id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        // Drop the last assistant turn locally; the fresh one streams in its place.
        if self
            .messages
            .last()
            .map(|m| m.role != "user")
            .unwrap_or(false)
        {
            self.messages.pop();
        }
        self.messages.push(UiMessage::assistant_pending());
        self.pending_idx = Some(self.messages.len() - 1);
        self.streaming = true;
        self.scroll = 0;
        self.status = t(Msg::AgentWorking);

        let mut body = serde_json::json!({});
        self.apply_run_opts(&mut body);
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move { stream_run(c, RunKind::Rerun, chat_id, body, tx).await });
    }

    /// Run a slash command typed into the composer.
    fn run_command(&mut self, input: &str) {
        let input = input.trim();
        let (cmd, rest) = match input.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (input, ""),
        };
        self.remember_prompt(&self.input.clone());
        self.clear_input();
        match cmd {
            "/btw" => {
                if rest.is_empty() {
                    self.status = t(Msg::SideQuestionEmpty);
                } else if !self.streaming {
                    self.start_side_run(rest.to_string());
                }
            }
            "/retry" | "/rerun" => {
                if !self.streaming {
                    self.start_rerun();
                }
            }
            "/steer" => {
                if rest.is_empty() {
                    self.status = t(Msg::SteerEmpty);
                } else {
                    self.steer(rest.to_string());
                }
            }
            "/model" => {
                if rest.is_empty() {
                    self.popup = Popup::Models;
                    self.sync_model_row();
                    if self.models.is_empty() {
                        self.spawn_models();
                    }
                } else {
                    let found = self
                        .models
                        .iter()
                        .find(|m| m.id.contains(rest) || m.model.contains(rest))
                        .map(|m| m.id.clone());
                    if let Some(id) = found {
                        self.model = Some(id.clone());
                        self.status = t(Msg::ModelSet(&id));
                    }
                }
            }
            "/think" => {
                let level = rest.to_ascii_lowercase();
                self.thinking = match level.as_str() {
                    "" | "default" => None,
                    other => Some(other.to_string()),
                };
                let shown = self.thinking.clone().unwrap_or_else(|| "default".into());
                self.status = t(Msg::ThinkingSet(&shown));
            }
            "/tools" => {
                self.tools_enabled = !self.tools_enabled;
                self.status = t(Msg::ToolsState(self.tools_enabled));
            }
            "/security" => {
                let want = rest.to_ascii_lowercase();
                if want.is_empty() {
                    self.open_security_popup();
                } else if want == "default" {
                    self.set_security(None);
                } else if SECURITY_MODES.contains(&want.as_str()) {
                    self.set_security(Some(want));
                } else {
                    self.status = t(Msg::SecurityUnknown(rest));
                }
            }
            "/main" => {
                // No args → jump to the main chat; with args → jump there and stage the
                // message in the composer (a terminal-friendly take on the web's /main).
                self.goto_main_chat();
                if !rest.is_empty() {
                    self.input = rest.to_string();
                    self.cursor = self.input.len();
                }
            }
            "/rename" => {
                if rest.is_empty() {
                    self.status = t(Msg::RenameEmpty);
                } else {
                    self.rename_current_chat(rest.to_string());
                }
            }
            "/summarize" => self.run_prompt(t(Msg::SummarizePrompt)),
            "/proofread" => {
                if rest.is_empty() {
                    self.status = t(Msg::ProofreadEmpty);
                } else {
                    self.run_prompt(t(Msg::ProofreadPrompt(rest)));
                }
            }
            "/attach" => {
                if self.attachments.len() >= MAX_ATTACHMENTS {
                    self.status = t(Msg::AttachTooMany);
                } else if rest.is_empty() {
                    self.popup = Popup::Attach;
                } else {
                    self.spawn_attach(rest.to_string());
                }
            }
            "/agents" => self.open_agents_popup(),
            "/integrations" | "/int" => self.open_integrations_popup(),
            "/memory" => self.open_memory_popup(),
            "/new" => self.new_chat(),
            "/inbox" => {
                if self.convs.is_empty() {
                    self.spawn_conversations();
                }
                self.view = View::Inbox;
            }
            "/computer-service" => {
                self.computer_service_request = Some(if rest.is_empty() {
                    "Computer".to_string()
                } else {
                    rest.to_string()
                });
                self.should_quit = true;
            }
            "/help" => self.popup = Popup::Help,
            "/logout" => self.logout(),
            // Not a built-in → a user's custom command (server template), expanded with args.
            other => {
                let key = other.trim_start_matches('/');
                match self.commands.iter().find(|c| c.name == key).cloned() {
                    Some(c) => self.run_prompt(expand_template(&c.template, rest)),
                    None => self.status = t(Msg::CmdUnknown(other)),
                }
            }
        }
    }

    /// `/logout` — remove the stored credentials and quit (next launch needs `login`).
    fn logout(&mut self) {
        let path = crate::config::config_path();
        let _ = std::fs::remove_file(&path);
        self.status = t(Msg::LogoutDone(&path.display().to_string()));
        self.should_quit = true;
    }

    fn cancel_run(&mut self) {
        // Cancelling discards the queued follow-ups too (they were meant for this turn).
        self.clear_followups();
        // Real server-side cancel over the control WS (ownership-checked, then published to
        // the run's Redis control channel). The run then emits a terminal SSE event that
        // finalises the pending turn. If the socket isn't up, detach locally as a fallback.
        match (&self.ws, &self.active_run_id) {
            (Some(ws), Some(run_id)) if self.live => {
                ws.cancel(run_id);
                self.status = t(Msg::CancelSent);
            }
            _ => {
                if let Some(idx) = self.pending_idx.take() {
                    if let Some(m) = self.messages.get_mut(idx) {
                        m.pending = false;
                        if m.text.is_empty() {
                            m.text = t(Msg::CancelledTag);
                        }
                    }
                }
                self.streaming = false;
                self.active_run_id = None;
                self.status = t(Msg::RunCancelled);
            }
        }
    }

    fn handle_server(&mut self, frame: ServerFrame) {
        match frame {
            ServerFrame::Connected(up) => {
                self.live = up;
                self.status = t(if up {
                    Msg::LiveConnected
                } else {
                    Msg::LiveDisconnected
                });
            }
            ServerFrame::ChatTitle { chat_id, title } => {
                if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.title = title;
                } else {
                    self.spawn_chats();
                }
            }
            ServerFrame::BackgroundResumed { chat_id, run_id } => {
                if self.current_chat.as_deref() == Some(chat_id.as_str()) && !self.streaming {
                    self.start_attach(chat_id, run_id);
                }
            }
            ServerFrame::SubagentUpdate {
                chat_id,
                run_id,
                status,
                kind,
                label,
                background,
                error,
                input_tokens,
                output_tokens,
                cost_usd,
                tool_calls,
                started_at,
            } => {
                self.status = t(Msg::SubagentUpdate {
                    kind: &kind,
                    status: &status,
                    label: &label,
                });
                // Upsert by run_id so a sub-agent moves running → completed/failed in place.
                self.upsert_subagent(
                    chat_id,
                    Subagent {
                        run_id,
                        kind,
                        label,
                        status,
                        background,
                        error,
                        input_tokens,
                        output_tokens,
                        cost_usd,
                        tool_calls,
                        started_at,
                    },
                );
            }
            ServerFrame::ToolApproval {
                approval_id,
                device_name,
                tool,
                command,
                ..
            } => {
                // Queue it (don't clobber an undecided one); show the approval popup if idle.
                self.approvals.push_back(ApprovalCard {
                    approval_id,
                    device_name,
                    tool,
                    command,
                    sel: 0,
                });
                if self.popup == Popup::None {
                    self.popup = Popup::Approval;
                }
            }
            ServerFrame::AgentQuestion {
                chat_id,
                question_id,
                subs,
                multi,
                ..
            } => {
                self.question = Some(QuestionCard {
                    chat_id,
                    question_id,
                    multi,
                    subs: subs.into_iter().map(SubQuestion::from_ws).collect(),
                    q_idx: 0,
                    opt_idx: 0,
                });
                self.popup = Popup::Question;
            }
            ServerFrame::ChatsChanged => {
                // A chat was created/renamed/deleted elsewhere → reload the sidebar live.
                self.spawn_chats();
            }
            ServerFrame::ChatRun {
                chat_id,
                active,
                run_id,
            } => {
                // Mirror the sidebar active dot.
                if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.active = active;
                }
                // Follow a server-started run live (auto-drained follow-up, goal turn, other
                // tab) so it streams here instead of only appearing on the next reload.
                if active
                    && self.current_chat.as_deref() == Some(chat_id.as_str())
                    && !self.streaming
                {
                    if let Some(rid) = run_id {
                        self.start_attach(chat_id, rid);
                    }
                }
            }
            ServerFrame::FollowupsChanged { chat_id, count } => {
                // Keep the queued-count chip consistent with the server queue (another tab
                // enqueued/cleared, or the run drained it mid-turn).
                if self.current_chat.as_deref() == Some(chat_id.as_str()) {
                    self.followup_count = count;
                }
            }
            ServerFrame::InboxChanged => {
                self.status = t(Msg::NoteInbox);
                if self.view == View::Inbox {
                    self.spawn_conversations();
                    // Refresh the open thread too, so a new message / status shows.
                    if let Some(id) = self.conv_detail.as_ref().map(|d| d.entity_id.clone()) {
                        self.open_conversation(id);
                    }
                }
            }
            ServerFrame::Note(note) => self.status = note,
        }
    }

    fn decide_approval(&mut self, approved: bool, remember: bool) {
        let Some(card) = self.approvals.pop_front() else {
            return;
        };
        // Reveal the next queued approval, or close the popup if that was the last.
        self.popup = if self.approvals.is_empty() {
            Popup::None
        } else {
            Popup::Approval
        };
        self.status = t(if approved {
            Msg::ToolAllowed
        } else {
            Msg::ToolRejected
        });
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Err(e) = c
                .decide_approval(&card.approval_id, approved, remember)
                .await
            {
                let _ = tx.send(AppMsg::OpError(Op::Approval, format!("{e:#}")));
            }
        });
    }

    /// Submit the (possibly multi-) question: one selection list per sub-question. Every
    /// sub-question must have at least one pick (a selected option or a custom answer).
    fn answer_question(&mut self) {
        let Some(card) = self.question.as_ref() else {
            return;
        };
        let answers: Vec<Vec<String>> = card.subs.iter().map(|s| s.picks()).collect();
        if answers.iter().any(|a| a.is_empty()) {
            self.status = t(Msg::QuestionIncomplete);
            return;
        }
        let card = self.question.take().unwrap();
        self.popup = Popup::None;
        self.status = t(Msg::AnswerSent);
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Err(e) = c
                .answer_question(&card.chat_id, &card.question_id, &answers, card.multi)
                .await
            {
                let _ = tx.send(AppMsg::OpError(Op::Answer, format!("{e:#}")));
            }
        });
    }

    // ── inbox / conversations ──────────────────────────────────────────────────

    fn toggle_view(&mut self) {
        self.view = match self.view {
            View::Chat => {
                if self.convs.is_empty() {
                    self.spawn_conversations();
                }
                View::Inbox
            }
            View::Inbox => View::Chat,
            // From a transcript, F3 returns to the chat.
            View::Transcript => View::Chat,
        };
    }

    fn current_filter(&self) -> Option<&'static str> {
        INBOX_FILTERS[self.conv_filter]
    }

    fn spawn_conversations(&self) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        let status = self.current_filter().map(String::from);
        tokio::spawn(async move {
            match c.list_conversations(status.as_deref()).await {
                Ok(list) => {
                    let _ = tx.send(AppMsg::Conversations(list));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Conversations, format!("{e:#}")));
                }
            }
        });
    }

    fn open_conversation(&mut self, entity_id: String) {
        self.conv_scroll = 0;
        self.reply_input.clear();
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.get_conversation(&entity_id).await {
                Ok(detail) => {
                    let _ = tx.send(AppMsg::Conversation(detail));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Conversation, format!("{e:#}")));
                }
            }
        });
    }

    fn suggest_reply(&mut self) {
        let Some(detail) = &self.conv_detail else {
            return;
        };
        let entity_id = detail.entity_id.clone();
        self.status = t(Msg::Suggesting);
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.suggest_reply(&entity_id).await {
                Ok(s) => {
                    let _ = tx.send(AppMsg::Suggested(s.body));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Suggest, format!("{e:#}")));
                }
            }
        });
    }

    fn send_reply(&mut self) {
        let body = self.reply_input.trim().to_string();
        let Some(detail) = &self.conv_detail else {
            return;
        };
        if body.is_empty() {
            return;
        }
        let entity_id = detail.entity_id.clone();
        self.reply_input.clear();
        self.inbox_focus = InboxFocus::List;
        self.status = t(Msg::ReplySent);
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.reply_conversation(&entity_id, &body).await {
                // Reload the thread + list so the sent reply + new status show.
                Ok(()) => {
                    if let Ok(detail) = c.get_conversation(&entity_id).await {
                        let _ = tx.send(AppMsg::Conversation(detail));
                    }
                    if let Ok(list) = c.list_conversations(None).await {
                        let _ = tx.send(AppMsg::Conversations(list));
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Reply, format!("{e:#}")));
                }
            }
        });
    }

    /// Resolve (`done`) or dismiss (`read`) the selected conversation, then refresh the list.
    fn resolve_conversation(&mut self, done: bool) {
        let Some(detail) = &self.conv_detail else {
            return;
        };
        let entity_id = detail.entity_id.clone();
        self.status = t(if done {
            Msg::ConvResolved
        } else {
            Msg::ConvDismissed
        });
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = if done {
                c.conversation_done(&entity_id).await
            } else {
                c.conversation_read(&entity_id).await
            };
            match res {
                Ok(()) => {
                    if let Ok(list) = c.list_conversations(None).await {
                        let _ = tx.send(AppMsg::Conversations(list));
                    }
                }
                Err(e) => {
                    let op = if done { Op::Done } else { Op::Dismiss };
                    let _ = tx.send(AppMsg::OpError(op, format!("{e:#}")));
                }
            }
        });
    }

    fn handle_inbox_key(&mut self, code: KeyCode) {
        match self.inbox_focus {
            InboxFocus::List => match code {
                KeyCode::Up => self.sel_conv = self.sel_conv.saturating_sub(1),
                KeyCode::Down => {
                    if self.sel_conv + 1 < self.convs.len() {
                        self.sel_conv += 1;
                    }
                }
                KeyCode::PageUp => self.conv_scroll = self.conv_scroll.saturating_add(5),
                KeyCode::PageDown => self.conv_scroll = self.conv_scroll.saturating_sub(5),
                KeyCode::Enter => {
                    if let Some(c) = self.convs.get(self.sel_conv) {
                        self.open_conversation(c.entity_id.clone());
                    }
                }
                KeyCode::Tab => {
                    if self.conv_detail.is_some() {
                        // Seed an empty reply with the pending draft, if there is one.
                        if self.reply_input.is_empty() {
                            if let Some(b) = self
                                .conv_detail
                                .as_ref()
                                .and_then(|d| d.draft_body())
                                .map(String::from)
                            {
                                self.reply_input = b;
                            }
                        }
                        self.inbox_focus = InboxFocus::Composer;
                    }
                }
                KeyCode::Char('f') => {
                    self.conv_filter = (self.conv_filter + 1) % INBOX_FILTERS.len();
                    self.status = t(Msg::InboxFilter(self.current_filter().unwrap_or("all")));
                    self.sel_conv = 0;
                    self.spawn_conversations();
                }
                KeyCode::Char('s') => {
                    self.suggest_reply();
                    self.inbox_focus = InboxFocus::Composer;
                }
                KeyCode::Char('d') => self.resolve_conversation(true),
                KeyCode::Char('x') => self.resolve_conversation(false),
                _ => {}
            },
            InboxFocus::Composer => match code {
                KeyCode::Enter => self.send_reply(),
                KeyCode::Backspace => {
                    self.reply_input.pop();
                }
                KeyCode::Char(c) => self.reply_input.push(c),
                KeyCode::Esc | KeyCode::Tab => self.inbox_focus = InboxFocus::List,
                _ => {}
            },
        }
    }

    // ── message handling ──────────────────────────────────────────────────────

    fn handle_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::Me(me) => {
                let who = me
                    .email
                    .clone()
                    .or(me.display_name.clone())
                    .unwrap_or_default();
                if !who.is_empty() {
                    self.status = t(Msg::SignedIn(&who));
                }
                // Adopt the user's web accent colour, if set.
                self.accent = me
                    .ui
                    .as_ref()
                    .and_then(|u| u.accent.as_deref())
                    .and_then(parse_hex_color);
                self.me = Some(me);
            }
            AppMsg::Chats(chats) => {
                self.chats = chats;
                if self.sel_chat >= self.chats.len() {
                    self.sel_chat = self.chats.len().saturating_sub(1);
                }
                // On startup, land in the user's main chat by default.
                if !self.initial_chat_opened && !self.chats.is_empty() {
                    self.initial_chat_opened = true;
                    if let Some(id) = self.chats.iter().find(|c| c.is_main).map(|c| c.id.clone()) {
                        self.open_chat(id);
                    }
                }
                // A Ctrl+M issued before the list loaded resolves here.
                if self.pending_main {
                    self.pending_main = false;
                    self.goto_main_chat();
                }
            }
            AppMsg::Models(models) => self.models = models,
            AppMsg::Commands(cmds) => self.commands = cmds,
            AppMsg::Messages { chat_id, msgs } => {
                if self.current_chat.as_deref() == Some(chat_id.as_str()) {
                    self.messages = msgs;
                    self.scroll = 0;
                    self.status = t(Msg::HistoryLoaded);
                    // History is in place — now reconnect to any in-flight run on this chat.
                    if let Some(run_id) = self.pending_attach.take() {
                        self.start_attach(chat_id, run_id);
                    }
                }
            }
            AppMsg::ChatCreated(chat) => {
                let id = chat.id.clone();
                self.chats.insert(0, chat);
                self.sel_chat = 0;
                self.open_chat(id);
            }
            AppMsg::Stream(s) => self.handle_stream(s),
            AppMsg::Server(f) => self.handle_server(f),
            AppMsg::Conversations(list) => {
                self.convs = list.items;
                self.conv_counts = list.counts;
                if self.sel_conv >= self.convs.len() {
                    self.sel_conv = self.convs.len().saturating_sub(1);
                }
            }
            AppMsg::Conversation(detail) => {
                self.conv_scroll = 0;
                self.conv_detail = Some(detail);
            }
            AppMsg::AgentRuns { chat_id, items } => {
                // Merge persisted runs with any live-pushed ones (live wins for run state).
                for it in items {
                    let usage = it.usage.unwrap_or_default();
                    self.upsert_subagent(
                        chat_id.clone(),
                        Subagent {
                            run_id: it.run_id,
                            kind: it.kind,
                            label: it.label,
                            status: it.status,
                            background: it.background,
                            error: it.error,
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cost_usd: usage.cost_usd,
                            tool_calls: it.tool_calls,
                            started_at: it.started_at,
                        },
                    );
                }
            }
            AppMsg::ShellOutput { output, code } => {
                if let Some(idx) = self.shell_idx.take() {
                    if let Some(m) = self.messages.get_mut(idx) {
                        m.pending = false;
                        if !output.is_empty() {
                            m.text.push('\n');
                            m.text.push_str(&output);
                        }
                        // Note a non-zero exit so failures are visible.
                        if let Some(c) = code.filter(|c| *c != 0) {
                            m.text.push_str(&format!("\n[exit {c}]"));
                        }
                    }
                }
                self.status = t(Msg::Ready);
            }
            AppMsg::Suggested(body) => {
                self.reply_input = body;
                self.inbox_focus = InboxFocus::Composer;
                self.status = t(Msg::Ready);
            }
            AppMsg::Attached(att) => {
                if self.attachments.len() < MAX_ATTACHMENTS {
                    self.status = t(Msg::AttachAdded(&att.filename));
                    self.attachments.push(att);
                } else {
                    self.status = t(Msg::AttachTooMany);
                }
            }
            AppMsg::Integrations { groups, disabled } => {
                self.integrations = groups;
                self.disabled_tools = disabled.into_iter().collect();
                self.int_filter.clamp(self.integration_rows().len());
            }
            AppMsg::Context { chat_id, ctx } => {
                // Ignore a late arrival for a chat the user already navigated away from.
                if self.current_chat.as_deref() == Some(chat_id.as_str()) {
                    self.context = Some(ctx);
                }
            }
            AppMsg::Memory { chat_id, access } => {
                if self.current_chat.as_deref() == Some(chat_id.as_str()) {
                    self.mem_access = access;
                    let len = self.mem_rows().len();
                    if self.mem_sel >= len {
                        self.mem_sel = len.saturating_sub(1);
                    }
                }
            }
            AppMsg::Transcript { title, msgs } => {
                self.transcript = msgs;
                self.transcript_title = title;
                self.transcript_scroll = 0;
                self.popup = Popup::None;
                self.view = View::Transcript;
                self.status = t(Msg::Ready);
            }
            AppMsg::OpError(op, e) => self.status = t(Msg::OpError(op, &e)),
        }
    }

    fn handle_stream(&mut self, s: StreamMsg) {
        let Some(idx) = self.pending_idx else {
            return;
        };
        let done = matches!(s, StreamMsg::Finished | StreamMsg::Error(_));
        match s {
            StreamMsg::RunId(rid) => self.active_run_id = Some(rid),
            // Stream dropped → reset the live turn; the server's replay rebuilds it.
            StreamMsg::Reset => {
                let m = &mut self.messages[idx];
                m.text.clear();
                m.thinking.clear();
                m.tools.clear();
                m.usage = None;
                self.scroll = 0;
                self.status = t(Msg::Reconnecting);
            }
            StreamMsg::Text(d) => self.messages[idx].text.push_str(&d),
            StreamMsg::Thinking(d) => self.messages[idx].thinking.push_str(&d),
            StreamMsg::ToolStart { id, name } => {
                self.messages[idx].tools.push(UiTool {
                    id,
                    name,
                    args: String::new(),
                    result: None,
                });
            }
            StreamMsg::ToolArgs { id, delta } => {
                if let Some(t) = tool_mut(&mut self.messages[idx].tools, &id) {
                    t.args.push_str(&delta);
                }
            }
            StreamMsg::ToolResult { id, content } => {
                if let Some(t) = tool_mut(&mut self.messages[idx].tools, &id) {
                    t.result = Some(content);
                }
            }
            StreamMsg::Usage {
                model,
                input,
                output,
                cost,
            } => {
                self.messages[idx].usage = Some(api::MsgUsage {
                    input_tokens: input,
                    output_tokens: output,
                    cost_usd: cost,
                    model_name: model,
                });
            }
            StreamMsg::Finished => self.status = t(Msg::Done),
            StreamMsg::Error(e) => {
                let m = &mut self.messages[idx];
                if !m.text.is_empty() {
                    m.text.push('\n');
                }
                m.text.push_str(&t(Msg::ErrorTag(&e)));
                self.status = t(Msg::RunError(&e));
            }
        }
        if done {
            self.messages[idx].pending = false;
            self.pending_idx = None;
            self.streaming = false;
            self.active_run_id = None;
            // Titles may have changed (the agent can rename a chat); refresh the list.
            self.spawn_chats();
            // Refresh this chat's sub-agents (final tallies). Queued follow-ups drain
            // SERVER-SIDE on run finish (one combined run); we follow it via the chat_run
            // push (start_attach) — no client drain trigger.
            if let Some(id) = self.current_chat.clone() {
                self.spawn_agent_runs(id.clone());
                // Refresh the telemetry: new context fill + cumulative tokens/cost.
                self.spawn_context(id);
            }
        }
    }

    // ── input ──────────────────────────────────────────────────────────────────

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Alt or Shift + Enter inserts a newline in the composer (terminals vary on which
        // they can report; accept either). Plain Enter still sends.
        let newline = key.modifiers.contains(KeyModifiers::ALT)
            || key.modifiers.contains(KeyModifiers::SHIFT);

        // Global quit.
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.should_quit = true;
            return;
        }

        // Jump to the main chat from anywhere. Ctrl+M needs keyboard-enhancement support to
        // be distinguishable from Enter (see `run`); F6 is the always-works fallback.
        if matches!(key.code, KeyCode::F(6)) || (ctrl && matches!(key.code, KeyCode::Char('m'))) {
            self.goto_main_chat();
            return;
        }

        // Popups capture keys.
        if self.popup != Popup::None {
            self.handle_popup_key(key.code, ctrl);
            return;
        }

        // Global, view-independent keys.
        match key.code {
            KeyCode::F(1) => {
                self.popup = Popup::Help;
                return;
            }
            KeyCode::F(2) => {
                self.popup = Popup::Models;
                self.sync_model_row();
                if self.models.is_empty() {
                    self.spawn_models();
                }
                return;
            }
            KeyCode::F(3) => {
                self.toggle_view();
                return;
            }
            KeyCode::F(4) => {
                self.open_sessions_popup();
                return;
            }
            KeyCode::F(5) => {
                self.open_agents_popup();
                return;
            }
            KeyCode::F(7) => {
                self.open_security_popup();
                return;
            }
            KeyCode::F(8) => {
                self.open_integrations_popup();
                return;
            }
            KeyCode::F(9) => {
                self.open_memory_popup();
                return;
            }
            _ => {}
        }

        match self.view {
            View::Chat => self.handle_chat_key(key.code, ctrl, newline),
            View::Inbox => self.handle_inbox_key(key.code),
            View::Transcript => self.handle_transcript_key(key.code),
        }
    }

    /// Keys in the full-view sub-agent transcript: scroll, or back to the agents drawer.
    fn handle_transcript_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                // Back to the chat with the agents drawer reopened (where it was launched).
                self.view = View::Chat;
                self.popup = Popup::Agents;
            }
            KeyCode::Up => self.transcript_scroll = self.transcript_scroll.saturating_add(2),
            KeyCode::Down => self.transcript_scroll = self.transcript_scroll.saturating_sub(2),
            KeyCode::PageUp => self.transcript_scroll = self.transcript_scroll.saturating_add(10),
            KeyCode::PageDown => self.transcript_scroll = self.transcript_scroll.saturating_sub(10),
            _ => {}
        }
    }

    fn handle_chat_key(&mut self, code: KeyCode, ctrl: bool, newline: bool) {
        match code {
            // Tab completes a slash command (the only Tab affordance now that the
            // session list lives in a popup, not a focusable pane).
            KeyCode::Tab if self.input.starts_with('/') => {
                self.complete_command();
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::Char('t') if ctrl => {
                self.tools_enabled = !self.tools_enabled;
                self.status = t(Msg::ToolsState(self.tools_enabled));
            }
            KeyCode::Char('n') if ctrl => self.new_chat(),
            KeyCode::Char('o') if ctrl => {
                if self.attachments.len() >= MAX_ATTACHMENTS {
                    self.status = t(Msg::AttachTooMany);
                } else {
                    self.attach_input.clear();
                    self.popup = Popup::Attach;
                }
            }
            KeyCode::Char('x') if ctrl => {
                if self.streaming {
                    self.cancel_run();
                }
            }
            _ => self.handle_composer_key(code, ctrl, newline),
        }
    }

    /// Jump to the pinned main chat from anywhere (closes popups, leaves the inbox).
    fn goto_main_chat(&mut self) {
        self.popup = Popup::None;
        self.view = View::Chat;
        match self.chats.iter().find(|c| c.is_main).map(|c| c.id.clone()) {
            Some(id) => {
                if self.current_chat.as_deref() != Some(id.as_str()) {
                    self.open_chat(id);
                }
                self.status = t(Msg::MainChatOpened);
            }
            // Not in the loaded list yet — refresh and open it once it arrives.
            None => {
                self.pending_main = true;
                self.spawn_chats();
            }
        }
    }

    /// Open the security-mode picker, syncing the highlighted row to the current selection.
    fn open_security_popup(&mut self) {
        self.sel_security_row = match self.security_mode.as_deref() {
            Some(m) => SECURITY_MODES
                .iter()
                .position(|x| *x == m)
                .map_or(0, |i| i + 1),
            None => 0,
        };
        self.popup = Popup::Security;
    }

    /// Set the per-run security mode (None = inherit the chat/user default) + status line.
    fn set_security(&mut self, mode: Option<String>) {
        self.security_mode = mode;
        self.status = match &self.security_mode {
            Some(m) => t(Msg::SecuritySet(m)),
            None => t(Msg::SecurityDefaultStatus),
        };
    }

    /// `/rename <title>` — rename the open chat (PATCH), then refresh the sidebar list.
    fn rename_current_chat(&mut self, title: String) {
        let Some(chat_id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        self.status = t(Msg::Renamed(&title));
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.rename_chat(&chat_id, &title).await {
                Ok(()) => match c.list_chats().await {
                    Ok(chats) => {
                        let _ = tx.send(AppMsg::Chats(chats));
                    }
                    Err(e) => {
                        let _ = tx.send(AppMsg::OpError(Op::Chats, format!("{e:#}")));
                    }
                },
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::CreateChat, format!("{e:#}")));
                }
            }
        });
    }

    /// Indices into `self.chats` matching the current session search (all when blank).
    pub fn session_results(&self) -> Vec<usize> {
        filter_sessions(&self.chats, &self.session_query)
    }

    /// The open chat's mode ("standard" | "coding" | a custom mode); "standard" if none open.
    pub fn chat_mode(&self) -> &str {
        self.current_chat
            .as_ref()
            .and_then(|id| self.chats.iter().find(|c| &c.id == id))
            .map(|c| c.mode.as_str())
            .unwrap_or("standard")
    }

    /// Whether the open chat is in `mode` (e.g. `is_mode("coding")` for the workspace shell).
    pub fn is_mode(&self, mode: &str) -> bool {
        self.chat_mode() == mode
    }

    /// Sub-agents / background tasks tracked for the open chat (spawn order; newest last).
    pub fn current_subagents(&self) -> &[Subagent] {
        self.current_chat
            .as_ref()
            .and_then(|id| self.subagents.get(id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// How many of the current chat's sub-agents are still running (for the live chip).
    pub fn running_subagents(&self) -> usize {
        self.current_subagents()
            .iter()
            .filter(|s| s.status == "running")
            .count()
    }

    /// Indices into `current_subagents` matching the agents-drawer filter (all when blank).
    pub fn agent_results(&self) -> Vec<usize> {
        let q = self.agent_query.trim().to_lowercase();
        self.current_subagents()
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                q.is_empty()
                    || a.label.to_lowercase().contains(&q)
                    || a.kind.to_lowercase().contains(&q)
                    || a.status.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Open the transcript of the highlighted sub-agent (Enter in the agents drawer).
    fn open_subagent_transcript(&mut self) {
        let Some(&idx) = self.agent_results().get(self.sel_agent) else {
            return;
        };
        let Some(sa) = self.current_subagents().get(idx) else {
            return;
        };
        // A running sub-agent has no persisted transcript yet (the endpoint 404s).
        if sa.status == "running" {
            self.status = t(Msg::TranscriptRunning);
            return;
        }
        let Some(chat_id) = self.current_chat.clone() else {
            return;
        };
        let run_id = sa.run_id.clone();
        let title = if sa.label.is_empty() {
            t(Msg::AgentKind(&sa.kind))
        } else {
            sa.label.clone()
        };
        self.status = t(Msg::TranscriptLoading);
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.get_run_transcript(&chat_id, &run_id).await {
                Ok(parts) => {
                    let msgs = parts.into_iter().fold(Vec::new(), fold_transcript_part);
                    let _ = tx.send(AppMsg::Transcript { title, msgs });
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Transcript, format!("{e:#}")));
                }
            }
        });
    }

    /// Open the agents drawer for the current chat: clear the filter and (re)load the
    /// persisted runs so completed/historical sub-agents show, not just this session's.
    fn open_agents_popup(&mut self) {
        self.sel_agent = 0;
        self.agent_query.clear();
        self.popup = Popup::Agents;
        if let Some(id) = self.current_chat.clone() {
            self.spawn_agent_runs(id);
        }
    }

    /// Fetch a chat's persisted sub-agent runs (running + completed/failed) off-thread.
    fn spawn_agent_runs(&self, chat_id: String) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            match c.list_agent_runs(&chat_id).await {
                Ok(items) => {
                    let _ = tx.send(AppMsg::AgentRuns { chat_id, items });
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Agents, format!("{e:#}")));
                }
            }
        });
    }

    /// Fetch the chat's context-fill + cumulative usage off-thread (status-bar telemetry).
    /// Best-effort: telemetry is non-essential, so a failure just keeps the last value.
    fn spawn_context(&self, chat_id: String) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Ok(ctx) = c.get_chat_context(&chat_id).await {
                let _ = tx.send(AppMsg::Context { chat_id, ctx });
            }
        });
    }

    /// Elapsed wall-clock of the active run (for the telemetry timer), or None when idle.
    pub fn run_elapsed(&self) -> Option<std::time::Duration> {
        self.run_started.map(|t| t.elapsed())
    }

    /// Insert or update a sub-agent by `run_id`, then keep the chat's list newest-first.
    /// A live frame missing `started_at` keeps the timestamp from the persisted row.
    fn upsert_subagent(&mut self, chat_id: String, sa: Subagent) {
        let list = self.subagents.entry(chat_id).or_default();
        match list.iter_mut().find(|s| s.run_id == sa.run_id) {
            Some(existing) => {
                let started = sa
                    .started_at
                    .clone()
                    .or_else(|| existing.started_at.clone());
                *existing = sa;
                existing.started_at = started;
            }
            None => list.push(sa),
        }
        list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    }

    /// Open the sessions picker popup: clear the filter and pre-select the current chat.
    fn open_sessions_popup(&mut self) {
        self.session_query.clear();
        // With an empty query the results mirror `self.chats` 1:1, so the row == the index.
        self.sel_chat = self
            .current_chat
            .as_ref()
            .and_then(|cur| self.chats.iter().position(|c| &c.id == cur))
            .unwrap_or(0);
        self.popup = Popup::Sessions;
        if self.chats.is_empty() {
            self.spawn_chats();
        }
    }

    // ── integrations / per-chat tool selection ─────────────────────────────────

    /// Open the integrations picker (F8 / `/integrations`): clear the filter and load this
    /// chat's tool catalog + current deny-list off-thread. No-op without an open chat.
    fn open_integrations_popup(&mut self) {
        let Some(id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        self.int_filter.reset();
        self.popup = Popup::Integrations;
        self.spawn_integrations(id);
    }

    /// Fetch the chat's integrations catalog and current `disabled_tools` deny-list together,
    /// so the picker opens already reflecting the saved per-tool selection.
    fn spawn_integrations(&self, chat_id: String) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let groups = match c.list_integrations_catalog(&chat_id).await {
                Ok(g) => g,
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Integrations, format!("{e:#}")));
                    return;
                }
            };
            // Best-effort: an empty deny-list (all tools on) is the safe default if this fails.
            let disabled = c.get_disabled_tools(&chat_id).await.unwrap_or_default();
            let _ = tx.send(AppMsg::Integrations { groups, disabled });
        });
    }

    /// Display label for an integration group: localized for the built-in/web buckets (whose
    /// server-side label is empty), else the integration/device/MCP instance label.
    pub fn group_label(&self, g: &api::IntegrationGroup) -> String {
        match g.kind.as_str() {
            "builtin" => t(Msg::IntGroupBuiltin),
            "web" => t(Msg::IntGroupWeb),
            _ if !g.label.is_empty() => g.label.clone(),
            _ => g.key.clone(),
        }
    }

    /// Whether a tool is enabled for the chat (i.e. NOT on the deny-list).
    pub fn tool_enabled(&self, name: &str) -> bool {
        !self.disabled_tools.contains(name)
    }

    /// The integration rows matching the current filter, in display order (a group header then
    /// its matching tools). A group whose label matches shows all its tools; otherwise only the
    /// matching tools, and the header only when at least one tool (or the label) matches. Mirrors
    /// the web picker's `filteredGroups` + `toolsOf`.
    pub fn integration_rows(&self) -> Vec<IntRow> {
        let mut rows = Vec::new();
        for (gi, g) in self.integrations.iter().enumerate() {
            let label_match = self.int_filter.matches(&self.group_label(g));
            let tool_idxs: Vec<usize> = g
                .tools
                .iter()
                .enumerate()
                .filter(|(_, tn)| label_match || self.int_filter.matches(tn))
                .map(|(i, _)| i)
                .collect();
            if tool_idxs.is_empty() && !label_match {
                continue;
            }
            rows.push(IntRow::Header(gi));
            for ti in tool_idxs {
                rows.push(IntRow::Tool(gi, ti));
            }
        }
        rows
    }

    /// Toggle the highlighted row: a tool flips its own deny-state; a header flips the whole
    /// group (all-on → disable all, else enable all — the web's "all/none" affordance). Persists
    /// the new deny-list immediately, like the web composer.
    fn toggle_int_row(&mut self) {
        let rows = self.integration_rows();
        let Some(&row) = rows.get(self.int_filter.cursor) else {
            return;
        };
        match row {
            IntRow::Tool(gi, ti) => {
                if let Some(name) = self
                    .integrations
                    .get(gi)
                    .and_then(|g| g.tools.get(ti))
                    .cloned()
                {
                    if !self.disabled_tools.remove(&name) {
                        self.disabled_tools.insert(name);
                    }
                }
            }
            IntRow::Header(gi) => {
                let tools = self
                    .integrations
                    .get(gi)
                    .map(|g| g.tools.clone())
                    .unwrap_or_default();
                let all_on = tools.iter().all(|tn| !self.disabled_tools.contains(tn));
                for tn in tools {
                    if all_on {
                        self.disabled_tools.insert(tn);
                    } else {
                        self.disabled_tools.remove(&tn);
                    }
                }
            }
        }
        self.persist_int_selection();
    }

    /// Persist the chat's `disabled_tools` deny-list to its run_config (best-effort).
    fn persist_int_selection(&mut self) {
        let Some(chat_id) = self.current_chat.clone() else {
            return;
        };
        let disabled: Vec<String> = self.disabled_tools.iter().cloned().collect();
        self.status = t(Msg::IntegrationsSaved(disabled.len()));
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Err(e) = c.set_disabled_tools(&chat_id, &disabled).await {
                let _ = tx.send(AppMsg::OpError(Op::Integrations, format!("{e:#}")));
            }
        });
    }

    // ── memory access (per-chat world-memory scoping) ──────────────────────────

    /// Open the memory-access picker (F9 / `/memory`): load this chat's current policy off-thread.
    fn open_memory_popup(&mut self) {
        let Some(id) = self.current_chat.clone() else {
            self.status = t(Msg::NoChatSelected);
            return;
        };
        self.mem_sel = 0;
        self.popup = Popup::Memory;
        self.spawn_memory(id);
    }

    fn spawn_memory(&self, chat_id: String) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let access = c.get_memory_access(&chat_id).await.unwrap_or(None);
            let _ = tx.send(AppMsg::Memory { chat_id, access });
        });
    }

    /// The selected mode row: 0 = default (inherit) · 1 = full · 2 = none · 3 = scoped.
    pub fn mem_mode_index(&self) -> usize {
        match self.mem_access.as_ref().map(|m| m.mode.as_str()) {
            Some("full") => 1,
            Some("none") => 2,
            Some("scoped") => 3,
            _ => 0,
        }
    }

    fn mem_is_scoped(&self) -> bool {
        self.mem_mode_index() == 3
    }

    /// Whether a domain is included (a null axis = unrestricted → every box checked).
    pub fn mem_domain_on(&self, i: usize) -> bool {
        match self.mem_access.as_ref().and_then(|m| m.domains.as_ref()) {
            None => true,
            Some(list) => list.iter().any(|d| d == MEM_DOMAINS[i]),
        }
    }

    /// Whether a source is included (a null axis = unrestricted → every box checked).
    pub fn mem_source_on(&self, i: usize) -> bool {
        match self.mem_access.as_ref().and_then(|m| m.sources.as_ref()) {
            None => true,
            Some(list) => list.iter().any(|s| s == MEM_SOURCES[i]),
        }
    }

    /// The visible rows: the four modes, plus the domain + source checkboxes when scoped.
    pub fn mem_rows(&self) -> Vec<MemRow> {
        let mut rows = vec![
            MemRow::Mode(0),
            MemRow::Mode(1),
            MemRow::Mode(2),
            MemRow::Mode(3),
        ];
        if self.mem_is_scoped() {
            rows.extend((0..MEM_DOMAINS.len()).map(MemRow::Domain));
            rows.extend((0..MEM_SOURCES.len()).map(MemRow::Source));
        }
        rows
    }

    /// Enter/Space on the highlighted row: pick a mode (radio) or toggle a domain/source box.
    fn mem_activate(&mut self) {
        let rows = self.mem_rows();
        let Some(&row) = rows.get(self.mem_sel) else {
            return;
        };
        match row {
            MemRow::Mode(0) => self.mem_access = None,
            MemRow::Mode(1) => self.mem_access = Some(mem_policy("full", None, None)),
            MemRow::Mode(2) => self.mem_access = Some(mem_policy("none", None, None)),
            MemRow::Mode(3) => {
                // Enter scoped with everything checked (explicit full lists), like the web.
                let all_d = MEM_DOMAINS.iter().map(|s| s.to_string()).collect();
                let all_s = MEM_SOURCES.iter().map(|s| s.to_string()).collect();
                self.mem_access = Some(mem_policy("scoped", Some(all_d), Some(all_s)));
            }
            MemRow::Mode(_) => {}
            MemRow::Domain(i) => self.mem_toggle_axis(true, i),
            MemRow::Source(i) => self.mem_toggle_axis(false, i),
        }
        self.persist_memory();
    }

    /// Toggle a scoped domain (`domain=true`) or source box by index.
    fn mem_toggle_axis(&mut self, domain: bool, i: usize) {
        let Some(ma) = self.mem_access.as_mut() else {
            return;
        };
        if ma.mode != "scoped" {
            return;
        }
        let (key, all): (String, Vec<String>) = if domain {
            (
                MEM_DOMAINS[i].to_string(),
                MEM_DOMAINS.iter().map(|s| s.to_string()).collect(),
            )
        } else {
            (
                MEM_SOURCES[i].to_string(),
                MEM_SOURCES.iter().map(|s| s.to_string()).collect(),
            )
        };
        let list = if domain {
            ma.domains.get_or_insert(all)
        } else {
            ma.sources.get_or_insert(all)
        };
        match list.iter().position(|x| x == &key) {
            Some(pos) => {
                list.remove(pos);
            }
            None => list.push(key),
        }
    }

    /// Persist the chat's memory policy. Default (None) inherits the user policy; the PATCH
    /// endpoint has no clear path, so default is local-only (matches the web composer).
    fn persist_memory(&mut self) {
        let Some(chat_id) = self.current_chat.clone() else {
            return;
        };
        let Some(ma) = self.mem_access.clone() else {
            self.status = t(Msg::MemoryDefaultStatus);
            return;
        };
        self.status = t(Msg::MemorySet(&ma.mode));
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let r = c
                .set_memory_access(
                    &chat_id,
                    &ma.mode,
                    ma.domains.as_deref(),
                    ma.sources.as_deref(),
                )
                .await;
            if let Err(e) = r {
                let _ = tx.send(AppMsg::OpError(Op::Memory, format!("{e:#}")));
            }
        });
    }

    /// Reset the composer buffer (text, cursor, history navigation state).
    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.hist_idx = None;
        self.hist_draft.clear();
    }

    /// Replace the composer buffer and park the cursor at the end (history recall, completion).
    fn set_input(&mut self, text: String) {
        self.cursor = text.len();
        self.input = text;
    }

    /// Record a just-sent prompt for ↑/↓ recall (skip blanks + consecutive duplicates).
    fn remember_prompt(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.input_history.last().map(String::as_str) == Some(text) {
            return;
        }
        self.input_history.push(text.to_string());
    }

    /// Recall the previous prompt (↑ on the first composer line). Saves the live draft first.
    fn history_prev(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            None => {
                self.hist_draft = self.input.clone();
                self.input_history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(idx);
        self.set_input(self.input_history[idx].clone());
    }

    /// Step forward through recalled prompts (↓); past the newest restores the live draft.
    fn history_next(&mut self) {
        let Some(idx) = self.hist_idx else {
            return;
        };
        if idx + 1 < self.input_history.len() {
            self.hist_idx = Some(idx + 1);
            self.set_input(self.input_history[idx + 1].clone());
        } else {
            self.hist_idx = None;
            let draft = std::mem::take(&mut self.hist_draft);
            self.set_input(draft);
        }
    }

    fn handle_composer_key(&mut self, code: KeyCode, ctrl: bool, newline: bool) {
        let cmd_mode = self.input.starts_with('/');
        match code {
            // Alt/Shift+Enter inserts a newline; plain Enter sends / runs.
            KeyCode::Enter if newline => {
                composer::insert_char(&mut self.input, &mut self.cursor, '\n');
                self.hist_idx = None;
            }
            KeyCode::Enter => {
                if cmd_mode {
                    let input = self.input.clone();
                    self.run_command(&input);
                } else if self.input.starts_with('!') {
                    self.run_shell();
                } else {
                    self.submit();
                }
            }
            // In command mode, ↑/↓ move the command-menu selection (not the cursor/history).
            KeyCode::Up if cmd_mode => self.cmd_sel = self.cmd_sel.saturating_sub(1),
            KeyCode::Down if cmd_mode => {
                if self.cmd_sel + 1 < self.palette().len() {
                    self.cmd_sel += 1;
                }
            }
            // ↑/↓ move between lines in a multiline draft; at the top/bottom edge they recall
            // prompt history (the common shell affordance).
            KeyCode::Up => {
                if !composer::move_up(&self.input, &mut self.cursor) {
                    self.history_prev();
                }
            }
            KeyCode::Down => {
                if !composer::move_down(&self.input, &mut self.cursor) {
                    self.history_next();
                }
            }
            KeyCode::Left => composer::left(&self.input, &mut self.cursor),
            KeyCode::Right => composer::right(&self.input, &mut self.cursor),
            KeyCode::Home => composer::home(&self.input, &mut self.cursor),
            KeyCode::End => composer::end(&self.input, &mut self.cursor),
            KeyCode::Delete => composer::delete(&mut self.input, &mut self.cursor),
            KeyCode::Char('w') if ctrl => {
                composer::delete_word_back(&mut self.input, &mut self.cursor);
                self.cmd_sel = 0;
            }
            KeyCode::Char('u') if ctrl => {
                composer::kill_line_back(&mut self.input, &mut self.cursor);
                self.cmd_sel = 0;
            }
            KeyCode::Backspace => {
                composer::backspace(&mut self.input, &mut self.cursor);
                self.hist_idx = None;
                self.cmd_sel = 0;
            }
            KeyCode::Char(c) => {
                composer::insert_char(&mut self.input, &mut self.cursor, c);
                self.hist_idx = None;
                self.cmd_sel = 0;
            }
            _ => {}
        }
    }

    /// Complete the composer's `/…` to the highlighted command name (keeping any args).
    fn complete_command(&mut self) {
        let matches = self.palette();
        if matches.is_empty() {
            return;
        }
        let name = matches[self.cmd_sel.min(matches.len() - 1)].name.clone();
        let next = match self.input.split_once(char::is_whitespace) {
            Some((_, rest)) => format!("{name} {}", rest.trim_start()),
            None => format!("{name} "),
        };
        self.set_input(next);
        self.cmd_sel = 0;
    }

    fn handle_popup_key(&mut self, code: KeyCode, ctrl: bool) {
        match self.popup {
            Popup::Help => {
                if matches!(code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Enter) {
                    self.popup = Popup::None;
                }
            }
            Popup::Models => match code {
                KeyCode::Esc | KeyCode::F(2) => self.popup = Popup::None,
                KeyCode::Up => self.sel_model_row = self.sel_model_row.saturating_sub(1),
                KeyCode::Down => {
                    // Rows: 0 = "(Standard)", then one per model — so the last index is len().
                    if self.sel_model_row < self.models.len() {
                        self.sel_model_row += 1;
                    }
                }
                KeyCode::Enter => {
                    // Row 0 = "(Standard)" → inherit the chat default; rest = explicit model.
                    self.model = if self.sel_model_row == 0 {
                        None
                    } else {
                        self.models
                            .get(self.sel_model_row - 1)
                            .map(|m| m.id.clone())
                    };
                    self.status = match &self.model {
                        Some(m) => t(Msg::ModelSet(m)),
                        None => t(Msg::ModelDefaultStatus),
                    };
                    self.popup = Popup::None;
                }
                _ => {}
            },
            Popup::Security => match code {
                KeyCode::Esc | KeyCode::F(7) => self.popup = Popup::None,
                KeyCode::Up => self.sel_security_row = self.sel_security_row.saturating_sub(1),
                KeyCode::Down => {
                    if self.sel_security_row < SECURITY_MODES.len() {
                        self.sel_security_row += 1;
                    }
                }
                KeyCode::Enter => {
                    // Row 0 = "(default)" → inherit; rest = an explicit mode.
                    let mode = if self.sel_security_row == 0 {
                        None
                    } else {
                        SECURITY_MODES
                            .get(self.sel_security_row - 1)
                            .map(|m| m.to_string())
                    };
                    self.set_security(mode);
                    self.popup = Popup::None;
                }
                _ => {}
            },
            Popup::Sessions => match code {
                KeyCode::Esc | KeyCode::F(4) => self.popup = Popup::None,
                KeyCode::Up => self.sel_chat = self.sel_chat.saturating_sub(1),
                KeyCode::Down => {
                    if self.sel_chat + 1 < self.session_results().len() {
                        self.sel_chat += 1;
                    }
                }
                // Ctrl+N makes a new chat (plain letters feed the search box).
                KeyCode::Char('n') if ctrl => {
                    self.popup = Popup::None;
                    self.new_chat();
                }
                KeyCode::Enter => {
                    if let Some(&idx) = self.session_results().get(self.sel_chat) {
                        let id = self.chats[idx].id.clone();
                        self.open_chat(id);
                    }
                    self.popup = Popup::None;
                }
                KeyCode::Backspace => {
                    self.session_query.pop();
                    self.sel_chat = 0;
                }
                KeyCode::Char(c) => {
                    self.session_query.push(c);
                    self.sel_chat = 0;
                }
                _ => {}
            },
            Popup::Agents => match code {
                KeyCode::Esc | KeyCode::F(5) => self.popup = Popup::None,
                KeyCode::Up => self.sel_agent = self.sel_agent.saturating_sub(1),
                KeyCode::Down => {
                    if self.sel_agent + 1 < self.agent_results().len() {
                        self.sel_agent += 1;
                    }
                }
                // Enter opens the highlighted sub-agent's transcript.
                KeyCode::Enter => self.open_subagent_transcript(),
                KeyCode::Backspace => {
                    self.agent_query.pop();
                    self.sel_agent = 0;
                }
                KeyCode::Char(c) => {
                    self.agent_query.push(c);
                    self.sel_agent = 0;
                }
                _ => {}
            },
            Popup::Approval => {
                let len = 3;
                match code {
                    // Esc rejects the current request (a no-decision close would hang the run).
                    KeyCode::Esc => self.decide_approval(false, false),
                    KeyCode::Up => {
                        if let Some(a) = self.approvals.front_mut() {
                            a.sel = (a.sel + len - 1) % len;
                        }
                    }
                    KeyCode::Down => {
                        if let Some(a) = self.approvals.front_mut() {
                            a.sel = (a.sel + 1) % len;
                        }
                    }
                    KeyCode::Enter => {
                        let sel = self.approvals.front().map(|a| a.sel).unwrap_or(2);
                        match sel {
                            0 => self.decide_approval(true, false),
                            1 => self.decide_approval(true, true),
                            _ => self.decide_approval(false, false),
                        }
                    }
                    _ => {}
                }
            }
            Popup::Question => match code {
                KeyCode::Esc => {
                    self.question = None;
                    self.popup = Popup::None;
                }
                // ←/→ switch between sub-questions (multi-question cards).
                KeyCode::Left => {
                    if let Some(q) = &mut self.question {
                        q.q_idx = q.q_idx.saturating_sub(1);
                        q.opt_idx = 0;
                    }
                }
                KeyCode::Right => {
                    if let Some(q) = &mut self.question {
                        if q.q_idx + 1 < q.subs.len() {
                            q.q_idx += 1;
                            q.opt_idx = 0;
                        }
                    }
                }
                KeyCode::Up => {
                    if let Some(q) = &mut self.question {
                        q.opt_idx = q.opt_idx.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if let Some(q) = &mut self.question {
                        if let Some(sub) = q.subs.get(q.q_idx) {
                            if q.opt_idx + 1 < sub.options.len() {
                                q.opt_idx += 1;
                            }
                        }
                    }
                }
                // Space toggles (multi-select) or picks (single-select) the highlighted option.
                KeyCode::Char(' ') => {
                    if let Some(q) = &mut self.question {
                        let opt = q.opt_idx;
                        if let Some(sub) = q.subs.get_mut(q.q_idx) {
                            sub.toggle(opt);
                        }
                    }
                }
                KeyCode::Backspace => {
                    if let Some(q) = &mut self.question {
                        if let Some(sub) = q.subs.get_mut(q.q_idx) {
                            sub.custom.pop();
                        }
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(q) = &mut self.question {
                        if let Some(sub) = q.subs.get_mut(q.q_idx) {
                            if sub.allow_custom {
                                sub.custom.push(c);
                            }
                        }
                    }
                }
                // Enter advances to the next sub-question, or submits on the last one.
                KeyCode::Enter => {
                    let advance = self
                        .question
                        .as_ref()
                        .map(|q| q.q_idx + 1 < q.subs.len())
                        .unwrap_or(false);
                    if advance {
                        if let Some(q) = &mut self.question {
                            q.q_idx += 1;
                            q.opt_idx = 0;
                        }
                    } else {
                        self.answer_question();
                    }
                }
                _ => {}
            },
            Popup::Attach => match code {
                KeyCode::Esc => self.popup = Popup::None,
                KeyCode::Backspace => {
                    self.attach_input.pop();
                }
                KeyCode::Char(c) => self.attach_input.push(c),
                KeyCode::Enter => {
                    let path = self.attach_input.trim().to_string();
                    self.popup = Popup::None;
                    if !path.is_empty() {
                        self.spawn_attach(path);
                    }
                }
                _ => {}
            },
            Popup::Integrations => match code {
                // Enter/Esc/F8 all close; toggles already persisted on each change.
                KeyCode::Esc | KeyCode::F(8) | KeyCode::Enter => self.popup = Popup::None,
                KeyCode::Up => self.int_filter.up(),
                KeyCode::Down => {
                    let len = self.integration_rows().len();
                    self.int_filter.down(len);
                }
                // Space toggles the highlighted tool/group; other chars filter the list.
                KeyCode::Char(' ') => self.toggle_int_row(),
                KeyCode::Backspace => self.int_filter.backspace(),
                KeyCode::Char(c) => self.int_filter.push(c),
                _ => {}
            },
            Popup::Memory => match code {
                KeyCode::Esc | KeyCode::F(9) => self.popup = Popup::None,
                KeyCode::Up => self.mem_sel = self.mem_sel.saturating_sub(1),
                KeyCode::Down => {
                    if self.mem_sel + 1 < self.mem_rows().len() {
                        self.mem_sel += 1;
                    }
                }
                // Enter / Space pick the highlighted mode or toggle a domain/source box.
                KeyCode::Enter | KeyCode::Char(' ') => self.mem_activate(),
                _ => {}
            },
            Popup::None => {}
        }
    }

    /// Read a file off-thread, base64-encode it, and stage it as an attachment.
    fn spawn_attach(&self, path: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    use base64::Engine as _;
                    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let filename = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone());
                    let media_type = guess_media_type(&filename);
                    let _ = tx.send(AppMsg::Attached(Attachment {
                        filename,
                        media_type,
                        data,
                    }));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::OpError(Op::Attach, format!("{e}")));
                }
            }
        });
    }

    fn sync_model_row(&mut self) {
        self.sel_model_row = match &self.model {
            None => 0,
            Some(id) => self
                .models
                .iter()
                .position(|m| &m.id == id)
                .map(|i| i + 1)
                .unwrap_or(0),
        };
    }

    fn tick(&mut self) {
        // Free-running: drives the working spinner/phrases AND the session picker's
        // "active" flash, which must animate even when this client isn't streaming.
        self.spinner = self.spinner.wrapping_add(1);
        // Time the active run for the telemetry timer: start on the first tick while streaming,
        // clear when it settles (a single chokepoint, so every run-start path is covered).
        if self.streaming {
            self.run_started.get_or_insert_with(std::time::Instant::now);
        } else {
            self.run_started = None;
        }
    }
}

fn tool_mut<'a>(tools: &'a mut [UiTool], id: &str) -> Option<&'a mut UiTool> {
    // Match by id; fall back to the last tool (some providers omit ids on arg deltas).
    if let Some(pos) = tools.iter().position(|t| t.id == id) {
        return tools.get_mut(pos);
    }
    tools.last_mut()
}

/// Fold one transcript part into the running message list: a `user` part starts a user
/// message; `text`/`thinking`/`tool` parts accumulate into the trailing assistant message.
fn fold_transcript_part(mut acc: Vec<UiMessage>, p: api::MsgPart) -> Vec<UiMessage> {
    if p.kind == "user" {
        acc.push(UiMessage {
            role: "user".into(),
            text: p.text.unwrap_or_default(),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        });
        return acc;
    }
    // Ensure a trailing assistant message to accumulate into.
    if !matches!(acc.last(), Some(m) if m.role == "assistant") {
        acc.push(UiMessage {
            role: "assistant".into(),
            text: String::new(),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        });
    }
    let m = acc.last_mut().expect("just ensured");
    match p.kind.as_str() {
        "text" => {
            if let Some(t) = p.text {
                m.text.push_str(&t);
            }
        }
        "thinking" => {
            if let Some(t) = p.text {
                m.thinking.push_str(&t);
            }
        }
        "tool" => m.tools.push(UiTool {
            id: String::new(),
            name: p.name.unwrap_or_else(|| "tool".into()),
            args: p.args.unwrap_or_default(),
            result: p.result,
        }),
        _ => {}
    }
    acc
}

/// Fold a persisted history message into the render model (display_text + ordered parts).
fn convert_message(m: api::Message) -> UiMessage {
    let mut text = m.display_text.clone().unwrap_or_default();
    let mut thinking = String::new();
    let mut tools = Vec::new();
    for p in &m.parts {
        match p.kind.as_str() {
            "text" if text.is_empty() => {
                if let Some(t) = &p.text {
                    text.push_str(t);
                }
            }
            "thinking" => {
                if let Some(t) = &p.text {
                    thinking.push_str(t);
                }
            }
            "tool" => tools.push(UiTool {
                id: String::new(),
                name: p.name.clone().unwrap_or_else(|| "tool".into()),
                args: p.args.clone().unwrap_or_default(),
                result: p.result.clone(),
            }),
            _ => {}
        }
    }
    UiMessage {
        role: m.role,
        text,
        thinking,
        tools,
        usage: m.usage,
        pending: false,
    }
}

/// Set up the terminal, run the event loop, and restore the terminal on exit.
pub async fn run(client: Arc<ApiClient>) -> Result<Option<String>> {
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };

    use crossterm::event::{
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Disambiguate escape codes so Ctrl+M is reported distinctly from Enter (Ctrl+M jumps
    // to the main chat). We push unconditionally rather than gating on
    // `supports_keyboard_enhancement()` — that probe false-negatives on several terminals
    // (tmux, some libvte builds); terminals that genuinely don't support it ignore the
    // escape and simply keep Ctrl+M == Enter.
    let pushed_kbd = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, client).await;

    if pushed_kbd {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    client: Arc<ApiClient>,
) -> Result<Option<String>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AppMsg>();
    let mut app = App::new(client.clone(), tx.clone());
    app.ws = Some(ws::spawn(client, tx.clone()));
    app.bootstrap();

    let mut reader = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;
        if app.should_quit {
            break;
        }
        tokio::select! {
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => app.handle_key(key),
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            Some(msg) = rx.recv() => app.handle_msg(msg),
            _ = tick.tick() => app.tick(),
        }
    }
    Ok(app.computer_service_request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_label_maps_index_to_status_key() {
        assert_eq!(filter_label(0), "all");
        assert_eq!(filter_label(1), "new");
        assert_eq!(filter_label(2), "needs_reply");
        // Out-of-range falls back to "all".
        assert_eq!(filter_label(99), "all");
    }

    #[test]
    fn command_matches_by_prefix() {
        assert_eq!(command_matches("/t"), vec!["/think", "/tools"]);
        assert_eq!(command_matches("/btw fix this"), vec!["/btw"]);
        assert!(command_matches("/zzz").is_empty());
        // Bare slash offers every built-in command.
        assert_eq!(command_matches("/").len(), COMMANDS.len());
    }

    #[test]
    fn expands_custom_command_templates() {
        assert_eq!(
            expand_template("Fix $ARGUMENTS now", "the bug"),
            "Fix the bug now"
        );
        assert_eq!(
            expand_template("Review {args}", "main.rs"),
            "Review main.rs"
        );
        // No placeholder → args appended; empty args → template verbatim.
        assert_eq!(expand_template("Summarize", "this"), "Summarize this");
        assert_eq!(expand_template("Summarize", ""), "Summarize");
    }

    #[test]
    fn custom_command_mode_visibility() {
        assert!(custom_visible(None, false) && custom_visible(None, true));
        assert!(custom_visible(Some("coding"), true) && !custom_visible(Some("coding"), false));
        assert!(custom_visible(Some("standard"), false) && !custom_visible(Some("standard"), true));
    }

    fn chat(title: &str, mode: &str) -> api::Chat {
        api::Chat {
            id: title.into(),
            title: title.into(),
            mode: mode.into(),
            is_main: false,
            active: false,
            active_run_id: None,
            updated_at: None,
        }
    }

    #[test]
    fn short_time_formats_iso() {
        assert_eq!(short_time("2026-06-14T21:05:09.123Z"), "06-14 21:05");
        assert_eq!(short_time("2026-12-01T08:00:00+00:00"), "12-01 08:00");
        assert_eq!(short_time("not-a-date"), "");
    }

    #[test]
    fn parses_hex_accent_colors() {
        assert_eq!(parse_hex_color("#2f5da6"), Some((0x2f, 0x5d, 0xa6)));
        assert_eq!(parse_hex_color("FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#abc"), None);
        assert_eq!(parse_hex_color("#zzzzzz"), None);
    }

    #[test]
    fn mode_icon_mirrors_web_modes() {
        assert_eq!(mode_icon("standard"), "💬");
        assert_eq!(mode_icon("coding"), "💻");
        // Unknown / custom modes get the neutral mark.
        assert_eq!(mode_icon("research"), "✦");
    }

    #[test]
    fn filter_sessions_matches_titles_case_insensitively() {
        let chats = vec![
            chat("Deploy plan", "coding"),
            chat("Grocery list", "standard"),
        ];
        // Blank query → every chat, in order.
        assert_eq!(filter_sessions(&chats, ""), vec![0, 1]);
        // Case-insensitive substring on the title.
        assert_eq!(filter_sessions(&chats, "GROCERY"), vec![1]);
        assert_eq!(filter_sessions(&chats, "plan"), vec![0]);
        assert!(filter_sessions(&chats, "zzz").is_empty());
    }

    #[test]
    fn media_type_from_extension() {
        assert_eq!(guess_media_type("photo.PNG"), "image/png");
        assert_eq!(guess_media_type("report.pdf"), "application/pdf");
        assert_eq!(guess_media_type("notes.md"), "text/markdown");
        assert_eq!(guess_media_type("archive.bin"), "application/octet-stream");
        assert_eq!(guess_media_type("noext"), "application/octet-stream");
    }
}
