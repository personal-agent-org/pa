//! HTTP data layer — talks to the SAME `/api/v1` endpoints the web SPA uses.
//!
//! Every call carries `Authorization: Bearer <jwt>` (+ optional `X-Personal-Agent-Org`).
//! On a 401 we silently refresh via the Keycloak refresh token, persist the new tokens,
//! and retry the request once — mirroring the web client's single-flight renew interceptor.

use anyhow::{Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::Config;

const ORG_HEADER: &str = "X-Personal-Agent-Org";

struct TokenState {
    access: String,
    refresh: String,
}

pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    server: String,
    client_id: String,
    // Where the refresh_token grant lives, discovered at login.
    token_endpoint: String,
    org: Option<String>,
    lang: Option<String>,
    tokens: Mutex<TokenState>,
}

// ── wire DTOs (subset of the backend schemas we render) ──────────────────────

#[allow(dead_code)] // active_org carried for completeness; org switching is a later slice
#[derive(Deserialize, Debug, Default)]
pub struct Me {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub active_org: Option<String>,
    /// Frontend appearance prefs (shared with the web app); we read `ui.accent`.
    #[serde(default)]
    pub ui: Option<UiPrefs>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct UiPrefs {
    /// User accent colour as `#rrggbb`, or '' / absent for the built-in default.
    #[serde(default)]
    pub accent: Option<String>,
}

#[allow(dead_code)] // mode/active_run_id decoded now; used when re-attaching live runs
#[derive(Deserialize, Debug, Clone)]
pub struct Chat {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// The user's pinned main chat (never archived/deleted); jumped to with Ctrl+M.
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub active_run_id: Option<String>,
    /// Last-activity timestamp (ISO 8601, UTC) — shown in the session picker.
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ComputerDevice {
    pub id: String,
}

fn default_mode() -> String {
    "standard".into()
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct MsgUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub model_name: Option<String>,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct MsgPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub args: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct Message {
    #[serde(default)]
    pub position: i64,
    pub role: String,
    #[serde(default)]
    pub display_text: Option<String>,
    #[serde(default)]
    pub usage: Option<MsgUsage>,
    #[serde(default)]
    pub parts: Vec<MsgPart>,
}

/// One of the user's skills, as the Skills view shows them.
///
/// A subset of `SkillOut`: the TUI lists and toggles, it does not edit instructions -- that is
/// what the web editor is for (personal-agent-org/personal-agent#126).
#[derive(Debug, Clone, Deserialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub pinned: bool,
    /// "active" | "stale" | … — the curator's aging verdict, shown as a hint.
    #[serde(default)]
    pub lifecycle_state: String,
    /// True for a skill adopted from the marketplace: read-only here, and it is somebody
    /// else's, so the list says so rather than offering to change it.
    #[serde(default)]
    pub adopted: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider_label: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    models: Vec<Model>,
}

// ── conversations / inbox ────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ConversationSummary {
    pub entity_id: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub unread: bool,
}

/// One persisted sub-agent run for the agents drawer (`GET /chats/{id}/agents`). Mirrors
/// the server's `AgentRunOut`; token/cost tallies are nested under `usage`.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct AgentRun {
    pub run_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub usage: Option<MsgUsage>,
    #[serde(default)]
    pub tool_calls: i64,
}

#[derive(Deserialize, Debug, Default)]
pub struct AgentRunList {
    #[serde(default)]
    pub items: Vec<AgentRun>,
}

/// A sub-agent run's persisted transcript: ordered render parts (same shape as chat history).
#[derive(Deserialize, Debug, Default)]
pub struct RunTranscript {
    #[serde(default)]
    pub parts: Vec<MsgPart>,
}

