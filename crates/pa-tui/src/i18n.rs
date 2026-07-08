//! Lightweight i18n. Every user-facing string funnels through `t(Msg)`, so the display
//! language is chosen once at startup and the call sites stay declarative. German is the
//! default (the product is German-first); English is selectable. Code and docs stay English.
//!
//! Language resolution order: an explicit value (config `lang`), the `PA_TUI_LANG` env var,
//! then the system `LANG` locale — otherwise German.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    De,
    En,
}

// 0 = unset (→ De), 1 = De, 2 = En. Atomic so `init_from` can be called more than once
// (env/system at startup, then refined by the loaded config's `lang`).
static LANG: AtomicU8 = AtomicU8::new(0);

pub fn init_from(explicit: Option<&str>) {
    let lang = explicit
        .and_then(parse)
        .or_else(|| std::env::var("PA_TUI_LANG").ok().and_then(|v| parse(&v)))
        .or_else(|| std::env::var("LANG").ok().and_then(|v| system(&v)))
        .unwrap_or(Lang::De);
    LANG.store(if lang == Lang::En { 2 } else { 1 }, Ordering::Relaxed);
}

fn parse(s: &str) -> Option<Lang> {
    match s.trim().to_ascii_lowercase().as_str() {
        "de" | "de-de" | "german" | "deutsch" => Some(Lang::De),
        "en" | "en-us" | "en-gb" | "english" => Some(Lang::En),
        _ => None,
    }
}

fn system(s: &str) -> Option<Lang> {
    let s = s.to_ascii_lowercase();
    if s.starts_with("en") {
        Some(Lang::En)
    } else if s.starts_with("de") {
        Some(Lang::De)
    } else {
        None
    }
}

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 2 {
        Lang::En
    } else {
        Lang::De
    }
}

fn en() -> bool {
    lang() == Lang::En
}

/// Background operations whose failure is reported as "<label>: <error>".
#[derive(Clone, Copy)]
pub enum Op {
    Me,
    Chats,
    Models,
    Commands,
    History,
    CreateChat,
    Approval,
    Answer,
    Conversations,
    Conversation,
    Agents,
    Transcript,
    Followup,
    Suggest,
    Reply,
    Done,
    Dismiss,
    Attach,
    Integrations,
    Memory,
}

