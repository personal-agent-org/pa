//! Rendering — the chat transcript with a composer below it, and a status bar. Popups
//! (session picker, model picker, help, …) overlay it.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use serde_json::Value;

use crate::app::{App, InboxFocus, IntRow, MemRow, Popup, UiMessage, UiTool, View};
use crate::i18n::{self, t, Msg};

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠇"];
/// Redraw ticks between playful-phrase changes (~5s at the 120ms tick, matching the web UI).
const PHRASE_TICKS: usize = 42;

pub fn draw(f: &mut Frame, app: &App) {
    // A telemetry strip (model · context% · tokens · cost · elapsed) sits just above the status
    // line in the chat view; the inbox/transcript views don't need it and keep the single line.
    let telemetry = app.view == View::Chat;
    let constraints = if telemetry {
        vec![
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(3), Constraint::Length(1)]
    };
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());
    let body = root[0];

    match app.view {
        View::Chat => draw_chat(f, app, body),
        View::Inbox => draw_inbox(f, app, body),
        View::Transcript => draw_transcript_view(f, app, body),
    }

    if telemetry {
        draw_telemetry(f, app, root[1]);
        draw_status(f, app, root[2]);
    } else {
        draw_status(f, app, root[1]);
    }

    match app.popup {
        Popup::Models => draw_models_popup(f, app),
        Popup::Security => draw_security_popup(f, app),
        Popup::Sessions => draw_sessions_popup(f, app),
        Popup::Agents => draw_agents_popup(f, app),
        Popup::Help => draw_help_popup(f, app),
        Popup::Approval => draw_approval_popup(f, app),
        Popup::Question => draw_question_popup(f, app),
        Popup::Attach => draw_attach_popup(f, app),
        Popup::Integrations => draw_integrations_popup(f, app),
        Popup::Memory => draw_memory_popup(f, app),
        Popup::Skills => draw_skills_popup(f, app),
        Popup::Messages => draw_message_picker(f, app),
        Popup::None => {}
    }

    // Slash-command menu: a non-modal hint while the composer holds a `/…` (chat view only).
    if app.popup == Popup::None && app.view == View::Chat && app.input.starts_with('/') {
        draw_command_menu(f, app, body);
    }
}

fn draw_command_menu(f: &mut Frame, app: &App, body: Rect) {
    let items_data = app.palette();
    if items_data.is_empty() {
        return;
    }
    let rows = items_data.len().min(8) as u16;
    let height = rows + 2;
    let width = 52.min(body.width);
    // Anchor just above the composer (the composer occupies the bottom 3 rows of the body).
    let y = body.height.saturating_sub(3 + height).max(body.y);
    let area = Rect {
        x: body.x + 1,
        y,
        width,
        height,
    };
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = items_data
        .iter()
        .map(|it| ListItem::new(format!("{:<10} {}", it.name, it.desc)))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t(Msg::CmdMenuTitle))
                .border_style(Style::default().fg(accent(app))),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = ListState::default();
    state.select(Some(app.cmd_sel.min(items_data.len() - 1)));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_chat(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(composer_height(app))])
        .split(area);
    draw_messages(f, app, rows[0]);
    draw_composer(f, app, rows[1]);
}

/// The UI accent colour: the user's web `ui.accent` if set, else the built-in cyan.
fn accent(app: &App) -> Color {
    match app.accent {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Cyan,
    }
}