/// A user-authored slash command (`GET /commands`): a prompt template the composer palette
/// merges in. `mode` is "standard" | "coding" | null (all modes).
#[derive(Deserialize, Debug, Clone, Default)]
pub struct CustomCommand {
    pub name: String,
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct ConversationList {
    #[serde(default)]
    pub items: Vec<ConversationSummary>,
    #[serde(default)]
    pub counts: std::collections::HashMap<String, i64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ThreadMessage {
    #[serde(default)]
    pub direction: String, // "in" | "out"
    #[serde(default)]
    pub sender: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub by: String, // outgoing: "user" | "agent"
}

#[derive(Deserialize, Debug, Default)]
struct ConvDraft {
    #[serde(default)]
    body: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct ConversationDetail {
    pub entity_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub messages: Vec<ThreadMessage>,
    #[serde(default)]
    draft: Option<ConvDraft>,
    #[serde(default)]
    pub can_reply: bool,
}

impl ConversationDetail {
    /// The body of a pending Personal Agent draft, if any (offered as a reply starting point).
    pub fn draft_body(&self) -> Option<&str> {
        self.draft
            .as_ref()
            .map(|d| d.body.as_str())
            .filter(|b| !b.is_empty())
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct SuggestReply {
    #[serde(default)]
    pub body: String,
}

/// One integration group in the per-chat tool catalog (`GET /chats/{id}/integrations-catalog`).
/// Mirrors the web `IntegrationGroup`: a built-in / web / integration / device / mcp bucket plus
/// the individual tool names it contributes. Deselected tool names feed the run's
/// `disabled_tools` deny-list (a chat always sees the user's full enabled set; this narrows it).
#[derive(Deserialize, Debug, Clone, Default)]
pub struct IntegrationGroup {
    #[serde(default)]
    pub key: String,
    /// Instance/entry name (e.g. "projects.example.com"); empty for the built-in/web buckets,
    /// which the UI localizes instead.
    #[serde(default)]
    pub label: String,
    /// The integration type from the manifest (e.g. "OpenProject"), if any.
    #[serde(default)]
    pub sub_label: Option<String>,
    /// "builtin" | "web" | "integration" | "device" | "mcp".
    #[serde(default)]
    pub kind: String,
    /// Governance chips (e.g. a required-tier badge).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Bare tool names this group contributes (device tools keep their `dev_<id>_` prefix).
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
struct IntegrationsCatalog {
    #[serde(default)]
    groups: Vec<IntegrationGroup>,
}

/// The chat's context meter + cumulative session usage (`GET /chats/{id}/context`) — feeds the
/// status-bar telemetry: current context fill (% of the model window) and session tokens/cost.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct ChatContext {
    /// Prompt size of the latest model request = the current context fill.
    #[serde(default)]
    pub context_tokens: i64,
    /// The model's full context window (gauge denominator).
    #[serde(default)]
    pub window_tokens: i64,
    /// Auto-compaction threshold (the soft ceiling before history is compacted).
    #[serde(default)]
    pub threshold_tokens: i64,
    #[serde(default)]
    pub model_name: Option<String>,
    /// Whether the latest run's context was auto-compacted.
    #[serde(default)]
    pub compacted: bool,
    /// Cumulative consumption across every run of this chat.
    #[serde(default)]
    pub session: SessionUsage,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct SessionUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cost_usd: f64,
}

/// Per-chat memory-access policy (`run_config.memory_access`) — which world-memory the agent may
/// read. Mirrors the backend `MemoryAccessPolicy`: `mode` = full|none|scoped; a null axis list is
/// unrestricted, an explicit list narrows it (empty = fail-closed for that axis).
#[derive(Deserialize, Debug, Clone, Default)]
pub struct MemoryAccess {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub sources: Option<Vec<String>>,
}

impl ApiClient {
    pub fn new(cfg: &Config) -> Result<ApiClient> {
        // pa_oidc::tls, not reqwest's own roots: the server may well be behind an internal
        // CA, which the compiled-in Mozilla set knows nothing about.
        let http = pa_oidc::tls::http_client_builder()
            .user_agent(concat!("pa/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(ApiClient {
            http,
            base: cfg.api_base(),
            server: cfg.server.clone(),
            client_id: cfg.client_id.clone(),
            token_endpoint: cfg.token_endpoint.clone(),
            org: cfg.org.clone(),
            lang: cfg.lang.clone(),
            tokens: Mutex::new(TokenState {
                access: cfg.access_token.clone(),
                refresh: cfg.refresh_token.clone(),
            }),
        })
    }

    /// The current access token (for the control-WS handshake subprotocol).
    pub async fn access_token(&self) -> String {
        self.tokens.lock().await.access.clone()
    }

    /// The control WebSocket URL (http(s) → ws(s)), `/api/v1/ws`.
    pub fn ws_url(&self) -> String {
        let base = self.server.trim_end_matches('/');
        let base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}/api/v1/ws")
    }

    /// The interactive-terminal WebSocket URL for a chat's bound coding workspace.
    pub fn terminal_ws_url(&self, chat_id: &str, cols: u16, rows: u16) -> String {
        let base = self.server.trim_end_matches('/');
        let base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}/api/v1/ws/terminal/{chat_id}?cols={cols}&rows={rows}")
    }

    /// Force a token refresh (used by the WS task before a reconnect attempt).
    pub async fn force_refresh(&self) -> Result<()> {
        self.refresh_token().await.map(|_| ())
    }

    fn attach(&self, rb: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
        let rb = rb.header(AUTHORIZATION, format!("Bearer {token}"));
        match &self.org {
            Some(org) => rb.header(ORG_HEADER, org),
            None => rb,
        }
    }

    /// Refresh the access token, persist it, and return the fresh access token.
    async fn refresh_token(&self) -> Result<String> {
        let mut guard = self.tokens.lock().await;
        let fresh = crate::oidc::refresh(&self.token_endpoint, &self.client_id, &guard.refresh)
            .await
            .context(crate::i18n::t(crate::i18n::Msg::ApiRefreshFailed))?;
        guard.access = fresh.access_token.clone();
        guard.refresh = fresh.refresh_token.clone();
        // Persist so the next process start reuses the rotated refresh token.
        let cfg = Config {
            server: self.server.clone(),
            client_id: self.client_id.clone(),
            token_endpoint: self.token_endpoint.clone(),
            access_token: guard.access.clone(),
            refresh_token: guard.refresh.clone(),
            org: self.org.clone(),
            lang: self.lang.clone(),
        };
        let _ = crate::config::save(&cfg);
        Ok(fresh.access_token)
    }

    /// Send a request with auth; on 401 refresh once and retry. `build` must be able to
    /// reconstruct the request (method + url + body) so the retry is a fresh send.
    async fn send<F>(&self, build: F) -> Result<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let token = { self.tokens.lock().await.access.clone() };
        let resp = self.attach(build(), &token).send().await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            let token = self.refresh_token().await?;
            let resp = self.attach(build(), &token).send().await?;
            return Ok(resp.error_for_status()?);
        }
        Ok(resp.error_for_status()?)
    }

    pub async fn me(&self) -> Result<Me> {
        let url = format!("{}/me", self.base);
        Ok(self.send(|| self.http.get(&url)).await?.json().await?)
    }

    pub async fn list_chats(&self) -> Result<Vec<Chat>> {
        let url = format!("{}/chats", self.base);
        Ok(self.send(|| self.http.get(&url)).await?.json().await?)
    }

    pub async fn create_chat(&self, title: &str) -> Result<Chat> {
        let url = format!("{}/chats", self.base);
        let body = serde_json::json!({ "title": title });
        Ok(self
            .send(|| self.http.post(&url).json(&body))
            .await?
            .json()
            .await?)
    }

    /// Reserve an ordinary computer device before the separate Computer Service enrolls.
    pub async fn create_computer_device(&self, name: &str) -> Result<ComputerDevice> {
        let url = format!("{}/devices", self.base);
        let body = serde_json::json!({ "name": name, "kind": "computer" });
        Ok(self
            .send(|| self.http.post(&url).json(&body))
            .await?
            .json()
            .await?)
    }

    pub fn server_origin(&self) -> &str {
        &self.server
    }

    pub async fn rename_chat(&self, chat_id: &str, title: &str) -> Result<()> {
        let url = format!("{}/chats/{}", self.base, chat_id);
        let body = serde_json::json!({ "title": title });
        self.send(|| self.http.patch(&url).json(&body)).await?;
        Ok(())
    }

    pub async fn list_messages(&self, chat_id: &str) -> Result<Vec<Message>> {
        let url = format!("{}/chats/{}/messages?limit=200", self.base, chat_id);
        let mut msgs: Vec<Message> = self.send(|| self.http.get(&url)).await?.json().await?;
        // The history endpoint returns turn-aligned rows; render oldest-first.
        msgs.sort_by_key(|m| m.position);
        Ok(msgs)
    }

    /// The user's custom slash commands (prompt templates) for the composer palette.
    pub async fn list_commands(&self) -> Result<Vec<CustomCommand>> {
        let url = format!("{}/commands", self.base);
        Ok(self.send(|| self.http.get(&url)).await?.json().await?)
    }

    pub async fn list_models(&self) -> Result<Vec<Model>> {
        let url = format!("{}/models", self.base);
        let resp: ModelsResponse = self.send(|| self.http.get(&url)).await?.json().await?;
        Ok(resp.models)
    }

    /// The user's skills. `GET /skills` returns them all -- no paging, no filter parameters --
    /// so the filtering happens in the picker.
    pub async fn list_skills(&self) -> Result<Vec<Skill>> {
        let url = format!("{}/skills", self.base);
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            items: Vec<Skill>,
        }
        let resp: Wrap = self.send(|| self.http.get(&url)).await?.json().await?;
        Ok(resp.items)
    }