/// Every translatable message. Dynamic parts are borrowed so call sites read naturally.
pub enum Msg<'a> {
    // status line
    Ready,
    SignedIn(&'a str),
    LoadingHistory,
    HistoryLoaded,
    NoChatSelected,
    AgentWorking,
    ConnectingRun,
    ShellCodingOnly,
    ShellBusy,
    ShellRunning,
    Reconnecting,
    StreamReconnectFailed,
    StreamLost,
    CancelSent,
    RunCancelled,
    Done,
    RunError(&'a str),
    LiveConnected,
    LiveDisconnected,
    SubagentUpdate {
        kind: &'a str,
        status: &'a str,
        label: &'a str,
    },
    NoteDraftPending,
    NoteInbox,
    NoteMemory,
    ToolAllowed,
    ToolRejected,
    AnswerSent,
    ModelSet(&'a str),
    ModelDefaultStatus,
    ToolsState(bool),
    OpError(Op, &'a str),
    // tags inserted into transcript text
    CancelledTag,
    ErrorTag(&'a str),
    NewChatTitle,
    // panes / roles
    Untitled,
    RoleYou,
    RoleAgent,
    RoleAside,
    RoleShell,
    RoleSteer,
    NoChatOpen,
    PaneChatWelcome,
    // composer
    ComposerLabel,
    ShellComposerLabel,
    ModelDefaultShort,
    ToolsShort(bool),
    StatusHint,
    // popups
    ModelPopupTitle,
    ModelDefaultRow,
    SessionsPopupTitle,
    SessionsNoMatch,
    MainChatOpened,
    AgentsPopupTitle,
    AgentsEmpty,
    TranscriptRunning,
    TranscriptLoading,
    TranscriptTitle(&'a str),
    TranscriptHint,
    AgentStatus(&'a str),
    AgentKind(&'a str),
    AgentsRunning(usize),
    FollowupQueued(usize),
    HelpTitle,
    ApprovalDevice(&'a str),
    ApprovalTool(&'a str),
    ApprovalAllow,
    ApprovalRemember,
    ApprovalReject,
    ApprovalTitle,
    QuestionCustom(&'a str),
    QuestionTitle,
    QuestionProgress(usize, usize),
    QuestionHint,
    QuestionIncomplete,
    // inbox / conversations
    PaneInbox,
    PaneThread,
    InboxEmpty,
    NoConversationOpen,
    ReplyComposer,
    ConvStatus(&'a str),
    ConvSummary(&'a str),
    Suggesting,
    ReplySent,
    ConvResolved,
    ConvDismissed,
    DraftPrefix,
    InboxFilter(&'a str),
    InboxHint,
    // attachments
    AttachTitle,
    AttachAdded(&'a str),
    AttachTooMany,
    AttachChip(usize),
    // integrations (per-chat tool deny-list picker)
    IntegrationsPopupTitle,
    IntegrationsHint,
    IntegrationsEmpty,
    IntegrationsSaved(usize),
    IntGroupBuiltin,
    IntGroupWeb,
    // memory access (per-chat world-memory scoping)
    MemoryPopupTitle,
    MemoryHint,
    MemoryDefaultStatus,
    MemorySet(&'a str),
    MemSectionModes,
    MemSectionDomains,
    MemSectionSources,
    MemMode(&'a str),
    MemDomain(&'a str),
    MemSource(&'a str),
    // slash commands
    CmdMenuTitle,
    CmdDesc(&'a str),
    CmdUnknown(&'a str),
    ThinkingSet(&'a str),
    SideQuestionEmpty,
    SteerEmpty,
    SteerSent,
    // security mode
    SecurityPopupTitle,
    SecurityDefaultRow,
    SecurityDefaultStatus,
    SecurityLabel(&'a str),
    SecuritySet(&'a str),
    SecurityUnknown(&'a str),
    // /rename, /summarize, /proofread
    RenameEmpty,
    Renamed(&'a str),
    SummarizePrompt,
    ProofreadEmpty,
    ProofreadPrompt(&'a str),
    // CLI (login / logout) output
    OidcOpen,
    OidcCode(&'a str),
    OidcWaiting,
    OidcFailed(&'a str),
    ConfigNotEnrolled(&'a str),
    ApiRefreshFailed,
    LoginSuccess(&'a str),
    LoginStartHint,
    LogoutDone(&'a str),
    LogoutNone,
}

fn pick(de: &str, en_s: &str) -> String {
    if en() { en_s } else { de }.to_string()
}

pub fn t(m: Msg) -> String {
    match m {
        Msg::Ready => pick("Bereit · F1 Hilfe", "Ready · F1 help"),
        Msg::SignedIn(w) => {
            if en() {
                format!("Signed in as {w} · F1 help")
            } else {
                format!("Angemeldet als {w} · F1 Hilfe")
            }
        }
        Msg::LoadingHistory => pick("Lade Verlauf …", "Loading history …"),
        Msg::HistoryLoaded => pick("Verlauf geladen", "History loaded"),
        Msg::NoChatSelected => pick(
            "Kein Chat ausgewählt (Enter in der Liste, oder Strg+N)",
            "No chat selected (Enter in the list, or Ctrl+N)",
        ),
        Msg::AgentWorking => pick("Agent arbeitet …", "Agent working …"),
        Msg::ConnectingRun => pick("Verbinde mit laufendem Run …", "Connecting to running run …"),
        Msg::ShellCodingOnly => pick(
            "!-Shell nur im Coding-Modus mit Workspace verfügbar",
            "!-shell only in coding mode with a workspace",
        ),
        Msg::ShellBusy => pick(
            "Ein Shell-Befehl läuft bereits …",
            "A shell command is already running …",
        ),
        Msg::ShellRunning => pick("Shell-Befehl läuft …", "Running shell command …"),
        Msg::Reconnecting => pick(
            "Verbindung verloren – verbinde neu …",
            "Connection lost – reconnecting …",
        ),
        Msg::StreamReconnectFailed => pick(
            "Reconnect fehlgeschlagen – Run läuft serverseitig weiter (Chat neu öffnen)",
            "Reconnect failed – the run continues server-side (reopen the chat)",
        ),
        Msg::StreamLost => pick("Verbindung zum Stream verloren", "Lost the stream connection"),
        Msg::CancelSent => pick("Abbruch gesendet …", "Cancel sent …"),
        Msg::RunCancelled => pick("Run abgebrochen", "Run cancelled"),
        Msg::Done => pick("Fertig", "Done"),
        Msg::RunError(e) => {
            if en() {
                format!("Run error: {e}")
            } else {
                format!("Run-Fehler: {e}")
            }
        }
        Msg::LiveConnected => pick("Live verbunden", "Live connected"),
        Msg::LiveDisconnected => {
            pick("Live getrennt — verbinde neu …", "Live disconnected — reconnecting …")
        }
        Msg::SubagentUpdate { kind, status, label } => {
            let label = if label.is_empty() { String::new() } else { format!(": {label}") };
            if en() {
                format!("Sub-agent {kind} {status}{label}")
            } else {
                format!("Sub-Agent {kind} {status}{label}")
            }
        }
        Msg::NoteDraftPending => {
            pick("Neuer Entwurf wartet auf Freigabe", "A new draft is awaiting approval")
        }
        Msg::NoteInbox => pick("Posteingang aktualisiert", "Inbox updated"),
        Msg::NoteMemory => pick("Gedächtnis aktualisiert", "Memory updated"),
        Msg::ToolAllowed => pick("Tool-Aufruf erlaubt", "Tool call allowed"),
        Msg::ToolRejected => pick("Tool-Aufruf abgelehnt", "Tool call rejected"),
        Msg::AnswerSent => {
            pick("Antwort gesendet — Agent setzt fort …", "Answer sent — agent resuming …")
        }
        Msg::ModelSet(m) => {
            if en() {
                format!("Model: {m}")
            } else {
                format!("Modell: {m}")
            }
        }
        Msg::ModelDefaultStatus => pick("Modell: Chat-Standard", "Model: chat default"),
        Msg::ToolsState(on) => {
            if en() {
                format!("Built-in tools: {}", if on { "on" } else { "off" })
            } else {
                format!("Eingebaute Tools: {}", if on { "an" } else { "aus" })
            }
        }
        Msg::OpError(op, e) => {
            let label = op_label(op);
            format!("{label}: {e}")
        }
        Msg::CancelledTag => pick("[abgebrochen]", "[cancelled]"),
        Msg::ErrorTag(e) => {
            if en() {
                format!("[Error] {e}")
            } else {
                format!("[Fehler] {e}")
            }
        }
        Msg::NewChatTitle => pick("Neuer Chat", "New chat"),
        Msg::Untitled => pick("(ohne Titel)", "(untitled)"),
        Msg::RoleYou => pick("Du", "You"),
        Msg::RoleAgent => pick("Agent", "Agent"),
        Msg::RoleAside => pick("Nebenfrage", "Aside"),
        Msg::RoleShell => pick("Shell", "Shell"),
        Msg::RoleSteer => pick("↪ Steuern", "↪ Steer"),
        Msg::NoChatOpen => pick(
            "Kein Chat geöffnet. Wähle links eine Sitzung (Enter) oder lege mit Strg+N eine neue an.",
            "No chat open. Pick a session on the left (Enter) or create one with Ctrl+N.",
        ),
        Msg::PaneChatWelcome => pick(" Chat — willkommen ", " Chat — welcome "),
        Msg::ComposerLabel => pick("Nachricht", "Message"),
        Msg::ShellComposerLabel => pick(
            "⚡ Shell · Workspace · Enter ausführen",
            "⚡ Shell · workspace · Enter to run",
        ),
        Msg::ModelDefaultShort => pick("Standard", "default"),
        Msg::ToolsShort(on) => {
            if en() {
                (if on { "tools on" } else { "tools off" }).to_string()
            } else {
                (if on { "Tools an" } else { "Tools aus" }).to_string()
            }
        }
        Msg::StatusHint => pick(
            "Enter senden · F4 Sitzungen · F5 Agenten · Strg+M/F6 Hauptchat · Strg+N neu · F2 Modell · F8 Integrationen · Strg+T Tools · Strg+X Stop · F3 Posteingang · F1 Hilfe",
            "Enter send · F4 sessions · F5 agents · Ctrl+M/F6 main · Ctrl+N new · F2 model · F8 integrations · Ctrl+T tools · Ctrl+X stop · F3 inbox · F1 help",
        ),
        Msg::ModelPopupTitle => {
            pick(" Modell wählen (Enter) · Esc ", " Pick a model (Enter) · Esc ")
        }
        Msg::ModelDefaultRow => pick("(Standard — Chat-Vorgabe)", "(Default — chat preset)"),
        Msg::SessionsPopupTitle => pick(
            " Sitzung suchen · ↑/↓ · Enter öffnen · Strg+N neu · Esc ",
            " Search sessions · ↑/↓ · Enter open · Ctrl+N new · Esc ",
        ),
        Msg::SessionsNoMatch => pick("Keine Treffer", "No matches"),
        Msg::MainChatOpened => pick("Hauptchat", "Main chat"),
        Msg::AgentsPopupTitle => pick(
            " Agenten & Tasks · suchen · ↑/↓ · Enter Transkript · Esc ",
            " Agents & tasks · search · ↑/↓ · Enter transcript · Esc ",
        ),
        Msg::AgentsEmpty => pick(
            "Noch keine Sub-Agenten in diesem Chat.",
            "No sub-agents in this chat yet.",
        ),
        Msg::TranscriptRunning => pick(
            "Läuft noch – Transkript erst nach Abschluss",
            "Still running – transcript available once it finishes",
        ),
        Msg::TranscriptLoading => pick("Transkript laden …", "Loading transcript …"),
        Msg::TranscriptTitle(name) => {
            if en() {
                format!(" 🧩 Transcript · {name} ")
            } else {
                format!(" 🧩 Transkript · {name} ")
            }
        }
        Msg::TranscriptHint => pick(
            "Bild↑/↓ scrollen · Esc zurück zu den Agenten",
            "PgUp/PgDn scroll · Esc back to agents",
        ),
        Msg::AgentStatus(s) => match s {
            "running" => pick("läuft", "running"),
            "completed" => pick("fertig", "completed"),
            "failed" => pick("fehlgeschlagen", "failed"),
            other => other.to_string(),
        },
        Msg::AgentKind(k) => match k {
            "explore" => pick("Recherche-Agent", "Research agent"),
            "delegate" | "generic" => pick("Delegierter Agent", "Delegated agent"),
            "code-reviewer" => pick("Code-Reviewer", "Code reviewer"),
            "agent" | "" => pick("Agent", "Agent"),
            other => other.to_string(),
        },
        Msg::AgentsRunning(n) => {
            if en() {
                format!("▸ {n} agent(s) running")
            } else {
                format!("▸ {n} Agent(en) aktiv")
            }
        }
        Msg::FollowupQueued(n) => {
            if en() {
                format!("⏳ {n} queued")
            } else {
                format!("⏳ {n} in Warteschlange")
            }
        }
        Msg::HelpTitle => pick(" Hilfe · Esc ", " Help · Esc "),
        Msg::ApprovalDevice(d) => {
            if en() {
                format!("Device {d} wants to run:")
            } else {
                format!("Gerät {d} möchte ausführen:")
            }
        }
        Msg::ApprovalTool(t) => format!("Tool: {t}"),
        Msg::ApprovalAllow => pick("Erlauben", "Allow"),
        Msg::ApprovalRemember => pick("Erlauben + merken", "Allow + remember"),
        Msg::ApprovalReject => pick("Ablehnen", "Reject"),
        Msg::ApprovalTitle => {
            pick(" Tool-Freigabe · ↑/↓ · Enter · Esc ", " Tool approval · ↑/↓ · Enter · Esc ")
        }
        Msg::QuestionCustom(input) => {
            if en() {
                format!("Custom answer: {input}▏")
            } else {
                format!("Eigene Antwort: {input}▏")
            }
        }
        Msg::QuestionTitle => pick(" Frage des Agenten · Esc ", " Agent question · Esc "),
        Msg::QuestionProgress(i, n) => {
            if en() {
                format!("Question {i}/{n}")
            } else {
                format!("Frage {i}/{n}")
            }
        }
        Msg::QuestionHint => pick(
            "↑/↓ Option · Leertaste wählen · ←/→ Frage · Enter weiter/senden · Esc",
            "↑/↓ option · Space select · ←/→ question · Enter next/send · Esc",
        ),
        Msg::QuestionIncomplete => {
            pick("Bitte jede Frage beantworten", "Please answer every question")
        }
        Msg::PaneInbox => pick(" Posteingang ", " Inbox "),
        Msg::PaneThread => pick(" Konversation ", " Conversation "),
        Msg::InboxEmpty => pick("Keine Konversationen", "No conversations"),
        Msg::NoConversationOpen => pick(
            "Keine Konversation geöffnet. Wähle links eine (Enter).",
            "No conversation open. Pick one on the left (Enter).",
        ),
        Msg::ReplyComposer => pick(
            " Antwort · Enter senden · s Vorschlag ",
            " Reply · Enter to send · s suggest ",
        ),
        Msg::ConvStatus(s) => conv_status(s),
        Msg::ConvSummary(s) => format!("ℹ {s}"),
        Msg::Suggesting => pick("Erstelle Vorschlag …", "Drafting a suggestion …"),
        Msg::ReplySent => pick("Antwort gesendet", "Reply sent"),
        Msg::ConvResolved => pick("Konversation erledigt", "Conversation resolved"),
        Msg::ConvDismissed => pick("Als gelesen markiert", "Marked as read"),
        Msg::DraftPrefix => pick("Entwurf (Tab übernimmt):", "Draft (Tab adopts):"),
        Msg::InboxFilter(s) => format!("Filter: {}", conv_status(s)),
        Msg::AttachTitle => pick(
            " Datei anhängen · Pfad eingeben · Enter · Esc ",
            " Attach file · type a path · Enter · Esc ",
        ),
        Msg::AttachAdded(name) => {
            if en() {
                format!("Attached: {name}")
            } else {
                format!("Angehängt: {name}")
            }
        }
        Msg::AttachTooMany => pick("Höchstens 6 Anhänge", "At most 6 attachments"),
        Msg::AttachChip(n) => format!("📎{n}"),
        Msg::IntegrationsPopupTitle => pick(
            " Integrationen suchen · Leertaste umschalten · Esc ",
            " Search integrations · Space toggle · Esc ",
        ),
        Msg::IntegrationsHint => pick(
            " ↑/↓ · Leertaste Tool/Gruppe · Enter fertig ",
            " ↑/↓ · Space tool/group · Enter done ",
        ),
        Msg::IntegrationsEmpty => pick(
            "Keine Tools/Integrationen für diesen Chat.",
            "No tools/integrations for this chat.",
        ),
        Msg::IntegrationsSaved(n) => {
            if en() {
                format!("Tool selection saved ({n} disabled)")
            } else {
                format!("Tool-Auswahl gespeichert ({n} deaktiviert)")
            }
        }
        Msg::IntGroupBuiltin => pick("Basis-Tools", "Basic tools"),
        Msg::IntGroupWeb => pick("Web-Tools", "Web tools"),
        Msg::MemoryPopupTitle => pick(
            " Gedächtnis-Zugriff · ↑/↓ · Enter/Leertaste · Esc ",
            " Memory access · ↑/↓ · Enter/Space · Esc ",
        ),
        Msg::MemoryHint => pick(
            "Modus wählen; Begrenzt schaltet Domänen/Quellen frei.",
            "Pick a mode; Scoped reveals the domain/source boxes.",
        ),
        Msg::MemoryDefaultStatus => pick(
            "Gedächtnis: Nutzer-Vorgabe",
            "Memory: user default",
        ),
        Msg::MemorySet(mode) => {
            let label = t(Msg::MemMode(mode));
            if en() {
                format!("Memory: {label}")
            } else {
                format!("Gedächtnis: {label}")
            }
        }
        Msg::MemSectionModes => pick("Modus", "Mode"),
        Msg::MemSectionDomains => pick("Domänen", "Domains"),
        Msg::MemSectionSources => pick("Quellen", "Sources"),
        Msg::MemMode(m) => match m {
            "default" => pick("Standard (Nutzer-Vorgabe)", "Default (user setting)").to_string(),
            "full" => pick("Voll", "Full").to_string(),
            "none" => pick("Aus", "Off").to_string(),
            "scoped" => pick("Begrenzt", "Scoped").to_string(),
            other => other.to_string(),
        },
        Msg::MemDomain(d) => match d {
            "people" => pick("Personen", "People").to_string(),
            "work" => pick("Arbeit", "Work").to_string(),
            "places_devices" => pick("Orte & Geräte", "Places & devices").to_string(),
            "notes_topics" => pick("Notizen & Themen", "Notes & topics").to_string(),
            other => other.to_string(),
        },
        Msg::MemSource(s) => match s {
            "preferences" => pick("Präferenzen", "Preferences").to_string(),
            "stated" => pick("Explizit Gesagtes", "Stated").to_string(),
            "inferred" => pick("Agenten-Schlüsse", "Inferred").to_string(),
            "observed" => pick("Beobachtungen", "Observed").to_string(),
            other => other.to_string(),
        },
        Msg::CmdMenuTitle => pick(
            " Befehle · Tab vervollständigen · Enter ausführen ",
            " Commands · Tab to complete · Enter to run ",
        ),
        Msg::CmdDesc(name) => cmd_desc(name),
        Msg::CmdUnknown(name) => {
            if en() {
                format!("Unknown command: {name}")
            } else {
                format!("Unbekannter Befehl: {name}")
            }
        }
        Msg::ThinkingSet(level) => format!("Thinking: {level}"),
        Msg::SideQuestionEmpty => pick("/btw braucht eine Frage", "/btw needs a question"),
        Msg::SteerEmpty => pick(
            "/steer braucht eine Korrektur (z. B. /steer nur src/ ändern)",
            "/steer needs a correction (e.g. /steer only touch src/)",
        ),
        Msg::SteerSent => pick(
            "Steuerung eingespielt — Agent passt sich am nächsten Tool-Schritt an",
            "Steer injected — the agent adjusts at the next tool step",
        ),
        Msg::SecurityPopupTitle => {
            pick(" Sicherheitsmodus (F7) ", " Security mode (F7) ")
        }
        Msg::SecurityDefaultRow => pick("(Standard)", "(default)"),
        Msg::SecurityDefaultStatus => {
            pick("Sicherheit: Standard", "Security: default")
        }
        Msg::SecurityLabel(m) => match m {
            "autonomous" => pick("autonom", "autonomous").to_string(),
            "approve_each" => pick("jede bestätigen", "approve each").to_string(),
            "judge" => pick("Prüfer", "judge").to_string(),
            other => other.to_string(),
        },
        Msg::SecuritySet(m) => {
            let label = t(Msg::SecurityLabel(m));
            if en() {
                format!("Security: {label}")
            } else {
                format!("Sicherheit: {label}")
            }
        }
        Msg::SecurityUnknown(m) => {
            if en() {
                format!("Unknown security mode: {m}")
            } else {
                format!("Unbekannter Sicherheitsmodus: {m}")
            }
        }
        Msg::RenameEmpty => pick("/rename braucht einen Titel", "/rename needs a title"),
        Msg::Renamed(title) => {
            if en() {
                format!("Renamed to: {title}")
            } else {
                format!("Umbenannt in: {title}")
            }
        }
        Msg::SummarizePrompt => pick(
            "Fasse diese Unterhaltung knapp als Stichpunkte zusammen.",
            "Summarize this conversation concisely as bullet points.",
        ),
        Msg::ProofreadEmpty => pick("/proofread braucht Text", "/proofread needs text"),
        Msg::ProofreadPrompt(text) => {
            if en() {
                format!(
                    "Proofread the following for spelling and grammar and return only the \
                     corrected version:\n\n{text}"
                )
            } else {
                format!(
                    "Korrigiere den folgenden Text auf Rechtschreibung und Grammatik und gib \
                     nur die korrigierte Version zurück:\n\n{text}"
                )
            }
        }
        Msg::InboxHint => pick(
            "↑/↓ wählen · Enter öffnen · f Filter · s Vorschlag · d erledigt · x verwerfen · F3 Chat",
            "↑/↓ select · Enter open · f filter · s suggest · d done · x dismiss · F3 chat",
        ),
        Msg::OidcOpen => pick(
            "\n  Zum Anmelden im Browser öffnen:",
            "\n  Open in your browser to sign in:",
        ),
        Msg::OidcCode(c) => {
            if en() {
                format!("  and confirm this code: {c}\n")
            } else {
                format!("  und diesen Code bestätigen: {c}\n")
            }
        }
        Msg::OidcWaiting => pick("  Warte auf Bestätigung …", "  Waiting for confirmation …"),
        Msg::OidcFailed(e) => {
            if en() {
                format!("Sign-in failed: {e}")
            } else {
                format!("Anmeldung fehlgeschlagen: {e}")
            }
        }
        Msg::ConfigNotEnrolled(path) => {
            if en() {
                format!("not signed in — run `personal-agent-tui login` first ({path})")
            } else {
                format!("nicht angemeldet — zuerst `personal-agent-tui login` ausführen ({path})")
            }
        }
        Msg::ApiRefreshFailed => pick(
            "Token-Refresh fehlgeschlagen — bitte erneut `login` ausführen",
            "token refresh failed — please run `login` again",
        ),
        Msg::LoginSuccess(path) => {
            if en() {
                format!("\nSigned in ✓ config: {path}")
            } else {
                format!("\nAngemeldet ✓ Konfiguration: {path}")
            }
        }
        Msg::LoginStartHint => pick(
            "Start the UI with: personal-agent-tui",
            "Start the UI with: personal-agent-tui",
        ),
        Msg::LogoutDone(path) => {
            if en() {
                format!("Signed out — removed {path}.")
            } else {
                format!("Abgemeldet — {path} entfernt.")
            }
        }
        Msg::LogoutNone => {
            pick("Keine gespeicherte Anmeldung gefunden.", "No stored sign-in found.")
        }
    }
}

fn cmd_desc(name: &str) -> String {
    let en = en();
    match name {
        "/btw" => {
            if en {
                "Side question (ephemeral)"
            } else {
                "Seitenfrage (ephemer)"
            }
        }
        "/retry" => {
            if en {
                "Regenerate the last answer"
            } else {
                "Letzte Antwort neu generieren"
            }
        }
        "/steer" => {
            if en {
                "Steer a running answer mid-flight <text>"
            } else {
                "Laufende Antwort mid-run lenken <Text>"
            }
        }
        "/model" => {
            if en {
                "Pick a model [name]"
            } else {
                "Modell wählen [Name]"
            }
        }
        "/think" => {
            if en {
                "Thinking effort: off|low|medium|high"
            } else {
                "Thinking-Stufe: off|low|medium|high"
            }
        }
        "/tools" => {
            if en {
                "Toggle built-in tools"
            } else {
                "Eingebaute Tools an/aus"
            }
        }
        "/attach" => {
            if en {
                "Attach a file <path>"
            } else {
                "Datei anhängen <Pfad>"
            }
        }
        "/security" => {
            if en {
                "Security mode: autonomous|approve_each|judge"
            } else {
                "Sicherheitsmodus: autonomous|approve_each|judge"
            }
        }
        "/agents" => {
            if en {
                "Sub-agents / background tasks"
            } else {
                "Sub-Agenten / Hintergrund-Tasks"
            }
        }
        "/integrations" => {
            if en {
                "Pick tools / integrations (per chat)"
            } else {
                "Tools / Integrationen wählen (pro Chat)"
            }
        }
        "/memory" => {
            if en {
                "Memory access for this chat (full/none/scoped)"
            } else {
                "Gedächtnis-Zugriff für diesen Chat (voll/aus/begrenzt)"
            }
        }
        "/main" => {
            if en {
                "Jump to the main chat [message]"
            } else {
                "Zum Hauptchat springen [Nachricht]"
            }
        }
        "/rename" => {
            if en {
                "Rename this chat <title>"
            } else {
                "Diesen Chat umbenennen <Titel>"
            }
        }
        "/summarize" => {
            if en {
                "Summarize this conversation"
            } else {
                "Diese Unterhaltung zusammenfassen"
            }
        }
        "/proofread" => {
            if en {
                "Proofread text <text>"
            } else {
                "Text korrigieren <Text>"
            }
        }
        "/new" => {
            if en {
                "New chat"
            } else {
                "Neuer Chat"
            }
        }
        "/inbox" => {
            if en {
                "Open the inbox"
            } else {
                "Posteingang öffnen"
            }
        }
        "/logout" => {
            if en {
                "Log out (remove credentials)"
            } else {
                "Abmelden (Zugang entfernen)"
            }
        }
        "/help" => {
            if en {
                "Show help"
            } else {
                "Hilfe anzeigen"
            }
        }
        _ => "",
    }
    .to_string()
}

fn conv_status(s: &str) -> String {
    let en = en();
    match s {
        "" | "all" => {
            if en {
                "All"
            } else {
                "Alle"
            }
        }
        "new" => {
            if en {
                "New"
            } else {
                "Neu"
            }
        }
        "needs_reply" => {
            if en {
                "Needs reply"
            } else {
                "Antwort nötig"
            }
        }
        "answered" => {
            if en {
                "Answered"
            } else {
                "Beantwortet"
            }
        }
        "seen" => {
            if en {
                "Seen"
            } else {
                "Gesehen"
            }
        }
        "open" => {
            if en {
                "Open"
            } else {
                "Offen"
            }
        }
        other => other,
    }
    .to_string()
}

fn op_label(op: Op) -> &'static str {
    let en = en();
    match op {
        Op::Me => "/me",
        Op::Chats => {
            if en {
                "Loading chats"
            } else {
                "Chats laden"
            }
        }
        Op::Models => {
            if en {
                "Loading models"
            } else {
                "Modelle laden"
            }
        }
        Op::Commands => {
            if en {
                "Loading commands"
            } else {
                "Befehle laden"
            }
        }
        Op::History => {
            if en {
                "Loading history"
            } else {
                "Verlauf laden"
            }
        }
        Op::CreateChat => {
            if en {
                "Creating chat"
            } else {
                "Chat anlegen"
            }
        }
        Op::Approval => {
            if en {
                "Approval"
            } else {
                "Freigabe"
            }
        }
        Op::Answer => {
            if en {
                "Answer"
            } else {
                "Antwort"
            }
        }
        Op::Conversations => {
            if en {
                "Loading inbox"
            } else {
                "Posteingang laden"
            }
        }
        Op::Conversation => {
            if en {
                "Loading conversation"
            } else {
                "Konversation laden"
            }
        }
        Op::Agents => {
            if en {
                "Loading agents"
            } else {
                "Agenten laden"
            }
        }
        Op::Transcript => {
            if en {
                "Loading transcript"
            } else {
                "Transkript laden"
            }
        }
        Op::Followup => {
            if en {
                "Follow-up"
            } else {
                "Folgenachricht"
            }
        }
        Op::Suggest => {
            if en {
                "Suggestion"
            } else {
                "Vorschlag"
            }
        }
        Op::Reply => {
            if en {
                "Reply"
            } else {
                "Antwort senden"
            }
        }
        Op::Done => {
            if en {
                "Resolve"
            } else {
                "Erledigen"
            }
        }
        Op::Dismiss => {
            if en {
                "Dismiss"
            } else {
                "Verwerfen"
            }
        }
        Op::Attach => {
            if en {
                "Attachment"
            } else {
                "Anhang"
            }
        }
        Op::Integrations => {
            if en {
                "Integrations"
            } else {
                "Integrationen"
            }
        }
        Op::Memory => {
            if en {
                "Memory"
            } else {
                "Gedächtnis"
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The display language is a process-global (`static LANG`); serialize the tests that
    // mutate it so they don't race when the harness runs tests in parallel.
    static LANG_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn switches_language_and_formats_args() {
        let _guard = LANG_LOCK.lock().unwrap();
        init_from(Some("en"));
        assert_eq!(t(Msg::Done), "Done");
        assert_eq!(t(Msg::SignedIn("ada")), "Signed in as ada · F1 help");
        assert_eq!(t(Msg::QuestionProgress(2, 3)), "Question 2/3");

        init_from(Some("de"));
        assert_eq!(t(Msg::Done), "Fertig");
        assert_eq!(t(Msg::ToolsState(true)), "Eingebaute Tools: an");
        assert_eq!(t(Msg::ConvStatus("needs_reply")), "Antwort nötig");
        assert_eq!(t(Msg::ConvStatus("unknown_x")), "unknown_x");
    }

    #[test]
    fn working_phrase_rotates_and_localizes() {
        let _guard = LANG_LOCK.lock().unwrap();
        init_from(Some("de"));
        assert_eq!(working_phrase(0), "Denkt nach…");
        // Wraps around the list (12 phrases).
        assert_eq!(working_phrase(12), "Denkt nach…");
        assert_eq!(working_phrase(1), "Lässt die Zahnräder rattern…");
        init_from(Some("en"));
        assert_eq!(working_phrase(0), "Thinking…");
    }

    #[test]
    fn parses_locale_strings() {
        assert!(matches!(parse("EN-US"), Some(Lang::En)));
        assert!(matches!(parse("Deutsch"), Some(Lang::De)));
        assert!(parse("fr").is_none());
    }
}

/// Playful "agent is working" phrases, rotating while a run streams (mirrors the web UI's
/// `WORKING_PHRASES`). Localized, English fallback; `tick` advances every few seconds.
pub fn working_phrase(tick: usize) -> &'static str {
    const DE: [&str; 12] = [
        "Denkt nach…",
        "Lässt die Zahnräder rattern…",
        "Knobelt an einer Antwort…",
        "Sortiert die Gedanken…",
        "Strengt die Synapsen an…",
        "Wühlt sich durch…",
        "Verbindet die Punkte…",
        "Tüftelt…",
        "Holt kurz Luft…",
        "Rechnet scharf nach…",
        "Bastelt etwas Schlaues…",
        "Gleich ist es so weit…",
    ];
    const EN: [&str; 12] = [
        "Thinking…",
        "Spinning up the gears…",
        "Pondering an answer…",
        "Collecting its thoughts…",
        "Firing some synapses…",
        "Digging through it…",
        "Connecting the dots…",
        "Tinkering…",
        "Taking a deep breath…",
        "Crunching the details…",
        "Cooking up something clever…",
        "Almost there…",
    ];
    let list: &[&str] = if en() { &EN } else { &DE };
    list[tick % list.len()]
}

/// The help popup body (localized), one entry per line.
pub fn help_lines() -> Vec<&'static str> {
    if en() {
        vec![
            "Personal Agent — terminal UI",
            "",
            "  Enter         Send the message",
            "  Alt/Shift+↵   Newline (multiline prompt)",
            "  ↑/↓           Prompt history (line up/down in a multiline draft)",
            "  ←/→ Home/End  Move the cursor · Ctrl+W delete word · Ctrl+U clear line",
            "  F4            Sessions (search / open a chat)",
            "  F5            Agents drawer (sub-agents / background tasks)",
            "  Ctrl+M / F6   Jump to the main chat",
            "  Ctrl+N        Create a new chat",
            "  F2            Pick a model",
            "  F8            Integrations & tools (per chat, search + toggle)",
            "  F9            Memory access (full / none / scoped) for this chat",
            "  F7            Security mode (autonomous / approve / judge)",
            "  Ctrl+T        Toggle built-in tools",
            "  Ctrl+O        Attach a file (image / document)",
            "  Ctrl+X        Cancel the running run",
            "  F3            Toggle chat ↔ inbox",
            "  PgUp/PgDn     Scroll the transcript",
            "  F1            This help",
            "  Ctrl+Q        Quit",
            "",
            "Type / in the composer for commands (/btw, /retry, /model, /think …).",
            "Same HTTP API as the web app · sign-in via Keycloak device flow.",
        ]
    } else {
        vec![
            "Personal Agent — Terminal-UI",
            "",
            "  Enter         Nachricht senden",
            "  Alt/Umsch+↵   Zeilenumbruch (mehrzeilig)",
            "  ↑/↓           Verlauf (bzw. Zeile hoch/runter im mehrzeiligen Entwurf)",
            "  ←/→ Pos1/Ende Cursor bewegen · Strg+W Wort löschen · Strg+U Zeile leeren",
            "  F4            Sitzungen (suchen / öffnen)",
            "  F5            Agenten-Drawer (Sub-Agenten / Tasks)",
            "  Strg+M / F6   Zum Hauptchat springen",
            "  Strg+N        Neuen Chat anlegen",
            "  F2            Modell wählen",
            "  F8            Integrationen & Tools (pro Chat, suchen + umschalten)",
            "  F9            Gedächtnis-Zugriff (voll / aus / begrenzt) für diesen Chat",
            "  F7            Sicherheitsmodus (autonom / bestätigen / Prüfer)",
            "  Strg+T        Eingebaute Tools an/aus",
            "  Strg+O        Datei anhängen (Bild / Dokument)",
            "  Strg+X        Laufenden Run abbrechen",
            "  F3            Chat ↔ Posteingang wechseln",
            "  Bild ↑/↓      Verlauf scrollen",
            "  F1            Diese Hilfe",
            "  Strg+Q        Beenden",
            "",
            "Tippe / in der Eingabe für Befehle (/btw, /retry, /model, /think …).",
            "Gleiche HTTP-API wie die Web-App · Anmeldung via Keycloak Device-Flow.",
        ]
    }
}