fn border_style(app: &App, active: bool) -> Style {
    if active {
        Style::default().fg(accent(app))
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_sessions_popup(f: &mut Frame, app: &App) {
    let area = centered(60, 70, f.area());
    f.render_widget(Clear, area);

    // A search box on top, the filtered session list below.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let search = Paragraph::new(format!("🔍 {}▏", app.session_query)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::SessionsPopupTitle))
            .border_style(Style::default().fg(accent(app))),
    );
    f.render_widget(search, rows[0]);

    let results = app.session_results();
    let items: Vec<ListItem> = if results.is_empty() {
        let empty = if app.chats.is_empty() {
            t(Msg::Untitled)
        } else {
            t(Msg::SessionsNoMatch)
        };
        vec![ListItem::new(Span::styled(
            empty,
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        results
            .iter()
            .map(|&i| {
                let c = &app.chats[i];
                let icon = crate::app::mode_icon(&c.mode);
                let title = if c.title.is_empty() {
                    t(Msg::Untitled)
                } else {
                    c.title.clone()
                };
                // Active (a run in flight) sessions flash their dot ~every 0.5s so they stand out.
                let marker = if c.active {
                    let on = (app.spinner / 4).is_multiple_of(2);
                    let style = if on {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    Span::styled("●", style)
                } else {
                    Span::raw(" ")
                };
                // Last-active time, dimmed after the title.
                let when = c
                    .updated_at
                    .as_deref()
                    .map(crate::app::short_time)
                    .unwrap_or_default();
                ListItem::new(Line::from(vec![
                    marker,
                    Span::raw(format!(" {icon} {title}")),
                    Span::styled(format!("  {when}"), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent(app))),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

    let mut state = ListState::default();
    if !results.is_empty() {
        state.select(Some(app.sel_chat.min(results.len() - 1)));
    }
    f.render_stateful_widget(list, rows[1], &mut state);
}

/// The transcript as lines, plus how many of them are FINISHED.
///
/// Finished means the line can be printed into the terminal's scrollback and never touched
/// again. Every line of a settled turn qualifies; of a turn that is still streaming, every line
/// except the last, which is still growing (personal-agent-org/personal-agent#126).
///
/// Shared with the alternate-screen views, which render the same transcript and simply ignore
/// the second number.
pub fn transcript_lines(app: &App, width: usize) -> (Vec<Line<'static>>, usize) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut finished = 0usize;
    for m in &app.messages {
        let before = lines.len();
        render_message(&mut lines, m, width, app.spinner);
        lines.push(Line::from(""));
        if m.pending {
            // The blank separator was pushed after a line that may still grow, so neither it
            // nor that line is finished yet.
            finished = lines.len().saturating_sub(2).max(before);
        } else {
            finished = lines.len();
        }
    }
    let finished = finished.min(lines.len());
    (lines, finished)
}

fn draw_messages(f: &mut Frame, app: &App, area: Rect) {
    let inner_w = area.width.saturating_sub(2).max(1) as usize;

    let mut lines: Vec<Line> = Vec::new();
    if app.current_chat.is_none() {
        lines.push(Line::from(Span::styled(
            t(Msg::NoChatOpen),
            Style::default().fg(Color::DarkGray),
        )));
    }
    for m in &app.messages {
        render_message(&mut lines, m, inner_w, app.spinner);
        lines.push(Line::from(""));
    }

    // Show the open session's name (with its mode icon) as the pane title, not a generic "Chat".
    let title = match app
        .current_chat
        .as_ref()
        .and_then(|id| app.chats.iter().find(|c| &c.id == id))
    {
        Some(c) => {
            let name = if c.title.is_empty() {
                t(Msg::Untitled)
            } else {
                c.title.clone()
            };
            format!(" {} {} ", crate::app::mode_icon(&c.mode), name)
        }
        None => t(Msg::PaneChatWelcome),
    };
    draw_message_pane(f, app, area, lines, title, app.scroll);
}

/// A sub-agent's transcript shown full-screen using the normal chat message view.
fn draw_transcript_view(f: &mut Frame, app: &App, area: Rect) {
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();
    if app.transcript.is_empty() {
        lines.push(Line::from(Span::styled(
            t(Msg::AgentsEmpty),
            Style::default().fg(Color::DarkGray),
        )));
    }
    for m in &app.transcript {
        render_message(&mut lines, m, inner_w, app.spinner);
        lines.push(Line::from(""));
    }
    let title = t(Msg::TranscriptTitle(&app.transcript_title));
    draw_message_pane(f, app, area, lines, title, app.transcript_scroll);
}

/// Render a bordered, word-wrapped, bottom-anchored message pane (the chat transcript and
/// the sub-agent transcript share this). `Paragraph` wraps; `line_count` clamps the scroll.
fn draw_message_pane(
    f: &mut Frame,
    app: &App,
    area: Rect,
    lines: Vec<Line>,
    title: String,
    scroll: usize,
) {
    let inner_h = area.height.saturating_sub(2).max(1) as usize;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app, false));
    let para = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    let total = para.line_count(area.width);
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = scroll.min(max_scroll);
    let top = (max_scroll - scroll) as u16;
    f.render_widget(para.scroll((top, 0)), area);
}

/// Tools whose call IS a task list — rendered as a checklist, not raw JSON.
const TODO_TOOLS: [&str; 2] = ["todowrite", "todoread"];
/// File-editing tools — their result carries a rendered diff we colour as one.
const EDIT_TOOLS: [&str; 6] = [
    "write_file",
    "edit_file",
    "multi_edit",
    "apply_patch",
    "create_file",
    "str_replace",
];

/// Strip the `dev_<short>_` device prefix → the bare tool name (mirrors the backend).
fn bare_tool(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("dev_") {
        if let Some(idx) = rest.find('_') {
            return &rest[idx + 1..];
        }
    }
    name
}

/// A tool result that reads as a failure (so we mark it ✗ instead of ✓).
fn tool_looks_error(r: &str) -> bool {
    let t = r.trim_start();
    [
        "[error",
        "[failed",
        "[blocked",
        "[not executed",
        "Error",
        "error:",
        "Traceback",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

/// The most informative single arg to show inline (command / path / query …).
fn primary_arg(args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args).ok()?;
    let obj = v.as_object()?;
    for key in [
        "command",
        "path",
        "file_path",
        "file",
        "filename",
        "query",
        "pattern",
        "url",
        "name",
        "cwd",
    ] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// One tool call: a status-iconed header + a tailored body (todos as a checklist, file
/// edits as a coloured diff, everything else a short collapsed result preview).
fn render_tool(lines: &mut Vec<Line<'static>>, tool: &UiTool, width: usize, spinner: usize) {
    let bare = bare_tool(&tool.name);
    let (icon, icon_color) = match &tool.result {
        None => (SPINNER[spinner % SPINNER.len()], Color::Yellow),
        Some(r) if tool_looks_error(r) => ("✗", Color::Red),
        Some(_) => ("✓", Color::Green),
    };
    let mut spans = vec![
        Span::styled(format!("  {icon} "), Style::default().fg(icon_color)),
        Span::styled(
            bare.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(arg) = primary_arg(&tool.args) {
        let arg = sanitize(&arg).replace('\n', " ");
        spans.push(Span::styled(
            format!("  {}", truncate(&arg, width.saturating_sub(bare.len() + 8))),
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(spans));

    let bare_l = bare.to_ascii_lowercase();
    if TODO_TOOLS.contains(&bare_l.as_str()) {
        render_todos(lines, &tool.args, width);
    } else if EDIT_TOOLS.contains(&bare_l.as_str()) {
        // The diff is in the result (device tools pre-read + render it); fall back to the
        // patch in the args for apply_patch when there's no result yet.
        let body = tool
            .result
            .as_deref()
            .filter(|r| !r.is_empty())
            .map(sanitize)
            .or_else(|| diff_from_args(&tool.args));
        if let Some(d) = body {
            render_diff(lines, &d, width);
        }
    } else if let Some(r) = &tool.result {
        render_result_preview(lines, r, width);
    }
}

/// Render `todowrite` args (`{items:[{content,status}]}`) as a checklist.
fn render_todos(lines: &mut Vec<Line<'static>>, args: &str, width: usize) {
    let Ok(v) = serde_json::from_str::<Value>(args) else {
        return;
    };
    let Some(items) = v.get("items").and_then(Value::as_array) else {
        return;
    };
    for it in items {
        let content = it.get("content").and_then(Value::as_str).unwrap_or("");
        let status = it
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let (mark, color) = match status {
            "completed" => ("[x]", Color::Green),
            "in_progress" => ("[~]", Color::Yellow),
            _ => ("[ ]", Color::DarkGray),
        };
        for (i, w) in wrap(content, width.saturating_sub(8))
            .into_iter()
            .enumerate()
        {
            let prefix = if i == 0 {
                format!("    {mark} ")
            } else {
                "        ".to_string()
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{w}"),
                Style::default().fg(color),
            )));
        }
    }
}

/// The patch text inside an `apply_patch` args object, if any.
fn diff_from_args(args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args).ok()?;
    let obj = v.as_object()?;
    for key in ["changes", "patch", "diff", "content"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(sanitize(s));
            }
        }
    }
    None
}

/// Colour a unified-diff/patch block: +added green, -removed red, @@hunks cyan, rest dim.
fn render_diff(lines: &mut Vec<Line<'static>>, text: &str, width: usize) {
    const MAX: usize = 24;
    let total = text.lines().count();
    for (n, line) in text.lines().enumerate() {
        if n >= MAX {
            lines.push(Line::from(Span::styled(
                format!("    … (+{} lines)", total - MAX),
                Style::default().fg(Color::DarkGray),
            )));
            break;
        }
        let color = if line.starts_with("@@") {
            Color::Cyan
        } else if line.starts_with('+') && !line.starts_with("+++") {
            Color::Green
        } else if line.starts_with('-') && !line.starts_with("---") {
            Color::Red
        } else {
            Color::DarkGray
        };
        lines.push(Line::from(Span::styled(
            format!("    {}", truncate(line, width.saturating_sub(4))),
            Style::default().fg(color),
        )));
    }
}

/// A short, collapsed preview of a generic tool result (first few lines, dimmed).
fn render_result_preview(lines: &mut Vec<Line<'static>>, result: &str, width: usize) {
    const MAX: usize = 6;
    let s = sanitize(result);
    let total = s.lines().count();
    for (n, line) in s.lines().enumerate() {
        if n >= MAX {
            lines.push(Line::from(Span::styled(
                format!("    … (+{} lines)", total - MAX),
                Style::default().fg(Color::DarkGray),
            )));
            break;
        }
        lines.push(Line::from(Span::styled(
            format!("    {}", truncate(line, width.saturating_sub(4))),
            Style::default().fg(Color::DarkGray),
        )));
    }
}

fn render_message(lines: &mut Vec<Line<'static>>, m: &UiMessage, width: usize, spinner: usize) {
    let (label, color) = match m.role.as_str() {
        "user" => (t(Msg::RoleYou), Color::Green),
        "assistant" => (t(Msg::RoleAgent), Color::Cyan),
        "side" => (t(Msg::RoleAside), Color::Magenta),
        "shell" => (t(Msg::RoleShell), Color::Yellow),
        "steer" => (t(Msg::RoleSteer), Color::Blue),
        other => (other.to_string(), Color::Magenta),
    };
    let mut header = label;
    if m.pending {
        header.push(' ');
        header.push_str(SPINNER[spinner % SPINNER.len()]);
    }
    lines.push(Line::from(Span::styled(
        header,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));

    // While the agent is working but hasn't produced visible text yet, show a rotating
    // playful phrase next to the spinner (mirrors the web chat's "agent is working" line).
    if m.pending && m.text.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", i18n::working_phrase(spinner / PHRASE_TICKS)),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    if !m.thinking.is_empty() {
        for w in wrap(&sanitize(&m.thinking), width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(
                format!("  {w}"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }

    for tool in &m.tools {
        render_tool(lines, tool, width, spinner);
    }

    // Shell (`!cmd`) output is verbatim terminal text — render it plain (no markdown), so
    // command output isn't reinterpreted. Everything else renders as markdown.
    if !m.text.is_empty() {
        if m.role == "shell" {
            for w in wrap(&sanitize(&m.text), width) {
                lines.push(Line::from(Span::styled(
                    w,
                    Style::default().fg(Color::Gray),
                )));
            }
        } else {
            // Sanitize first (strip ANSI/control chars incl. CR) so arbitrary model output
            // can't desync the terminal, then render markdown into owned ('static) lines.
            let body = sanitize(&m.text);
            for line in tui_markdown::from_str(&body).lines {
                let spans: Vec<Span<'static>> = line
                    .spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style))
                    .collect();
                lines.push(Line::from(spans));
            }
        }
    }

    if let Some(u) = &m.usage {
        let mut meta = String::new();
        if let Some(model) = &u.model_name {
            meta.push_str(model);
            meta.push_str(" · ");
        }
        meta.push_str(&format!("{}↑ {}↓ Tokens", u.input_tokens, u.output_tokens));
        if let Some(cost) = u.cost_usd {
            meta.push_str(&format!(" · ${cost:.4}"));
        }
        lines.push(Line::from(Span::styled(
            meta,
            Style::default().fg(Color::DarkGray),
        )));
    }
}

fn draw_composer(f: &mut Frame, app: &App, area: Rect) {
    let model = match &app.model {
        None => t(Msg::ModelDefaultShort),
        Some(id) => app
            .models
            .iter()
            .find(|m| &m.id == id)
            .map(|m| m.model.clone())
            .unwrap_or_else(|| id.clone()),
    };
    let tools = t(Msg::ToolsShort(app.tools_enabled));
    let chip = if app.attachments.is_empty() {
        String::new()
    } else {
        format!(" · {}", t(Msg::AttachChip(app.attachments.len())))
    };
    let think = match &app.thinking {
        Some(level) => format!(" · 🧠{level}"),
        None => String::new(),
    };
    // Non-default security mode shown as a lock chip (default = inherit, no chip).
    let sec = match &app.security_mode {
        Some(mode) => format!(" · 🔒{}", t(Msg::SecurityLabel(mode))),
        None => String::new(),
    };
    // Queued mid-run follow-ups (you can keep typing/sending while a run streams).
    let queued = if app.followup_count > 0 {
        format!(" · {}", t(Msg::FollowupQueued(app.followup_count)))
    } else {
        String::new()
    };
    // `!…` puts the composer in shell mode — but only in coding mode, where the workspace
    // shell actually applies. In any other mode the composer never changes colour/text.
    let shell_mode = app.input.starts_with('!') && app.is_mode("coding");
    let title = if shell_mode {
        format!(" {} ", t(Msg::ShellComposerLabel))
    } else {
        format!(
            " {} · {model} · {tools}{sec}{chip}{think}{queued} ",
            t(Msg::ComposerLabel)
        )
    };

    // The composer stays editable during a run — typing is never blocked; a send while
    // streaming queues a follow-up. The running state shows in the bubble + status bar.
    // Render the cursor as a block glyph inserted at its real byte index (multiline-safe).
    let focused = app.popup == Popup::None;
    let mut text = app.input.clone();
    if focused {
        let at = app.cursor.min(text.len());
        text.insert(at, '▏');
    }

    let border = if shell_mode {
        Style::default().fg(Color::Yellow)
    } else {
        border_style(app, focused)
    };
    let text_style = if shell_mode {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let p = Paragraph::new(text).style(text_style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border),
    );
    f.render_widget(p, area);
}

/// Composer box height: grows with the draft's line count, bordered, clamped to a sane band.
fn composer_height(app: &App) -> u16 {
    let lines = app.input.lines().count().max(1) + usize::from(app.input.ends_with('\n'));
    (lines as u16 + 2).clamp(3, 10)
}

/// Compact token count: `940`, `12.3k`, `1.2M` (gauge-friendly, like the web meter).
fn fmt_k(n: i64) -> String {
    let n = n.max(0);
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// A run's elapsed wall-clock as `m:ss` (a long run reads `12:04`).
fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// Context-fill colour, mirroring the web meter's bands: green &lt;50%, yellow &lt;75%,
/// orange &lt;90%, red at/over 90%.
fn ctx_color(pct: i64) -> Color {
    match pct {
        p if p < 50 => Color::Green,
        p if p < 75 => Color::Yellow,
        p if p < 90 => Color::Rgb(255, 165, 0),
        _ => Color::Red,
    }
}

/// The telemetry strip: model · context-fill % (coloured) · cumulative tokens · cost · the
/// active run's elapsed timer. Reads the per-chat `GET …/context` snapshot; quiet (just the
/// model) for a fresh chat with no usage yet.
fn draw_telemetry(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans: Vec<Span> = Vec::new();

    // Model: the context snapshot's resolved model, else the composer's pick / chat default.
    let model = app
        .context
        .as_ref()
        .and_then(|c| c.model_name.clone())
        .or_else(|| app.model.clone())
        .unwrap_or_else(|| t(Msg::ModelDefaultShort));
    spans.push(Span::styled(
        format!(" {model}"),
        Style::default().fg(Color::Cyan),
    ));

    if let Some(cx) = &app.context {
        if cx.window_tokens > 0 {
            let pct = ((cx.context_tokens as f64 / cx.window_tokens as f64) * 100.0).round() as i64;
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(
                format!(
                    "⌑ {pct}% ({}/{})",
                    fmt_k(cx.context_tokens),
                    fmt_k(cx.window_tokens)
                ),
                Style::default().fg(ctx_color(pct)),
            ));
            if cx.compacted {
                // History was already auto-compacted on the latest turn.
                spans.push(Span::styled(" ⟲", Style::default().fg(Color::Magenta)));
            } else if cx.threshold_tokens > 0 && cx.context_tokens >= cx.threshold_tokens {
                // Past the auto-compaction threshold — the next turn will compact history.
                spans.push(Span::styled(
                    " ⟲",
                    Style::default().fg(Color::Rgb(255, 165, 0)),
                ));
            }
        }
        let s = &cx.session;
        if s.input_tokens > 0 || s.output_tokens > 0 {
            spans.push(Span::styled(
                format!("  {}↑ {}↓", fmt_k(s.input_tokens), fmt_k(s.output_tokens)),
                dim,
            ));
        }
        if s.cost_usd > 0.0 {
            spans.push(Span::styled(format!("  ${:.4}", s.cost_usd), dim));
        }
    }

    // The active run's elapsed timer (only while streaming).
    if let Some(elapsed) = app.run_elapsed() {
        spans.push(Span::styled(
            format!("  ⏱ {}", fmt_elapsed(elapsed)),
            Style::default().fg(Color::Yellow),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (dot, dot_color) = if app.live {
        ("●", Color::Green)
    } else {
        ("○", Color::DarkGray)
    };
    let hint = match app.view {
        View::Chat => t(Msg::StatusHint),
        View::Inbox => t(Msg::InboxHint),
        View::Transcript => t(Msg::TranscriptHint),
    };
    let mut spans = vec![
        Span::styled(format!(" {dot} "), Style::default().fg(dot_color)),
        Span::styled(
            format!(" {} ", app.status),
            Style::default().fg(Color::Black).bg(accent(app)),
        ),
        Span::raw(" "),
    ];
    // Live chip: number of sub-agents still running in the current chat (F5 to inspect).
    let running = app.running_subagents();
    if running > 0 {
        spans.push(Span::styled(
            format!("{} ", t(Msg::AgentsRunning(running))),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── inbox / conversations ─────────────────────────────────────────────────────

fn draw_inbox(f: &mut Frame, app: &App, area: Rect) {
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(38), Constraint::Min(20)])
        .split(area);

    draw_conv_list(f, app, body[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(body[1]);
    draw_thread(f, app, right[0]);
    draw_reply(f, app, right[1]);
}

fn draw_conv_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = if app.convs.is_empty() {
        vec![ListItem::new(t(Msg::InboxEmpty))]
    } else {
        app.convs
            .iter()
            .map(|c| {
                let marker = if c.unread { "● " } else { "  " };
                let title = if c.title.is_empty() {
                    t(Msg::Untitled)
                } else {
                    c.title.clone()
                };
                let second = if !c.subtitle.is_empty() {
                    &c.subtitle
                } else {
                    &c.snippet
                };
                // Secondary line: channel + triage status + a snippet/subtitle preview.
                let mut meta = String::new();
                if !c.channel.is_empty() {
                    meta.push_str(&format!("{} · ", c.channel));
                }
                if !c.status.is_empty() {
                    meta.push_str(&format!("{} · ", t(Msg::ConvStatus(&c.status))));
                }
                ListItem::new(vec![
                    Line::from(format!("{marker}{title}")),
                    Line::from(Span::styled(
                        format!("    {}{}", meta, truncate(second, 28)),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect()
    };

    // Title carries the active filter + the unfiltered count badges.
    let filter = app.conv_filter;
    let filter_label = crate::app::filter_label(filter);
    let total: i64 = app
        .conv_counts
        .get("all")
        .copied()
        .unwrap_or(app.convs.len() as i64);
    let title = format!(
        "{}· {} ({total})",
        t(Msg::PaneInbox),
        t(Msg::InboxFilter(filter_label))
    );

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(border_style(app, app.inbox_focus == InboxFocus::List)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

    let mut state = ListState::default();
    if !app.convs.is_empty() {
        state.select(Some(app.sel_conv.min(app.convs.len() - 1)));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_thread(f: &mut Frame, app: &App, area: Rect) {
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let inner_h = area.height.saturating_sub(2).max(1) as usize;

    let mut lines: Vec<Line> = Vec::new();
    match &app.conv_detail {
        None => lines.push(Line::from(Span::styled(
            t(Msg::NoConversationOpen),
            Style::default().fg(Color::DarkGray),
        ))),
        Some(d) => {
            // Header: subtitle (e.g. the email subject) + the triage status.
            let mut head = d.subtitle.clone();
            if !d.status.is_empty() {
                let status = t(Msg::ConvStatus(&d.status));
                head = if head.is_empty() {
                    format!("[{status}]")
                } else {
                    format!("{head}  [{status}]")
                };
            }
            if !head.is_empty() {
                lines.push(Line::from(Span::styled(
                    head,
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if !d.summary.is_empty() {
                for w in wrap(&t(Msg::ConvSummary(&d.summary)), inner_w) {
                    lines.push(Line::from(Span::styled(
                        w,
                        Style::default().fg(Color::Yellow),
                    )));
                }
            }
            lines.push(Line::from(""));
            for m in &d.messages {
                let out = m.direction == "out";
                let who = if out {
                    if m.by == "agent" {
                        t(Msg::RoleAgent)
                    } else {
                        t(Msg::RoleYou)
                    }
                } else if m.sender.is_empty() {
                    "—".to_string()
                } else {
                    m.sender.clone()
                };
                let color = if out { Color::Green } else { Color::Cyan };
                lines.push(Line::from(Span::styled(
                    who,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )));
                for w in wrap(&m.body, inner_w) {
                    lines.push(Line::from(w));
                }
                lines.push(Line::from(""));
            }
            // A pending Personal Agent draft (awaiting approval) is offered as a reply seed.
            if let Some(body) = d.draft_body() {
                lines.push(Line::from(Span::styled(
                    t(Msg::DraftPrefix),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )));
                for w in wrap(body, inner_w) {
                    lines.push(Line::from(Span::styled(
                        w,
                        Style::default().fg(Color::Magenta),
                    )));
                }
            }
        }
    }

    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = app.conv_scroll.min(max_scroll);
    let start = total.saturating_sub(inner_h + scroll);
    let view: Vec<Line> = lines.into_iter().skip(start).take(inner_h).collect();

    // The pane title carries the conversation's title (the room / sender).
    let title = match &app.conv_detail {
        Some(d) if !d.title.is_empty() => format!("{}· {} ", t(Msg::PaneThread), d.title),
        _ => t(Msg::PaneThread),
    };
    let p = Paragraph::new(view).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style(app, false)),
    );
    f.render_widget(p, area);
}

fn draw_reply(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.inbox_focus == InboxFocus::Composer;
    let can_reply = app
        .conv_detail
        .as_ref()
        .map(|d| d.can_reply)
        .unwrap_or(false);
    let mut text = app.reply_input.clone();
    if focused {
        text.push('▏');
    }
    let border = if can_reply {
        border_style(app, focused)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let p = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::ReplyComposer))
            .border_style(border),
    );
    f.render_widget(p, area);
}

fn draw_models_popup(f: &mut Frame, app: &App) {
    let area = centered(60, 70, f.area());
    f.render_widget(Clear, area);

    let mut items: Vec<ListItem> = vec![ListItem::new(t(Msg::ModelDefaultRow))];
    for m in &app.models {
        let tags = if m.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", m.tags.join(","))
        };
        items.push(ListItem::new(format!(
            "{} · {}{}",
            m.model, m.provider_label, tags
        )));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t(Msg::ModelPopupTitle))
                .border_style(Style::default().fg(accent(app))),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

    let mut state = ListState::default();
    state.select(Some(app.sel_model_row.min(app.models.len())));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_security_popup(f: &mut Frame, app: &App) {
    use crate::app::SECURITY_MODES;
    let area = centered(50, 40, f.area());
    f.render_widget(Clear, area);

    let mut items: Vec<ListItem> = vec![ListItem::new(t(Msg::SecurityDefaultRow))];
    for m in SECURITY_MODES {
        items.push(ListItem::new(t(Msg::SecurityLabel(m))));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t(Msg::SecurityPopupTitle))
                .border_style(Style::default().fg(accent(app))),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

    let mut state = ListState::default();
    state.select(Some(app.sel_security_row.min(SECURITY_MODES.len())));
    f.render_stateful_widget(list, area, &mut state);
}

/// Pick the turn a fork or rewind addresses.
///
/// Only the user's own earlier turns are offered — see `App::message_targets`. The title says
/// which of the two is about to happen, because rewind is not undoable and the two rows look
/// identical otherwise.
fn draw_message_picker(f: &mut Frame, app: &App) {
    let area = centered(76, 70, f.area());
    f.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    let title = match app.msg_action {
        crate::app::MsgAction::Fork => t(Msg::ForkPickTitle),
        crate::app::MsgAction::Rewind => t(Msg::RewindPickTitle),
    };
    // Rewind discards; the border says so before the user commits to a row.
    let tone = match app.msg_action {
        crate::app::MsgAction::Fork => accent(app),
        crate::app::MsgAction::Rewind => Color::Yellow,
    };
    f.render_widget(
        Paragraph::new(format!("🔍 {}▏", app.msg_pick.query)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(tone)),
        ),
        rows[0],
    );

    let targets = app.message_targets();
    let items: Vec<ListItem> = if targets.is_empty() {
        vec![ListItem::new(Span::styled(
            t(Msg::SessionsNoMatch),
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        targets
            .iter()
            .enumerate()
            .map(|(row, &i)| {
                let m = &app.messages[i];
                let one_line = m.text.split('\n').next().unwrap_or("").trim();
                let style = if row == app.msg_pick.cursor {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:>3}  ", i + 1),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(one_line.chars().take(100).collect::<String>()),
                ]))
                .style(style)
            })
            .collect()
    };
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style(app, false)),
        ),
        rows[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            t(Msg::MessagePickHint),
            Style::default().fg(Color::DarkGray),
        )),
        rows[2],
    );
}

/// The skills picker: what the agent may reach next turn, and a space bar to change it.
fn draw_skills_popup(f: &mut Frame, app: &App) {
    let area = centered(72, 70, f.area());
    f.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    let search = Paragraph::new(format!("🔍 {}▏", app.skill_pick.query)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::SkillsPopupTitle))
            .border_style(Style::default().fg(accent(app))),
    );
    f.render_widget(search, rows[0]);

    let skills = app.skills.as_deref().unwrap_or(&[]);
    let results = app.skill_results();
    let items: Vec<ListItem> = if results.is_empty() {
        let empty = if app.skills.is_none() {
            t(Msg::SkillsLoading)
        } else if skills.is_empty() {
            t(Msg::SkillsEmpty)
        } else {
            t(Msg::SessionsNoMatch)
        };
        vec![ListItem::new(Span::styled(
            empty,
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        results
            .iter()
            .enumerate()
            .map(|(row, &i)| {
                let s = &skills[i];
                let mark = if s.enabled { "[x]" } else { "[ ]" };
                let mut spans = vec![
                    Span::styled(
                        format!("{mark} "),
                        Style::default().fg(if s.enabled {
                            accent(app)
                        } else {
                            Color::DarkGray
                        }),
                    ),
                    Span::styled(
                        s.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ];
                // A stale skill is one the curator is about to archive; saying so here is the
                // only warning the user gets before it disappears from the preamble.
                if s.lifecycle_state == "stale" {
                    spans.push(Span::styled("  stale", Style::default().fg(Color::Yellow)));
                }
                if s.pinned {
                    spans.push(Span::styled("  pin", Style::default().fg(Color::DarkGray)));
                }
                if s.adopted {
                    spans.push(Span::styled("  ↗", Style::default().fg(Color::DarkGray)));
                }
                if !s.description.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", s.description),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                let style = if row == app.skill_pick.cursor {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(spans)).style(style)
            })
            .collect()
    };
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style(app, false)),
        ),
        rows[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            t(Msg::SkillsHint),
            Style::default().fg(Color::DarkGray),
        )),
        rows[2],
    );
}

fn draw_agents_popup(f: &mut Frame, app: &App) {
    let area = centered(72, 70, f.area());
    f.render_widget(Clear, area);

    // Search box on top, the filtered agents list below.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    let search = Paragraph::new(format!("🔍 {}▏", app.agent_query)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::AgentsPopupTitle))
            .border_style(Style::default().fg(accent(app))),
    );
    f.render_widget(search, rows[0]);

    let agents = app.current_subagents();
    let results = app.agent_results();
    let items: Vec<ListItem> = if results.is_empty() {
        let empty = if agents.is_empty() {
            t(Msg::AgentsEmpty)
        } else {
            t(Msg::SessionsNoMatch)
        };
        vec![ListItem::new(Span::styled(
            empty,
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        results
            .iter()
            .map(|&i| {
                let a = &agents[i];
                let (icon, color) = match a.status.as_str() {
                    "running" => (SPINNER[app.spinner % SPINNER.len()], Color::Yellow),
                    "completed" => ("✓", Color::Green),
                    "failed" => ("✗", Color::Red),
                    _ => ("•", Color::DarkGray),
                };
                let mut head = vec![
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::styled(
                        t(Msg::AgentKind(&a.kind)),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ];
                if a.background {
                    head.push(Span::styled(" [BG]", Style::default().fg(Color::Magenta)));
                }
                if !a.label.is_empty() {
                    head.push(Span::raw(format!(" · {}", a.label)));
                }

                // Secondary line: status + token/tool/cost tallies, or the failure reason.
                let mut meta = format!("    {}", t(Msg::AgentStatus(&a.status)));
                if a.input_tokens > 0 || a.output_tokens > 0 {
                    meta.push_str(&format!(" · {}↑ {}↓", a.input_tokens, a.output_tokens));
                }
                if a.tool_calls > 0 {
                    meta.push_str(&format!(" · {} Tools", a.tool_calls));
                }
                if let Some(c) = a.cost_usd {
                    meta.push_str(&format!(" · ${c:.4}"));
                }
                if let Some(e) = a.error.as_deref().filter(|e| !e.is_empty()) {
                    meta.push_str(&format!(" · {}", truncate(e, 48)));
                }

                ListItem::new(vec![
                    Line::from(head),
                    Line::from(Span::styled(meta, Style::default().fg(Color::DarkGray))),
                ])
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent(app))),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

    let mut state = ListState::default();
    if !results.is_empty() {
        state.select(Some(app.sel_agent.min(results.len() - 1)));
    }
    f.render_stateful_widget(list, rows[1], &mut state);
}

/// A compact glyph per integration-group kind (mirrors the web's per-kind icon set).
fn int_kind_glyph(kind: &str) -> &'static str {
    match kind {
        "builtin" => "✦",
        "web" => "🌐",
        "device" => "🖥",
        "mcp" => "🔌",
        _ => "🧩",
    }
}

/// The per-chat integrations picker (F8): a search box over a flat list of group headers and
/// their tools. Each tool is a checkbox into the chat's `disabled_tools` deny-list; a header
/// shows the enabled/total count and toggles the whole group.
fn draw_integrations_popup(f: &mut Frame, app: &App) {
    let area = centered(66, 75, f.area());
    f.render_widget(Clear, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let search = Paragraph::new(format!("🔍 {}▏", app.int_filter.query)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::IntegrationsPopupTitle))
            .border_style(Style::default().fg(accent(app))),
    );
    f.render_widget(search, rows[0]);

    let int_rows = app.integration_rows();
    let items: Vec<ListItem> = if int_rows.is_empty() {
        let empty = if app.integrations.is_empty() {
            t(Msg::IntegrationsEmpty)
        } else {
            t(Msg::SessionsNoMatch)
        };
        vec![ListItem::new(Span::styled(
            empty,
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        int_rows
            .iter()
            .map(|row| match *row {
                IntRow::Header(gi) => {
                    let g = &app.integrations[gi];
                    let glyph = int_kind_glyph(&g.kind);
                    let count = if g.tools.is_empty() {
                        "—".to_string()
                    } else {
                        let on = g.tools.iter().filter(|tn| app.tool_enabled(tn)).count();
                        format!("{on}/{}", g.tools.len())
                    };
                    let label = app.group_label(g);
                    let mut spans = vec![
                        Span::styled(format!("{glyph} "), Style::default().fg(accent(app))),
                        Span::styled(label.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    ];
                    // The manifest type (e.g. "OpenProject") as a dim caption, when it adds info.
                    if let Some(sub) = g
                        .sub_label
                        .as_deref()
                        .filter(|s| !s.is_empty() && *s != label)
                    {
                        spans.push(Span::styled(
                            format!(" · {sub}"),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    spans.push(Span::styled(
                        format!("  {count}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                    if !g.tags.is_empty() {
                        spans.push(Span::styled(
                            format!("  [{}]", g.tags.join(",")),
                            Style::default().fg(Color::Magenta),
                        ));
                    }
                    ListItem::new(Line::from(spans))
                }
                IntRow::Tool(gi, ti) => {
                    let name = &app.integrations[gi].tools[ti];
                    let on = app.tool_enabled(name);
                    let (mark, mark_color) = if on {
                        ("[x]", Color::Green)
                    } else {
                        ("[ ]", Color::DarkGray)
                    };
                    let name_color = if on { Color::White } else { Color::DarkGray };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("    {mark} "), Style::default().fg(mark_color)),
                        Span::styled(bare_tool(name).to_string(), Style::default().fg(name_color)),
                    ]))
                }
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t(Msg::IntegrationsHint))
                .border_style(Style::default().fg(accent(app))),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

    let mut state = ListState::default();
    if !int_rows.is_empty() {
        state.select(Some(app.int_filter.cursor.min(int_rows.len() - 1)));
    }
    f.render_stateful_widget(list, rows[1], &mut state);
}

/// The per-chat memory-access picker (F9): radio mode rows (default/full/none/scoped) and, when
/// scoped, the domain + source checkboxes. Rendered as manual lines (like the question card) so
/// non-selectable section headers can sit between the navigable rows.
fn draw_memory_popup(f: &mut Frame, app: &App) {
    let area = centered(58, 70, f.area());
    f.render_widget(Clear, area);

    let rows = app.mem_rows();
    let mut lines: Vec<Line> = Vec::new();
    let mut section: Option<u8> = None;
    for (idx, row) in rows.iter().enumerate() {
        // Emit a dim section header when the row's section changes.
        let (sec, header) = match row {
            MemRow::Mode(_) => (0, Msg::MemSectionModes),
            MemRow::Domain(_) => (1, Msg::MemSectionDomains),
            MemRow::Source(_) => (2, Msg::MemSectionSources),
        };
        if section != Some(sec) {
            if idx != 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                t(header),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )));
            section = Some(sec);
        }

        let (mark, mark_on, label) = match *row {
            MemRow::Mode(m) => {
                let on = app.mem_mode_index() == m;
                let key = ["default", "full", "none", "scoped"][m];
                (if on { "(•)" } else { "( )" }, on, t(Msg::MemMode(key)))
            }
            MemRow::Domain(i) => {
                let on = app.mem_domain_on(i);
                (
                    if on { "[x]" } else { "[ ]" },
                    on,
                    t(Msg::MemDomain(crate::app::MEM_DOMAINS[i])),
                )
            }
            MemRow::Source(i) => {
                let on = app.mem_source_on(i);
                (
                    if on { "[x]" } else { "[ ]" },
                    on,
                    t(Msg::MemSource(crate::app::MEM_SOURCES[i])),
                )
            }
        };
        let cursor = idx == app.mem_sel;
        let prefix = if cursor { "› " } else { "  " };
        let style = if cursor {
            Style::default().fg(Color::Black).bg(accent(app))
        } else if mark_on {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{mark} {label}"),
            style,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t(Msg::MemoryHint),
        Style::default().fg(Color::DarkGray),
    )));

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::MemoryPopupTitle))
            .border_style(Style::default().fg(accent(app))),
    );
    f.render_widget(p, area);
}

fn draw_attach_popup(f: &mut Frame, app: &App) {
    let area = centered(70, 40, f.area());
    f.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("> {}▏", app.attach_input),
            Style::default().fg(Color::Green),
        )),
        Line::from(""),
    ];
    // Already-staged attachments, so the user sees what will be sent.
    for a in &app.attachments {
        lines.push(Line::from(Span::styled(
            format!("📎 {} ({})", a.filename, a.media_type),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::AttachTitle))
            .border_style(Style::default().fg(accent(app))),
    );
    f.render_widget(p, area);
}

fn draw_help_popup(f: &mut Frame, app: &App) {
    let area = centered(60, 70, f.area());
    f.render_widget(Clear, area);
    let lines: Vec<Line> = i18n::help_lines().into_iter().map(Line::from).collect();
    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(t(Msg::HelpTitle))
                .border_style(Style::default().fg(accent(app))),
        )
        .alignment(Alignment::Left);
    f.render_widget(p, area);
}

fn draw_approval_popup(f: &mut Frame, app: &App) {
    let Some(card) = app.approvals.front() else {
        return;
    };
    let area = centered(70, 50, f.area());
    f.render_widget(Clear, area);
    let inner_w = area.width.saturating_sub(2).max(1) as usize;

    let mut lines = Vec::new();
    // When several approvals are queued, show "1/3" so the burst is visible.
    if app.approvals.len() > 1 {
        lines.push(Line::from(Span::styled(
            t(Msg::QuestionProgress(1, app.approvals.len())),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.extend([
        Line::from(Span::styled(
            t(Msg::ApprovalDevice(&card.device_name)),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            t(Msg::ApprovalTool(&card.tool)),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ]);
    for w in wrap(&card.command, inner_w.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(
            format!("  {w}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));
    let labels = [
        t(Msg::ApprovalAllow),
        t(Msg::ApprovalRemember),
        t(Msg::ApprovalReject),
    ];
    for (i, label) in labels.iter().enumerate() {
        let selected = card.sel == i;
        let prefix = if selected { "› " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(accent(app))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(format!("{prefix}{label}"), style)));
    }

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::ApprovalTitle))
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(p, area);
}

fn draw_question_popup(f: &mut Frame, app: &App) {
    let Some(card) = &app.question else { return };
    let Some(sub) = card.subs.get(card.q_idx) else {
        return;
    };
    let area = centered(70, 65, f.area());
    f.render_widget(Clear, area);
    let inner_w = area.width.saturating_sub(2).max(1) as usize;

    let mut lines: Vec<Line> = Vec::new();
    // Progress across sub-questions (multi-question cards show "Question i/n").
    if card.subs.len() > 1 {
        lines.push(Line::from(Span::styled(
            t(Msg::QuestionProgress(card.q_idx + 1, card.subs.len())),
            Style::default().fg(Color::DarkGray),
        )));
    }
    for w in wrap(&sub.question, inner_w) {
        lines.push(Line::from(Span::styled(
            w,
            Style::default().add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));

    // Options: [x]/[ ] for multi-select, (•)/( ) for single-select; the cursor row is highlighted.
    for (i, opt) in sub.options.iter().enumerate() {
        let on = sub.selected.get(i).copied().unwrap_or(false);
        let mark = match (sub.multi_select, on) {
            (true, true) => "[x] ",
            (true, false) => "[ ] ",
            (false, true) => "(•) ",
            (false, false) => "( ) ",
        };
        let cursor = card.opt_idx == i;
        let prefix = if cursor { "› " } else { "  " };
        let style = if cursor {
            Style::default().fg(Color::Black).bg(accent(app))
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{mark}{opt}"),
            style,
        )));
    }

    if sub.allow_custom {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            t(Msg::QuestionCustom(&sub.custom)),
            Style::default().fg(Color::Green),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t(Msg::QuestionHint),
        Style::default().fg(Color::DarkGray),
    )));

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t(Msg::QuestionTitle))
            .border_style(Style::default().fg(accent(app))),
    );
    f.render_widget(p, area);
}

// ── small helpers ───────────────────────────────────────────────────────────

fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

/// Strip ANSI/VT escape sequences and other control characters (keep `\n`; tabs → space)
/// from arbitrary model/tool output. Stray `\r`/escape codes otherwise desync the terminal
/// and leave stale artifacts after scrolling.
fn sanitize(s: &str) -> String {
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
                // OSC: ESC ] … BEL or ST
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
                _ => {}
            },
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            // Drop other C0 controls (incl. CR) and DEL.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            c => out.push(c),
        }
    }
    out
}

/// Word-wrap, preserving explicit newlines. Width is in columns (chars, approximate).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for segment in text.split('\n') {
        if segment.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in segment.split(' ') {
            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            }
            // Hard-break a single word longer than the width.
            while line.chars().count() > width {
                let cut: String = line.chars().take(width).collect();
                let rest: String = line.chars().skip(width).collect();
                out.push(cut);
                line = rest;
            }
        }
        out.push(line);
    }
    out
}

fn truncate(s: &str, width: usize) -> String {
    let width = width.max(1);
    if s.chars().count() <= width {
        return s.to_string();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_to_width_and_keeps_newlines() {
        let out = wrap("eins zwei drei", 8);
        assert_eq!(out, vec!["eins", "zwei", "drei"]);
        let out = wrap("a\n\nb", 10);
        assert_eq!(out, vec!["a", "", "b"]);
    }

    #[test]
    fn hard_breaks_overlong_words() {
        let out = wrap("aaaaaaaa", 3);
        assert_eq!(out, vec!["aaa", "aaa", "aa"]);
    }

    #[test]
    fn markdown_body_gets_inline_styles() {
        // The crate is wired up and emits styled spans (here: a bold run).
        let text = tui_markdown::from_str("**bold** plain `code`");
        let bold = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("bold"))
            .expect("a span containing 'bold'");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn bare_tool_strips_device_prefix() {
        assert_eq!(bare_tool("dev_ab12_write_file"), "write_file");
        assert_eq!(bare_tool("dev_ab12_run_command"), "run_command");
        assert_eq!(bare_tool("todowrite"), "todowrite");
        assert_eq!(bare_tool("search_documents"), "search_documents");
    }

    #[test]
    fn primary_arg_picks_informative_key() {
        assert_eq!(
            primary_arg(r#"{"command":"ls -la","cwd":"/tmp"}"#).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            primary_arg(r#"{"path":"src/main.rs","content":"…"}"#).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(primary_arg(r#"{"items":[]}"#), None);
        assert_eq!(primary_arg("not json"), None);
    }

    #[test]
    fn tool_error_detection() {
        assert!(tool_looks_error("[error] boom"));
        assert!(tool_looks_error("Traceback (most recent call last)"));
        assert!(tool_looks_error("[blocked by command policy]"));
        assert!(!tool_looks_error("wrote 3 files"));
    }

    #[test]
    fn diff_from_args_reads_patch() {
        let a = r#"{"changes":"@@ -1 +1 @@\n-old\n+new"}"#;
        assert!(diff_from_args(a).unwrap().contains("+new"));
        assert_eq!(diff_from_args(r#"{"path":"x"}"#), None);
    }

    #[test]
    fn sanitize_strips_control_and_ansi() {
        assert_eq!(sanitize("a\rb"), "ab"); // CR dropped (the artifact culprit)
        assert_eq!(sanitize("x\x1b[31my\x1b[0m"), "xy"); // CSI colour codes
        assert_eq!(sanitize("p\x1b]0;title\x07q"), "pq"); // OSC title
        assert_eq!(sanitize("line1\nline2\tend"), "line1\nline2 end"); // keep \n, tab→space
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hallo welt", 6), "hallo…");
        assert_eq!(truncate("kurz", 10), "kurz");
    }

    #[test]
    fn fmt_k_scales_thousands_and_millions() {
        assert_eq!(fmt_k(0), "0");
        assert_eq!(fmt_k(940), "940");
        assert_eq!(fmt_k(12_345), "12.3k");
        assert_eq!(fmt_k(1_200_000), "1.2M");
        assert_eq!(fmt_k(-5), "0"); // clamps negatives
    }

    #[test]
    fn fmt_elapsed_is_minutes_seconds() {
        assert_eq!(fmt_elapsed(std::time::Duration::from_secs(9)), "0:09");
        assert_eq!(fmt_elapsed(std::time::Duration::from_secs(75)), "1:15");
        assert_eq!(fmt_elapsed(std::time::Duration::from_secs(724)), "12:04");
    }

    #[test]
    fn ctx_color_bands_match_web_meter() {
        assert_eq!(ctx_color(10), Color::Green);
        assert_eq!(ctx_color(60), Color::Yellow);
        assert_eq!(ctx_color(80), Color::Rgb(255, 165, 0));
        assert_eq!(ctx_color(95), Color::Red);
    }
}

/// Snapshot tests for the render helpers (personal-agent-org/personal-agent#126).
///
/// Everything here turns data into `Vec<Line>`, and until now nothing compared the result to
/// anything. A change to wrapping, to how a tool call is summarised, or to the diff colouring
/// was invisible in review and only showed up in a terminal, if someone happened to look.
///
/// The helpers are snapshotted rather than the whole `draw()` on purpose: they take plain data
/// and no `App`, so a test says what it is about instead of assembling seventy-two fields of
/// application state to get at one line of output.
#[cfg(test)]
mod message_target_tests {
    use crate::app::{App, MsgAction, UiMessage};

    fn msg(id: &str, role: &str, text: &str) -> UiMessage {
        UiMessage {
            id: id.into(),
            run_id: None,
            revertable: false,
            role: role.into(),
            text: text.into(),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        }
    }

    fn app(msgs: Vec<UiMessage>) -> App {
        App::for_test(msgs)
    }

    #[test]
    fn only_the_users_own_turns_are_offered() {
        // Fork and rewind address a point the user wrote. Offering an assistant turn would ask
        // the server to cut the conversation in a place it does not accept.
        let a = app(vec![
            msg("1", "user", "erste"),
            msg("2", "assistant", "antwort"),
            msg("3", "user", "zweite"),
            msg("4", "assistant", "antwort"),
        ]);
        assert_eq!(a.message_targets(), vec![0, 2]);
    }

    #[test]
    fn the_newest_turn_is_never_a_target() {
        // Rewinding to the last message removes nothing and forking there copies the whole
        // chat -- both are a no-op dressed up as an action. Same rule as the web app's
        // canRewind.
        let a = app(vec![msg("1", "user", "einzige")]);
        assert!(a.message_targets().is_empty());

        let a = app(vec![msg("1", "user", "erste"), msg("2", "user", "letzte")]);
        assert_eq!(a.message_targets(), vec![0]);
    }

    #[test]
    fn a_turn_the_server_has_not_seen_is_not_a_target() {
        // A locally-created pending turn has no server id; sending an empty one would be a
        // request the backend cannot resolve.
        let a = app(vec![
            msg("", "user", "noch nicht gesendet"),
            msg("2", "user", "danach"),
            msg("3", "assistant", "antwort"),
        ]);
        assert_eq!(a.message_targets(), vec![1]);
    }

    #[test]
    fn the_filter_narrows_the_targets() {
        let mut a = app(vec![
            msg("1", "user", "Deployment prüfen"),
            msg("2", "user", "Tests laufen lassen"),
            msg("3", "assistant", "ok"),
        ]);
        a.msg_pick.query = "deploy".into();
        assert_eq!(a.message_targets(), vec![0]);
    }

    #[test]
    fn fork_and_rewind_are_distinct_actions() {
        // They differ only in consequence, so nothing in the code should treat them as one.
        assert_ne!(MsgAction::Fork, MsgAction::Rewind);
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::*;
    use crate::app::{App, UiMessage};

    fn settled(text: &str) -> UiMessage {
        UiMessage {
            id: String::new(),
            run_id: None,
            revertable: false,
            role: "assistant".into(),
            text: text.into(),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        }
    }

    fn streaming(text: &str) -> UiMessage {
        UiMessage {
            pending: true,
            ..settled(text)
        }
    }

    fn with(messages: Vec<UiMessage>) -> App {
        App::for_test(messages)
    }

    #[test]
    fn a_settled_transcript_is_finished_to_the_end() {
        let (lines, finished) = transcript_lines(&with(vec![settled("eins"), settled("zwei")]), 40);
        assert_eq!(finished, lines.len());
    }

    #[test]
    fn a_streaming_turn_holds_back_its_last_line() {
        // The rule the whole scrollback model rests on: what is printed can never be revised,
        // so the line still growing must not be printed.
        let (lines, finished) = transcript_lines(&with(vec![settled("alt"), streaming("neu")]), 40);
        assert!(
            finished < lines.len(),
            "a growing turn was reported as finished"
        );
        // Everything from the settled turn is still finished.
        assert!(finished >= 2);
    }

    #[test]
    fn an_empty_transcript_finishes_nothing() {
        let (lines, finished) = transcript_lines(&with(Vec::new()), 40);
        assert!(lines.is_empty());
        assert_eq!(finished, 0);
    }

    #[test]
    fn finished_never_exceeds_the_lines_produced() {
        // commit_transcript indexes with this number; overshooting it would panic.
        for msgs in [
            vec![streaming("")],
            vec![settled("")],
            vec![streaming("a\nb\nc")],
        ] {
            let (lines, finished) = transcript_lines(&with(msgs), 40);
            assert!(finished <= lines.len());
        }
    }
}

#[cfg(test)]
mod render_snapshots {
    use super::*;
    use crate::app::{UiMessage, UiTool};

    /// Lines as they would reach the terminal: text plus the modifiers that carry meaning.
    /// Colours are left out — they are a theme decision and would make every snapshot churn
    /// the day an accent changes.
    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| {
                        let m = s.style.add_modifier;
                        let mark = if m.contains(Modifier::BOLD) {
                            "*"
                        } else if m.contains(Modifier::ITALIC) {
                            "/"
                        } else if m.contains(Modifier::DIM) {
                            "·"
                        } else {
                            ""
                        };
                        format!("{mark}{}{mark}", s.content)
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool(name: &str, args: &str, result: Option<&str>) -> UiTool {
        UiTool {
            id: "t1".into(),
            name: name.into(),
            args: args.into(),
            result: result.map(str::to_string),
        }
    }

    fn msg(role: &str, text: &str) -> UiMessage {
        UiMessage {
            id: String::new(),
            run_id: None,
            revertable: false,
            role: role.into(),
            text: text.into(),
            thinking: String::new(),
            tools: Vec::new(),
            usage: None,
            pending: false,
        }
    }

    #[test]
    fn user_message() {
        let mut out = Vec::new();
        render_message(&mut out, &msg("user", "Wie geht es dem Deployment?"), 60, 0);
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn assistant_message_with_markdown() {
        let mut out = Vec::new();
        let m = msg(
            "assistant",
            "Der Stand ist **grün**.\n\n- Backend läuft\n- Frontend läuft\n\n`docker ps` zeigt alles.",
        );
        render_message(&mut out, &m, 60, 0);
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn long_words_break_rather_than_overflow() {
        let mut out = Vec::new();
        render_message(&mut out, &msg("user", &"a".repeat(90)), 40, 0);
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn a_running_tool_shows_the_spinner() {
        let mut out = Vec::new();
        render_tool(
            &mut out,
            &tool("web_search", r#"{"query":"ratatui"}"#, None),
            60,
            0,
        );
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn a_finished_tool_shows_its_result() {
        let mut out = Vec::new();
        let t = tool(
            "read_file",
            r#"{"path":"src/main.rs"}"#,
            Some("fn main() {}"),
        );
        render_tool(&mut out, &t, 60, 0);
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn a_failed_tool_is_told_apart_from_a_finished_one() {
        // tool_looks_error() decides this from the result text alone, so it is worth pinning:
        // a change to that heuristic silently restyles every error in the transcript.
        let t = tool(
            "run_command",
            r#"{"command":"false"}"#,
            Some("error: exit status 1"),
        );
        let mut out = Vec::new();
        render_tool(&mut out, &t, 60, 0);
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn todos_render_as_a_checklist() {
        let args = r#"{"items":[
            {"content":"Snapshot-Tests","status":"completed"},
            {"content":"Clipboard","status":"in_progress"},
            {"content":"Skills","status":"pending"}]}"#;
        let mut out = Vec::new();
        render_todos(&mut out, args, 60);
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn a_diff_marks_added_and_removed_lines() {
        let diff =
            "--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new();\n }";
        let mut out = Vec::new();
        render_diff(&mut out, diff, 60);
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn a_long_result_is_previewed_not_dumped() {
        let mut out = Vec::new();
        render_result_preview(
            &mut out,
            &(1..=40)
                .map(|i| format!("Zeile {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            60,
        );
        insta::assert_snapshot!(plain(&out));
    }

    #[test]
    fn wrapping_is_stable_at_the_narrowest_useful_width() {
        // A pane can get very narrow before the layout gives up; nothing should panic or
        // produce empty lines forever.
        insta::assert_snapshot!(wrap("die quelle der wahrheit", 8).join("\n"));
    }
}