    /// Turn one skill on or off.
    ///
    /// The only write the TUI offers on a skill: it is the one that changes what the agent can
    /// reach in the next turn, and it is a single bit. Editing instructions belongs in the web
    /// editor, which has room for them.
    pub async fn set_skill_enabled(&self, skill_id: &str, enabled: bool) -> Result<()> {
        let url = format!("{}/skills/{}", self.base, skill_id);
        let body = serde_json::json!({ "enabled": enabled });
        self.send(|| self.http.patch(&url).json(&body)).await?;
        Ok(())
    }

    /// Open the SSE stream for a new run. Returns the live response; the caller reads the
    /// `X-Run-Id` header and tails `bytes_stream()`. Refreshes + retries once on 401.
    pub async fn open_run_stream(
        &self,
        chat_id: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chats/{}/runs", self.base, chat_id);
        self.send(|| {
            self.http
                .post(&url)
                .header(ACCEPT, "text/event-stream")
                .json(body)
        })
        .await
    }

    /// Open the SSE stream for a `/btw` side query (ephemeral, read-only — not persisted).
    pub async fn open_btw_stream(
        &self,
        chat_id: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chats/{}/btw", self.base, chat_id);
        self.send(|| {
            self.http
                .post(&url)
                .header(ACCEPT, "text/event-stream")
                .json(body)
        })
        .await
    }

    /// Open the SSE stream for a rerun (regenerate the last answer / edit + re-run).
    pub async fn open_rerun_stream(
        &self,
        chat_id: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chats/{}/runs/rerun", self.base, chat_id);
        self.send(|| {
            self.http
                .post(&url)
                .header(ACCEPT, "text/event-stream")
                .json(body)
        })
        .await
    }

    /// Attach to an EXISTING run's live stream (reconnect / background-resumed). `Last-Event-ID: 0`
    /// replays from the start so the answer-so-far is rebuilt; a finished run replays + closes.
    pub async fn attach_run_stream(
        &self,
        chat_id: &str,
        run_id: &str,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chats/{}/runs/{}/stream", self.base, chat_id, run_id);
        self.send(|| {
            self.http
                .get(&url)
                .header(ACCEPT, "text/event-stream")
                .header("Last-Event-ID", "0")
        })
        .await
    }

    /// Approve or reject a pending device tool call (HITL). `remember` adds a standing
    /// allow-rule for the exact command so it stops asking.
    pub async fn decide_approval(
        &self,
        approval_id: &str,
        approved: bool,
        remember: bool,
    ) -> Result<()> {
        let verb = if approved { "approve" } else { "reject" };
        let url = format!("{}/approvals/{}/{}", self.base, approval_id, verb);
        let body = serde_json::json!({ "remember": remember });
        self.send(|| self.http.post(&url).json(&body)).await?;
        Ok(())
    }

    /// Answer a deferred agent question and resume the run (the continuation arrives via a
    /// background-resumed push the caller attaches to). A single-question frame sends the
    /// `answer` shape; a multi-question frame sends `answers` (one selection list per question).
    pub async fn answer_question(
        &self,
        chat_id: &str,
        question_id: &str,
        answers: &[Vec<String>],
        multi: bool,
    ) -> Result<()> {
        let url = format!(
            "{}/chats/{}/questions/{}/answer",
            self.base, chat_id, question_id
        );
        let body = if multi {
            serde_json::json!({ "answers": answers })
        } else {
            let single = answers
                .first()
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or_default();
            serde_json::json!({ "answer": single })
        };
        self.send(|| self.http.post(&url).json(&body)).await?;
        Ok(())
    }

    // ── conversations / inbox ────────────────────────────────────────────────

    /// The Posteingang grouped into conversations. `counts` always reflects the full set
    /// (so the badges stay stable); an optional `status` narrows the returned items.
    pub async fn list_conversations(&self, status: Option<&str>) -> Result<ConversationList> {
        let mut url = format!("{}/conversations?limit=50", self.base);
        if let Some(s) = status {
            url.push_str(&format!("&status={s}"));
        }
        Ok(self.send(|| self.http.get(&url)).await?.json().await?)
    }

    /// A completed sub-agent run's persisted transcript (404 while it's still running).
    pub async fn get_run_transcript(&self, chat_id: &str, run_id: &str) -> Result<Vec<MsgPart>> {
        let url = format!("{}/chats/{}/runs/{}/transcript", self.base, chat_id, run_id);
        let t: RunTranscript = self.send(|| self.http.get(&url)).await?.json().await?;
        Ok(t.parts)
    }

    /// Sub-agent runs of a chat (running + completed/failed), newest first — feeds the
    /// agents drawer with the historical agents the live `subagent_update` pushes miss.
    pub async fn list_agent_runs(&self, chat_id: &str) -> Result<Vec<AgentRun>> {
        let url = format!("{}/chats/{}/agents?limit=100", self.base, chat_id);
        let list: AgentRunList = self.send(|| self.http.get(&url)).await?.json().await?;
        Ok(list.items)
    }

    /// Enqueue a message typed while a run is in flight (server-side, Redis-backed queue
    /// shared across the user's clients) — drained as a normal run when the current settles.
    pub async fn enqueue_followup(&self, chat_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/chats/{}/followups", self.base, chat_id);
        let body = serde_json::json!({ "text": text });
        self.send(|| self.http.post(&url).json(&body)).await?;
        Ok(())
    }

    /// Drop all queued follow-ups (e.g. the user cancelled the current response).
    pub async fn clear_followups(&self, chat_id: &str) -> Result<()> {
        let url = format!("{}/chats/{}/followups", self.base, chat_id);
        self.send(|| self.http.delete(&url)).await?;
        Ok(())
    }

    pub async fn get_conversation(&self, entity_id: &str) -> Result<ConversationDetail> {
        let url = format!("{}/conversations/{}", self.base, entity_id);
        Ok(self.send(|| self.http.get(&url)).await?.json().await?)
    }

    /// Suggest (but do not send) a reply addressing the whole unanswered batch.
    pub async fn suggest_reply(&self, entity_id: &str) -> Result<SuggestReply> {
        let url = format!("{}/conversations/{}/suggest", self.base, entity_id);
        Ok(self.send(|| self.http.post(&url)).await?.json().await?)
    }

    /// Send the user's own reply into the thread (a human action — sent immediately).
    pub async fn reply_conversation(&self, entity_id: &str, body: &str) -> Result<()> {
        let url = format!("{}/conversations/{}/reply", self.base, entity_id);
        let payload = serde_json::json!({ "body": body });
        self.send(|| self.http.post(&url).json(&payload)).await?;
        Ok(())
    }

    /// Resolve the conversation without replying.
    pub async fn conversation_done(&self, entity_id: &str) -> Result<()> {
        let url = format!("{}/conversations/{}/done", self.base, entity_id);
        self.send(|| self.http.post(&url)).await?;
        Ok(())
    }

    /// Dismiss the suggestion / mark the unanswered batch read without answering.
    pub async fn conversation_read(&self, entity_id: &str) -> Result<()> {
        let url = format!("{}/conversations/{}/read", self.base, entity_id);
        self.send(|| self.http.post(&url)).await?;
        Ok(())
    }

    // ── integrations / per-chat tool selection ───────────────────────────────

    /// The per-chat tool catalog for the integrations picker: every integration / built-in /
    /// web / MCP group with the tools it contributes. Assembled exactly like a run, so it
    /// reflects this chat's governance. Deselected tools feed the `disabled_tools` deny-list.
    pub async fn list_integrations_catalog(&self, chat_id: &str) -> Result<Vec<IntegrationGroup>> {
        let url = format!("{}/chats/{}/integrations-catalog", self.base, chat_id);
        let cat: IntegrationsCatalog = self.send(|| self.http.get(&url)).await?.json().await?;
        Ok(cat.groups)
    }

    /// The chat's current `disabled_tools` deny-list (from its stored `run_config`). Empty when
    /// the chat uses the full enabled tool set — so the picker starts from the saved state.
    pub async fn get_disabled_tools(&self, chat_id: &str) -> Result<Vec<String>> {
        let url = format!("{}/chats/{}", self.base, chat_id);
        let v: serde_json::Value = self.send(|| self.http.get(&url)).await?.json().await?;
        let out = v
            .get("run_config")
            .and_then(|rc| rc.get("disabled_tools"))
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(out)
    }

    /// Persist the chat's tool selection (the `disabled_tools` deny-list) into its `run_config`.
    /// The backend merges this key, so other run-config fields (model, security, …) are kept.
    pub async fn set_disabled_tools(&self, chat_id: &str, disabled: &[String]) -> Result<()> {
        let url = format!("{}/chats/{}", self.base, chat_id);
        let body = serde_json::json!({ "disabled_tools": disabled });
        self.send(|| self.http.patch(&url).json(&body)).await?;
        Ok(())
    }

    /// The chat's context-fill + cumulative session usage, for the status-bar telemetry.
    pub async fn get_chat_context(&self, chat_id: &str) -> Result<ChatContext> {
        let url = format!("{}/chats/{}/context", self.base, chat_id);
        Ok(self.send(|| self.http.get(&url)).await?.json().await?)
    }

    /// The chat's per-chat memory-access policy (`run_config.memory_access`), or None when it
    /// inherits the user default.
    pub async fn get_memory_access(&self, chat_id: &str) -> Result<Option<MemoryAccess>> {
        let url = format!("{}/chats/{}", self.base, chat_id);
        let v: serde_json::Value = self.send(|| self.http.get(&url)).await?.json().await?;
        let ma = v.get("run_config").and_then(|rc| rc.get("memory_access"));
        Ok(match ma {
            Some(m) if m.is_object() => serde_json::from_value(m.clone()).ok(),
            _ => None,
        })
    }

    /// Persist the chat's memory-access policy into its run_config. A `None` axis is sent as JSON
    /// null (unrestricted); an explicit list narrows it.
    pub async fn set_memory_access(
        &self,
        chat_id: &str,
        mode: &str,
        domains: Option<&[String]>,
        sources: Option<&[String]>,
    ) -> Result<()> {
        let url = format!("{}/chats/{}", self.base, chat_id);
        let axis = |v: Option<&[String]>| match v {
            Some(list) => serde_json::json!(list),
            None => serde_json::Value::Null,
        };
        let body = serde_json::json!({
            "memory_access": { "mode": mode, "domains": axis(domains), "sources": axis(sources) }
        });
        self.send(|| self.http.patch(&url).json(&body)).await?;
        Ok(())
    }
}
