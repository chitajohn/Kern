//! `kern tui` — the interactive terminal dashboard (SPEC.md §16, ARCHITECTURE.md §38).
//!
//! The TUI is still a *client*: it speaks the daemon's HTTP API exactly like
//! every other `kern` command and never touches SQLite. It polls a snapshot
//! (agents, pending approvals, usage, tools) plus the selected agent's events,
//! and renders them into a persistent layout:
//!
//! ```text
//! ┌ KERN ────────────────────────────────────────────────────────────────┐
//! │  logo        │ 1 Overview  2 Events  3 Permissions  4 Tools           │
//! │  ◤ face ◢    │                                                        │
//! ├ AGENTS (3) ──┤  selected agent / event stream / approvals / catalog   │
//! │ ● researcher │                                                        │
//! │ ◌ writer     │                                                        │
//! └──────────────┴────────────────────────────────────────────────────────┘
//!  [↑↓] move  [1-4] tab  [m]essage  [p]ause  [t]erminate  [q]uit  [?] help
//! ```
//!
//! Design rules that matter for a control surface:
//! - **Read-only by default.** Every mutating key is explicit and the
//!   destructive one (`t`, terminate) requires a second confirmation.
//! - **Never lies about the daemon.** A failed poll flips the header status to
//!   `offline` and keeps the last snapshot on screen with a visible warning,
//!   rather than silently showing stale data as live.
//! - **The terminal is always restored.** Raw mode is left on the success path,
//!   on errors, and through a panic hook.
//! - **No hidden modes.** The footer always lists the keys that apply right
//!   now; `?` opens the full map with the logo.

use std::io::{IsTerminal, Stdout};
use std::time::Duration;

use chrono::Utc;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Tabs};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::client::Client;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const TEAL: Color = Color::Rgb(64, 224, 208);
const VISOR: Color = Color::Rgb(22, 96, 92);
const EYE: Color = Color::Rgb(210, 255, 250);
const DIM: Color = Color::Rgb(120, 130, 135);
const OK: Color = Color::Rgb(110, 220, 130);
const WARN: Color = Color::Rgb(240, 200, 90);
const BAD: Color = Color::Rgb(240, 110, 110);

// ---------------------------------------------------------------------------
// Logo art
// ---------------------------------------------------------------------------

/// The Kern face, 13 columns wide (`█` body, `▓` visor, `█` eyes inside it).
/// Same artwork as the CLI banner (`branding::logo`), minus the ANSI escapes so
/// it can be styled per cell.
const LOGO: [&str; 11] = [
    "      ████",
    "     ██████",
    "    ████████",
    "   ██████████",
    "   ██▓▓▓▓▓▓██",
    "   ██▓▓█▓█▓██",
    "   ██▓▓▓▓▓▓██",
    "    ████████",
    "     ██████",
    "     ██    ██",
    "     ██    ██",
];

/// Render logo rows as styled lines: body teal, visor dark teal, eyes bright.
///
/// A `█` inside a visor row (not one of the two frame blocks on either side) is
/// an eye — that is exactly how the artwork distinguishes them.
fn logo_lines(rows: &[&str]) -> Vec<Line<'static>> {
    rows.iter()
        .map(|row| {
            let first = row.find(|c: char| c != ' ').unwrap_or(0);
            let last = row.trim_end().len();
            let visor_row = row.contains('▓');
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (i, ch) in row.chars().enumerate() {
                let style = if ch == ' ' {
                    Style::default()
                } else if !visor_row || i < first + 2 || i >= last.saturating_sub(2) {
                    Style::default().fg(TEAL)
                } else if ch == '▓' {
                    Style::default().fg(VISOR)
                } else {
                    Style::default().fg(EYE).add_modifier(Modifier::BOLD)
                };
                spans.push(Span::styled(ch.to_string(), style));
            }
            Line::from(spans)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Overview,
    Events,
    Permissions,
    Tools,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Overview, Tab::Events, Tab::Permissions, Tab::Tools];

    fn index(self) -> usize {
        match self {
            Tab::Overview => 0,
            Tab::Events => 1,
            Tab::Permissions => 2,
            Tab::Tools => 3,
        }
    }

    fn from_index(i: usize) -> Self {
        Self::ALL[i % Self::ALL.len()]
    }

    fn next(self) -> Self {
        Self::from_index(self.index() + 1)
    }

    fn prev(self) -> Self {
        Self::from_index((self.index() + Self::ALL.len() - 1) % Self::ALL.len())
    }

    fn titles() -> Vec<&'static str> {
        vec![" Overview ", " Events ", " Permissions ", " Tools "]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    Agents,
    Main,
}

struct AgentRow {
    id: String,
    name: String,
    state: String,
    updated: String,
    last_error: Option<String>,
}

impl AgentRow {
    fn from_value(v: &Value) -> Self {
        let text = |key: &str| v[key].as_str().unwrap_or("").to_string();
        Self {
            id: text("id"),
            name: text("name"),
            state: v["lifecycle_state"]
                .as_str()
                .or_else(|| v["state"].as_str())
                .unwrap_or("?")
                .to_string(),
            updated: text("updated_at"),
            last_error: v["last_error"].as_str().map(str::to_string),
        }
    }

    fn state_color(&self) -> Color {
        match self.state.as_str() {
            "running" => OK,
            "waiting" => WARN,
            "sleeping" => Color::Rgb(120, 180, 240),
            "paused" => Color::Rgb(120, 200, 220),
            "failed" | "terminated" => BAD,
            "completed" => DIM,
            _ => DIM,
        }
    }

    fn dot(&self) -> &'static str {
        match self.state.as_str() {
            "running" => "●",
            "waiting" => "◐",
            "sleeping" => "☾",
            "paused" => "❙❙",
            "failed" | "terminated" => "✖",
            "completed" => "✔",
            _ => "·",
        }
    }
}

struct Status {
    text: String,
    error: bool,
    ticks: u16,
}

struct MessageInput {
    agent_id: String,
    agent_name: String,
    buffer: String,
}

struct Confirm {
    agent_id: String,
    agent_name: String,
}

/// One operator command produced by a key press.
enum Action {
    Lifecycle { id: String, action: &'static str },
    Message { id: String, text: String },
    Decide { id: String, grant: bool },
}

enum AppMsg {
    Key(KeyEvent),
    Agents(Vec<Value>),
    Detail {
        agent_id: String,
        agent: Value,
    },
    Events {
        agent_id: String,
        events: Vec<Value>,
    },
    Permissions(Vec<Value>),
    Usage(Value),
    Tools(Vec<Value>),
    ActionDone(String),
    Error(String),
}

#[derive(Default)]
struct App {
    agents: Vec<AgentRow>,
    selected: usize,
    filter: String,
    filter_input: Option<String>,
    tab: Tab,
    focus: Focus,
    detail: Option<Value>,
    detail_for: Option<String>,
    events: Vec<Value>,
    event_cursor: i64,
    follow: bool,
    event_scroll: u16,
    permissions: Vec<Value>,
    perm_selected: usize,
    usage: Value,
    tools: Vec<Value>,
    online: bool,
    status: Option<Status>,
    input: Option<MessageInput>,
    confirm: Option<Confirm>,
    help: bool,
    fetching: bool,
    quit: bool,
    daemon: String,
}

impl App {
    fn new(daemon: String) -> Self {
        Self {
            agents: Vec::new(),
            selected: 0,
            filter: String::new(),
            filter_input: None,
            tab: Tab::Overview,
            focus: Focus::Agents,
            detail: None,
            detail_for: None,
            events: Vec::new(),
            event_cursor: 0,
            follow: true,
            event_scroll: 0,
            permissions: Vec::new(),
            perm_selected: 0,
            usage: Value::Null,
            tools: Vec::new(),
            online: false,
            status: None,
            input: None,
            confirm: None,
            help: false,
            fetching: false,
            quit: false,
            daemon,
        }
    }

    /// Indices into `agents` that pass the filter, in display order.
    fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                needle.is_empty()
                    || a.name.to_lowercase().contains(&needle)
                    || a.id.to_lowercase().contains(&needle)
                    || a.state.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_agent(&self) -> Option<&AgentRow> {
        self.visible().get(self.selected).map(|i| &self.agents[*i])
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_agent().map(|a| a.id.clone())
    }

    fn selection(&self) -> Option<(String, i64)> {
        self.selected_id().map(|id| (id, self.event_cursor))
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status {
            text: text.into(),
            error,
            ticks: 6,
        });
    }

    /// Countdown for transient status lines (one tick per refresh interval).
    fn tick(&mut self) {
        if let Some(status) = &mut self.status {
            status.ticks = status.ticks.saturating_sub(1);
            if status.ticks == 0 {
                self.status = None;
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len as isize - 1) as usize;
    }

    fn select_index(&mut self, index: usize) {
        let len = self.visible().len();
        self.selected = index.min(len.saturating_sub(1));
    }

    /// Apply one message from the network or keyboard.
    fn apply(&mut self, msg: AppMsg) -> Option<Action> {
        match msg {
            AppMsg::Key(key) => return self.handle_key(key),
            AppMsg::Agents(agents) => {
                self.fetching = false;
                self.online = true;
                self.agents = agents.iter().map(AgentRow::from_value).collect();
                // Drop a selection the filter/list no longer contains.
                let len = self.visible().len();
                if self.selected >= len {
                    self.selected = len.saturating_sub(1);
                }
            }
            AppMsg::Detail { agent_id, agent } => {
                if self.selected_id().as_deref() == Some(agent_id.as_str()) {
                    self.detail = Some(agent);
                    self.detail_for = Some(agent_id);
                }
            }
            AppMsg::Events { agent_id, events } => {
                if self.selected_id().as_deref() != Some(agent_id.as_str()) {
                    return None;
                }
                if let Some(last) = events.iter().filter_map(|e| e["seq"].as_i64()).max() {
                    self.event_cursor = self.event_cursor.max(last);
                }
                self.events.extend(events);
                // Bound the in-memory transcript: the TUI is a window, not an archive.
                const MAX_EVENTS: usize = 5_000;
                if self.events.len() > MAX_EVENTS {
                    let drop = self.events.len() - MAX_EVENTS;
                    self.events.drain(..drop);
                }
                if self.follow {
                    self.event_scroll = u16::MAX;
                }
            }
            AppMsg::Permissions(requests) => {
                self.permissions = requests;
                if self.perm_selected >= self.permissions.len() {
                    self.perm_selected = self.permissions.len().saturating_sub(1);
                }
            }
            AppMsg::Usage(usage) => self.usage = usage,
            AppMsg::Tools(tools) => self.tools = tools,
            AppMsg::ActionDone(text) => {
                self.set_status(text, false);
                // A lifecycle action invalidates the cached detail immediately.
                self.detail = None;
                self.detail_for = None;
            }
            AppMsg::Error(text) => {
                self.fetching = false;
                self.online = false;
                self.set_status(text, true);
            }
        }
        None
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // 1. Overlays and inputs own the keyboard while open.
        if self.help {
            self.help = false;
            return None;
        }
        if self.confirm.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let confirm = self.confirm.take().expect("checked above");
                    return Some(Action::Lifecycle {
                        id: confirm.agent_id,
                        action: "terminate",
                    });
                }
                _ => {
                    self.confirm = None;
                    return None;
                }
            }
        }
        if self.filter_input.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.filter_input = None;
                    self.filter.clear();
                    self.selected = 0;
                }
                KeyCode::Enter => {
                    if let Some(text) = self.filter_input.take() {
                        self.filter = text.trim().to_string();
                        self.selected = 0;
                    }
                }
                KeyCode::Backspace => {
                    if let Some(text) = &mut self.filter_input {
                        text.pop();
                        self.filter = text.clone();
                        self.selected = 0;
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(text) = &mut self.filter_input {
                        text.push(c);
                        self.filter = text.clone();
                        self.selected = 0;
                    }
                }
                _ => {}
            }
            return None;
        }
        if self.input.is_some() {
            match key.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Enter => {
                    let input = self.input.take().expect("checked above");
                    let text = input.buffer.trim().to_string();
                    if text.is_empty() {
                        self.set_status("empty message not sent", true);
                    } else {
                        return Some(Action::Message {
                            id: input.agent_id,
                            text,
                        });
                    }
                }
                KeyCode::Backspace => {
                    if let Some(input) = &mut self.input {
                        input.buffer.pop();
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(input) = &mut self.input {
                        if input.buffer.chars().count() < 2_000 {
                            input.buffer.push(c);
                        }
                    }
                }
                _ => {}
            }
            return None;
        }

        // 2. Global keys.
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.quit = true;
                return None;
            }
            KeyCode::Char('q') => {
                self.quit = true;
                return None;
            }
            KeyCode::Char('?') => {
                self.help = true;
                return None;
            }
            KeyCode::Tab => {
                self.tab = self.tab.next();
                return None;
            }
            KeyCode::BackTab => {
                self.tab = self.tab.prev();
                return None;
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.tab = Tab::from_index(c as usize - '1' as usize);
                return None;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.focus = Focus::Agents;
                return None;
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.focus = Focus::Main;
                return None;
            }
            KeyCode::Char('r') if !ctrl => {
                // Force a poll now; the ticker would otherwise wait.
                self.fetching = false;
                self.set_status("refreshing", false);
                return None;
            }
            _ => {}
        }

        // 3. Agent-level commands (always target the selected agent).
        match key.code {
            KeyCode::Char('/') => {
                self.filter_input = Some(self.filter.clone());
                self.focus = Focus::Agents;
                return None;
            }
            KeyCode::Char('x') => {
                self.filter.clear();
                self.selected = 0;
                return None;
            }
            KeyCode::Char('m') => {
                if let Some(agent) = self.selected_agent() {
                    self.input = Some(MessageInput {
                        agent_id: agent.id.clone(),
                        agent_name: agent.name.clone(),
                        buffer: String::new(),
                    });
                }
                return None;
            }
            KeyCode::Char('s')
            | KeyCode::Char('p')
            | KeyCode::Char('u')
            | KeyCode::Char('c')
            | KeyCode::Char('t') => {
                let agent = self.selected_agent()?;
                let id = agent.id.clone();
                let name = agent.name.clone();
                match key.code {
                    KeyCode::Char('t') => {
                        self.confirm = Some(Confirm {
                            agent_id: id,
                            agent_name: name,
                        });
                        return None;
                    }
                    KeyCode::Char('s') => {
                        self.set_status(format!("starting {name}"), false);
                        return Some(Action::Lifecycle {
                            id,
                            action: "start",
                        });
                    }
                    KeyCode::Char('p') => {
                        self.set_status(format!("pausing {name}"), false);
                        return Some(Action::Lifecycle {
                            id,
                            action: "pause",
                        });
                    }
                    KeyCode::Char('u') => {
                        self.set_status(format!("resuming {name}"), false);
                        return Some(Action::Lifecycle {
                            id,
                            action: "resume",
                        });
                    }
                    _ => {
                        self.set_status(format!("checkpointing {name}"), false);
                        return Some(Action::Lifecycle {
                            id,
                            action: "checkpoint",
                        });
                    }
                }
            }
            _ => {}
        }

        // 4. Pane navigation.
        match (self.focus, key.code) {
            (_, KeyCode::Up) | (Focus::Agents, KeyCode::Char('k')) => self.move_selection(-1),
            (_, KeyCode::Down) | (Focus::Agents, KeyCode::Char('j')) => self.move_selection(1),
            (Focus::Agents, KeyCode::Home) | (Focus::Agents, KeyCode::Char('g')) => {
                self.select_index(0)
            }
            (Focus::Agents, KeyCode::End) | (Focus::Agents, KeyCode::Char('G')) => {
                self.select_index(self.visible().len().saturating_sub(1))
            }
            (_, KeyCode::Enter) => self.focus = Focus::Main,
            (Focus::Main, KeyCode::Char('j')) => match self.tab {
                Tab::Events => {
                    self.follow = false;
                    self.event_scroll = self.event_scroll.saturating_add(1);
                }
                Tab::Permissions => {
                    let len = self.permissions.len();
                    if len > 0 {
                        self.perm_selected = (self.perm_selected + 1).min(len - 1);
                    }
                }
                _ => {}
            },
            (Focus::Main, KeyCode::Char('k')) => match self.tab {
                Tab::Events => {
                    self.follow = false;
                    self.event_scroll = self.event_scroll.saturating_sub(1);
                }
                Tab::Permissions => {
                    self.perm_selected = self.perm_selected.saturating_sub(1);
                }
                _ => {}
            },
            (Focus::Main, KeyCode::Char('G')) => match self.tab {
                Tab::Events => {
                    self.follow = true;
                    self.event_scroll = u16::MAX;
                }
                Tab::Permissions => {
                    self.perm_selected = self.permissions.len().saturating_sub(1);
                }
                _ => {}
            },
            (Focus::Main, KeyCode::Char('g')) => match self.tab {
                Tab::Events => {
                    self.follow = false;
                    self.event_scroll = 0;
                }
                Tab::Permissions => self.perm_selected = 0,
                _ => {}
            },
            (Focus::Main, KeyCode::Char('f')) => {
                self.follow = !self.follow;
                if self.follow {
                    self.event_scroll = u16::MAX;
                }
            }
            (Focus::Main, KeyCode::Char('y')) if self.tab == Tab::Permissions => {
                if let Some(id) = self.selected_permission() {
                    return Some(Action::Decide { id, grant: true });
                }
            }
            (Focus::Main, KeyCode::Char('n')) if self.tab == Tab::Permissions => {
                if let Some(id) = self.selected_permission() {
                    return Some(Action::Decide { id, grant: false });
                }
            }
            (Focus::Main, KeyCode::PageDown) if self.tab == Tab::Events => {
                self.follow = false;
                self.event_scroll = self.event_scroll.saturating_add(10);
            }
            (Focus::Main, KeyCode::PageUp) if self.tab == Tab::Events => {
                self.follow = false;
                self.event_scroll = self.event_scroll.saturating_sub(10);
            }
            _ => {}
        }
        None
    }

    fn selected_permission(&self) -> Option<String> {
        self.permissions
            .get(self.perm_selected)
            .and_then(|p| p["id"].as_str())
            .map(str::to_string)
    }
}

// ---------------------------------------------------------------------------
// Network: one snapshot per refresh
// ---------------------------------------------------------------------------

fn spawn_fetch(client: Client, selected: Option<(String, i64)>, tx: UnboundedSender<AppMsg>) {
    tokio::spawn(async move {
        match client.list_agents().await {
            Ok(agents) => {
                let _ = tx.send(AppMsg::Agents(agents));
            }
            Err(err) => {
                let _ = tx.send(AppMsg::Error(err.to_string()));
                return;
            }
        }
        if let Ok(permissions) = client.pending_permissions().await {
            let _ = tx.send(AppMsg::Permissions(permissions));
        }
        if let Ok(usage) = client.usage().await {
            let _ = tx.send(AppMsg::Usage(usage));
        }
        if let Ok(tools) = client.tools().await {
            let _ = tx.send(AppMsg::Tools(tools));
        }
        if let Some((agent_id, cursor)) = selected {
            if let Ok(events) = client.agent_events(&agent_id, cursor, 200).await {
                let _ = tx.send(AppMsg::Events {
                    agent_id: agent_id.clone(),
                    events,
                });
            }
            if let Ok(agent) = client.get_agent(&agent_id).await {
                let _ = tx.send(AppMsg::Detail { agent_id, agent });
            }
        }
    });
}

fn spawn_action(client: Client, action: Action, tx: UnboundedSender<AppMsg>) {
    tokio::spawn(async move {
        let result = match action {
            Action::Lifecycle { id, action } => client
                .lifecycle(&id, action)
                .await
                .map(|_| format!("{action} accepted")),
            Action::Message { id, text } => client
                .send_message(&id, &text)
                .await
                .map(|_| "message queued for the next safe point".to_string()),
            Action::Decide { id, grant } => client
                .decide_permission(&id, grant)
                .await
                .map(|_| format!("permission {}", if grant { "granted" } else { "denied" })),
        };
        match result {
            Ok(text) => {
                let _ = tx.send(AppMsg::ActionDone(text));
            }
            Err(err) => {
                let _ = tx.send(AppMsg::Error(err.to_string()));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(f.area());

    render_header(f, app, rows[0]);

    let narrow = f.area().width < 90;
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(if narrow { 30 } else { 40 }),
            Constraint::Min(0),
        ])
        .split(rows[1]);
    render_sidebar(f, app, columns[0]);
    render_main(f, app, columns[1]);
    render_footer(f, app, rows[2]);

    if app.help {
        render_help(f, app);
    }
    if let Some(confirm) = &app.confirm {
        render_confirm(f, confirm);
    }
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let agents = app.agents.len();
    let totals = &app.usage;
    let tokens = format!(
        "{} in / {} out",
        totals["input_tokens"].as_u64().unwrap_or(0),
        totals["output_tokens"].as_u64().unwrap_or(0)
    );
    let (dot, state) = if app.online {
        ("●", "online".to_string())
    } else {
        ("○", "offline".to_string())
    };
    let line = Line::from(vec![
        Span::styled(
            " KERN ",
            Style::default()
                .fg(Color::Black)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{} ", kern_core::version::KERN_VERSION),
            Style::default().fg(DIM),
        ),
        Span::styled(dot, Style::default().fg(if app.online { OK } else { BAD })),
        Span::raw(" "),
        Span::styled(
            state,
            Style::default().fg(if app.online { DIM } else { BAD }),
        ),
        Span::styled(format!("  {}  ", app.daemon), Style::default().fg(DIM)),
        Span::styled(
            format!("agents {agents}"),
            Style::default().fg(if agents > 0 { Color::Reset } else { DIM }),
        ),
        Span::styled(format!("  tokens {tokens}"), Style::default().fg(DIM)),
        Span::styled(
            format!("  schema v{}", kern_core::version::STORAGE_SCHEMA_VERSION),
            Style::default().fg(DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let show_logo = area.height >= 16 && area.width >= 26;
    let (logo_area, list_area) = if show_logo {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(10), Constraint::Min(0)])
            .split(area);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, area)
    };

    if let Some(logo_area) = logo_area {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(VISOR))
            .title(Span::styled(
                " kern ",
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(logo_area);
        f.render_widget(block, logo_area);

        let mut lines = logo_lines(&LOGO[2..8]);
        lines.push(Line::from(Span::styled(
            "   runtime for agents",
            Style::default().fg(DIM),
        )));
        f.render_widget(
            Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
            inner,
        );
    }

    let title = if app.filter.is_empty() {
        format!(" agents ({}) ", app.visible().len())
    } else {
        format!(" agents ({}) filter:{:?} ", app.visible().len(), app.filter)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.focus == Focus::Agents {
            TEAL
        } else {
            VISOR
        }))
        .title(Span::styled(title, Style::default().fg(TEAL)));
    let inner = block.inner(list_area);
    f.render_widget(block, list_area);

    if app.agents.is_empty() {
        let hint = Text::from(vec![
            Line::from(Span::styled(
                "no agents yet",
                Style::default().fg(DIM).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "kern run agent.yaml",
                Style::default().fg(TEAL),
            )),
        ]);
        f.render_widget(Paragraph::new(hint).alignment(Alignment::Center), inner);
        return;
    }
    if app.visible().is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled("no match", Style::default().fg(WARN))),
                Line::from(Span::styled(
                    "x clears the filter",
                    Style::default().fg(DIM),
                )),
            ]))
            .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .visible()
        .iter()
        .map(|index| {
            let agent = &app.agents[*index];
            let line = Line::from(vec![
                Span::styled(
                    format!("{} ", agent.dot()),
                    Style::default().fg(agent.state_color()),
                ),
                Span::styled(
                    agent.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", agent.state),
                    Style::default().fg(agent.state_color()),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(VISOR).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    let mut state = ratatui::widgets::ListState::default();
    state.select(if app.visible().is_empty() {
        None
    } else {
        Some(app.selected)
    });
    f.render_stateful_widget(list, inner, &mut state);
}

fn render_main(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    let tabs = Tabs::new(Tab::titles())
        .select(app.tab.index())
        .style(Style::default().fg(DIM))
        .highlight_style(
            Style::default()
                .fg(TEAL)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .divider(Span::styled("│", Style::default().fg(VISOR)));
    f.render_widget(tabs, chunks[0]);

    match app.tab {
        Tab::Overview => render_overview(f, app, chunks[1]),
        Tab::Events => render_events(f, app, chunks[1]),
        Tab::Permissions => render_permissions(f, app, chunks[1]),
        Tab::Tools => render_tools(f, app, chunks[1]),
    }
}

fn panel(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { TEAL } else { VISOR }))
        .title(Span::styled(title, Style::default().fg(TEAL)))
}

fn render_overview(f: &mut Frame, app: &App, area: Rect) {
    let Some(agent) = app.selected_agent() else {
        render_empty(f, area, "select an agent");
        return;
    };
    let block = panel(format!(" {} ", agent.name), app.focus == Focus::Main);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let detail = app
        .detail
        .as_ref()
        .filter(|_| app.detail_for.as_deref() == Some(agent.id.as_str()));
    let config = detail.map(|d| &d["config"]).unwrap_or(&Value::Null);
    let provider = config["model"]["provider"].as_str().unwrap_or("?");
    let model = config["model"]["model"].as_str().unwrap_or("?");
    let tools: Vec<String> = config["tools"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let steps = config["runtime"]["max_steps"].as_u64().unwrap_or(100);
    let memory = config["memory"]["enabled"].as_bool().unwrap_or(false);
    let backend = config["memory"]["backend"].as_str().unwrap_or("store");
    let schedule = config["schedule"]["cron"]
        .as_str()
        .or_else(|| config["schedule"]["every"].as_str());

    let agent_usage = app.usage["agents"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["agent_id"].as_str() == Some(agent.id.as_str()))
        })
        .cloned()
        .unwrap_or(Value::Null);

    let mut lines = vec![
        field("state", agent.state.clone(), agent.state_color()),
        field("id", agent.id.clone(), DIM),
        field("model", format!("{provider} / {model}"), Color::Reset),
        field(
            "tools",
            if tools.is_empty() {
                "—".to_string()
            } else {
                tools.join(", ")
            },
            Color::Reset,
        ),
        field("max steps", steps.to_string(), DIM),
        field(
            "memory",
            if memory {
                format!("enabled ({backend})")
            } else {
                "disabled".to_string()
            },
            DIM,
        ),
        field(
            "schedule",
            schedule.unwrap_or("—").to_string(),
            if schedule.is_some() { WARN } else { DIM },
        ),
        field("updated", agent.updated.clone(), DIM),
        field(
            "tokens",
            format!(
                "{} in / {} out across {} run(s)",
                agent_usage["input_tokens"].as_u64().unwrap_or(0),
                agent_usage["output_tokens"].as_u64().unwrap_or(0),
                agent_usage["executions"].as_u64().unwrap_or(0)
            ),
            Color::Reset,
        ),
    ];
    if let Some(error) = &agent.last_error {
        lines.push(Line::from(""));
        lines.push(field("last error", error.clone(), BAD));
    }
    if app.detail.is_none() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "loading detail…",
            Style::default().fg(DIM),
        )));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn field(label: &str, value: impl Into<String>, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::default().fg(DIM)),
        Span::styled(value.into(), Style::default().fg(color)),
    ])
}

fn render_events(f: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        " events  ({})  follow:{} ",
        app.events.len(),
        if app.follow { "on" } else { "off" }
    );
    let block = panel(title, app.focus == Focus::Main);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.events.is_empty() {
        let hint = if app.selected_agent().is_some() {
            "no events yet — the run has not reached its first checkpoint"
        } else {
            "select an agent to follow its events"
        };
        f.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
            inner,
        );
        return;
    }

    let width = inner.width.saturating_sub(2) as usize;
    let lines: Vec<Line> = app
        .events
        .iter()
        .map(|event| event_line(event, width))
        .collect();
    let height = inner.height as usize;
    let scroll = if app.follow {
        lines.len().saturating_sub(height) as u16
    } else {
        app.event_scroll.min(lines.len().saturating_sub(1) as u16)
    };
    f.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), inner);
}

fn event_line(event: &Value, width: usize) -> Line<'static> {
    let kind = event["kind"].as_str().unwrap_or("?");
    let seq = event["seq"].as_i64().unwrap_or(0);
    let time = event["ts"]
        .as_str()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|ts| ts.with_timezone(&Utc).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into());
    let detail = crate::table::format_event(event);
    let detail = detail
        .strip_prefix(&format!("{kind}: "))
        .unwrap_or(&detail)
        .to_string();
    let color = if kind.starts_with("tool") {
        if kind.ends_with("failed") {
            BAD
        } else {
            OK
        }
    } else if kind.starts_with("model") {
        Color::Rgb(120, 190, 240)
    } else if kind.starts_with("agent") {
        TEAL
    } else if kind.starts_with("permission") {
        WARN
    } else if kind.starts_with("checkpoint") {
        DIM
    } else {
        Color::Reset
    };
    let budget = width.saturating_sub(kind.len() + 12);
    Line::from(vec![
        Span::styled(format!("{seq:>5} "), Style::default().fg(DIM)),
        Span::styled(format!("{time} "), Style::default().fg(DIM)),
        Span::styled(format!("{kind:<22}"), Style::default().fg(color)),
        Span::raw(truncate(&detail, budget)),
    ])
}

fn render_permissions(f: &mut Frame, app: &App, area: Rect) {
    let block = panel(
        format!(" pending approvals ({}) ", app.permissions.len()),
        app.focus == Focus::Main,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.permissions.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "nothing waiting — ask rules appear here while an agent is `waiting`",
                Style::default().fg(DIM),
            )),
            inner,
        );
        return;
    }

    let header = Row::new(vec![
        Cell::from("#"),
        Cell::from("agent"),
        Cell::from("resource"),
        Cell::from("action"),
        Cell::from("requested"),
    ])
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .permissions
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let agent_name = p["agent_id"]
                .as_str()
                .and_then(|id| {
                    app.agents
                        .iter()
                        .find(|a| a.id == id)
                        .map(|a| a.name.clone())
                })
                .unwrap_or_else(|| crate::table::short_id(p["agent_id"].as_str().unwrap_or("?")));
            Row::new(vec![
                Cell::from((i + 1).to_string()),
                Cell::from(agent_name),
                Cell::from(truncate(p["resource"].as_str().unwrap_or("?"), 40)),
                Cell::from(p["action"].as_str().unwrap_or("?").to_string()),
                Cell::from(
                    p["requested_at"]
                        .as_str()
                        .map(|ts| crate::table::relative_time(ts, Utc::now()))
                        .unwrap_or_default(),
                ),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Length(16),
        Constraint::Min(12),
        Constraint::Length(10),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(VISOR).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    let mut state = ratatui::widgets::TableState::default();
    state.select(Some(app.perm_selected));
    f.render_stateful_widget(table, inner, &mut state);
}

fn render_tools(f: &mut Frame, app: &App, area: Rect) {
    let block = panel(
        format!(" tool catalog ({}) ", app.tools.len()),
        app.focus == Focus::Main,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.tools.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("loading catalog…", Style::default().fg(DIM))),
            inner,
        );
        return;
    }
    let lines: Vec<Line> = app
        .tools
        .iter()
        .map(|tool| {
            Line::from(vec![
                Span::styled(
                    format!("{:<14}", tool["name"].as_str().unwrap_or("?")),
                    Style::default().fg(TEAL),
                ),
                Span::styled(
                    format!("{:<12}", tool["permission_class"].as_str().unwrap_or("—")),
                    Style::default().fg(DIM),
                ),
                Span::raw(truncate(tool["description"].as_str().unwrap_or(""), 60)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_empty(f: &mut Frame, area: Rect, hint: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(VISOR));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines = logo_lines(&LOGO);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        hint.to_string(),
        Style::default().fg(DIM),
    )));
    f.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        inner,
    );
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let keys = match (app.focus, app.tab) {
        (Focus::Agents, _) => " [↑↓] select  [1-4] tab  [m]essage  [s]tart  [p]ause  [u]resume  [c]heckpoint  [t]erminate  [/]filter  [?]help  [q]uit",
        (Focus::Main, Tab::Events) => " [j/k] scroll  [f]ollow  [g/G] top/bottom  [h] agents  [1-4] tab  [m]essage  [?]help  [q]uit",
        (Focus::Main, Tab::Permissions) => " [j/k] select  [y] grant  [n] deny  [h] agents  [1-4] tab  [?]help  [q]uit",
        (Focus::Main, _) => " [h] agents  [1-4] tab  [m]essage  [s]tart  [p]ause  [u]resume  [c]heckpoint  [t]erminate  [?]help  [q]uit",
    };
    f.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(DIM))),
        rows[0],
    );

    if let Some(input) = &app.input {
        let line = Line::from(vec![
            Span::styled(
                format!("message → {}: ", input.agent_name),
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            ),
            Span::raw(input.buffer.clone()),
            Span::styled("▏", Style::default().fg(TEAL)),
            Span::styled("   [enter] send  [esc] cancel", Style::default().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line), rows[1]);
    } else if let Some(filter) = &app.filter_input {
        let line = Line::from(vec![
            Span::styled(
                "filter: ",
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            ),
            Span::raw(filter.clone()),
            Span::styled("▏", Style::default().fg(TEAL)),
            Span::styled("   [enter] apply  [esc] clear", Style::default().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line), rows[1]);
    } else if let Some(status) = &app.status {
        let line = Line::from(vec![
            Span::styled(
                if status.error { "✖ " } else { "✔ " },
                Style::default().fg(if status.error { BAD } else { OK }),
            ),
            Span::styled(
                status.text.clone(),
                Style::default().fg(if status.error { BAD } else { Color::Reset }),
            ),
        ]);
        f.render_widget(Paragraph::new(line), rows[1]);
    } else if !app.online {
        f.render_widget(
            Paragraph::new(Span::styled(
                "daemon unreachable — start it with `kern daemon`; the TUI retries every refresh",
                Style::default().fg(BAD),
            )),
            rows[1],
        );
    } else {
        f.render_widget(
            Paragraph::new(Span::styled(
                " live view — every key acts on the selected agent",
                Style::default().fg(DIM),
            )),
            rows[1],
        );
    }
}

fn render_help(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 74, 30);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .title(Span::styled(
            " kern ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = logo_lines(&LOGO);
    lines.push(Line::from(Span::styled(
        format!(
            "Kern {} — durable agent runtime",
            kern_core::version::KERN_VERSION
        ),
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(""));
    for (keys, what) in [
        ("↑/↓ or j/k", "move selection (agents, events, approvals)"),
        ("1–4, Tab", "Overview · Events · Permissions · Tools"),
        ("h / l", "focus the agent list / the main panel"),
        ("Enter", "focus the main panel"),
        (
            "m",
            "message the selected agent (delivered at its next safe point)",
        ),
        ("s p u c", "start · pause · resume · checkpoint"),
        ("t", "terminate (asks for confirmation)"),
        ("y / n", "grant / deny the selected approval"),
        ("/ · x", "filter agents · clear the filter"),
        ("f · g/G", "follow the event stream · jump to top/bottom"),
        ("r", "refresh now"),
        ("q", "quit (Ctrl-C also works)"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {keys:<12}"), Style::default().fg(TEAL)),
            Span::styled(what.to_string(), Style::default().fg(Color::Reset)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  daemon: {}", app.daemon),
        Style::default().fg(DIM),
    )));
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_confirm(f: &mut Frame, confirm: &Confirm) {
    let area = centered(f.area(), 54, 7);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BAD))
        .title(Span::styled(
            " terminate agent ",
            Style::default().fg(BAD).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = Text::from(vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Terminate ", Style::default().fg(Color::Reset)),
            Span::styled(
                confirm.agent_name.clone(),
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            ),
            Span::styled("? Its run stops at the next safe point.", Style::default()),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  [y] yes    [n] no",
            Style::default().fg(DIM),
        )]),
    ]);
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center), inner);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.pop();
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// Terminal lifecycle + event loop
// ---------------------------------------------------------------------------

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Run the dashboard until the operator quits.
pub async fn run(refresh: Duration) -> Result<i32, String> {
    if !std::io::stdout().is_terminal() {
        return Err(
            "`kern tui` needs an interactive terminal (use `kern ps`, `kern logs`, or \
             `kern inspect` in scripts)"
                .to_string(),
        );
    }
    let client = Client::from_env().map_err(|e| e.to_string())?;
    let daemon = client.base_url().to_string();

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let result = run_loop(
        &mut terminal,
        client,
        daemon,
        refresh.max(Duration::from_millis(250)),
    )
    .await;
    restore_terminal(&mut terminal);
    result
}

fn setup_terminal() -> Result<Tui, String> {
    enable_raw_mode().map_err(|e| format!("enable raw mode: {e}"))?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| format!("enter alternate screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("create terminal: {e}"))?;
    terminal
        .hide_cursor()
        .map_err(|e| format!("hide cursor: {e}"))?;
    let _ = terminal.clear();
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

/// Leave the terminal usable even if a render or a task panics.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        previous(info);
    }));
}

async fn run_loop(
    terminal: &mut Tui,
    client: Client,
    daemon: String,
    refresh: Duration,
) -> Result<i32, String> {
    let (tx, mut rx) = unbounded_channel::<AppMsg>();

    // Keyboard: a dedicated thread owns the blocking `read`, forwarding only
    // key presses (crossterm emits releases on Windows too).
    {
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if tx.send(AppMsg::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        });
    }

    let mut app = App::new(daemon);
    let mut ticker = tokio::time::interval(refresh);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal
            .draw(|f| render(f, &app))
            .map_err(|e| format!("draw: {e}"))?;
        if app.quit {
            break;
        }

        tokio::select! {
            maybe = rx.recv() => {
                let Some(msg) = maybe else { break };
                if let Some(action) = app.apply(msg) {
                    spawn_action(client.clone(), action, tx.clone());
                }
            }
            _ = ticker.tick() => {
                app.tick();
                if !app.fetching {
                    app.fetching = true;
                    spawn_fetch(client.clone(), app.selection(), tx.clone());
                }
            }
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn agent(id: &str, name: &str, state: &str) -> Value {
        serde_json::json!({ "id": id, "name": name, "lifecycle_state": state, "updated_at": "2026-11-03T00:00:00Z" })
    }

    fn app_with_agents() -> App {
        let mut app = App::new("http://127.0.0.1:8787".into());
        app.apply(AppMsg::Agents(vec![
            agent("id-1", "researcher", "running"),
            agent("id-2", "writer", "waiting"),
            agent("id-3", "archivist", "completed"),
        ]));
        app
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn logo_art_is_teal_with_bright_eyes() {
        let lines = logo_lines(&LOGO);
        assert_eq!(lines.len(), 11);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("████"), "{text}");
        assert!(text.contains('▓'), "the visor must be drawn: {text}");

        // The eye row carries the bright eye colour; the body rows do not.
        let eye_row = &lines[5];
        assert!(
            eye_row.spans.iter().any(|s| s.style.fg == Some(EYE)),
            "row 6 is the eye row"
        );
        let body_row = &lines[0];
        assert!(body_row.spans.iter().all(|s| s.style.fg != Some(EYE)));
    }

    #[test]
    fn selection_clamps_and_filter_narrows() {
        let mut app = app_with_agents();
        app.move_selection(10);
        assert_eq!(app.selected, 2, "selection clamps to the last agent");
        app.move_selection(-10);
        assert_eq!(app.selected, 0);

        app.filter = "writ".into();
        assert_eq!(app.visible().len(), 1);
        app.move_selection(5);
        assert_eq!(app.selected, 0, "filter resets the effective range");
        app.filter.clear();
        assert_eq!(app.visible().len(), 3);
    }

    #[test]
    fn terminate_requires_confirmation() {
        let mut app = app_with_agents();
        assert!(app.handle_key(key(KeyCode::Char('t'))).is_none());
        assert!(app.confirm.is_some());

        // Any non-confirming key cancels without acting.
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.confirm.is_none());

        app.handle_key(key(KeyCode::Char('t')));
        match app.handle_key(key(KeyCode::Char('y'))) {
            Some(Action::Lifecycle { id, action }) => {
                assert_eq!(id, "id-1");
                assert_eq!(action, "terminate");
            }
            _ => panic!("confirmation must produce a terminate action"),
        }
    }

    #[test]
    fn message_input_edits_and_submits() {
        let mut app = app_with_agents();
        app.handle_key(key(KeyCode::Char('m')));
        assert!(app.input.is_some());
        for ch in "hii".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.input.as_ref().unwrap().buffer, "hi!");

        match app.handle_key(key(KeyCode::Enter)) {
            Some(Action::Message { id, text }) => {
                assert_eq!(id, "id-1");
                assert_eq!(text, "hi!");
            }
            _ => panic!("enter must send the message"),
        }
        assert!(app.input.is_none());
    }

    #[test]
    fn permission_decisions_target_the_selected_request() {
        let mut app = app_with_agents();
        app.apply(AppMsg::Permissions(vec![
            serde_json::json!({ "id": "p-1", "agent_id": "id-1", "resource": "/etc", "action": "read", "requested_at": "2026-11-03T00:00:00Z" }),
            serde_json::json!({ "id": "p-2", "agent_id": "id-2", "resource": "api.example.com", "action": "network", "requested_at": "2026-11-03T00:00:00Z" }),
        ]));
        app.tab = Tab::Permissions;
        app.focus = Focus::Main;

        match app.handle_key(key(KeyCode::Char('y'))) {
            Some(Action::Decide { id, grant }) => {
                assert_eq!(id, "p-1");
                assert!(grant);
            }
            _ => panic!("grant action expected"),
        }
        app.handle_key(key(KeyCode::Char('j')));
        match app.handle_key(key(KeyCode::Char('n'))) {
            Some(Action::Decide { id, grant }) => {
                assert_eq!(id, "p-2");
                assert!(!grant);
            }
            _ => panic!("deny action expected"),
        }
    }

    #[test]
    fn quitting_and_tab_navigation() {
        let mut app = app_with_agents();
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab.index(), 1);
        app.handle_key(key(KeyCode::Char('4')));
        assert_eq!(app.tab.index(), 3);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.tab.index(), 2);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.help);
        app.handle_key(key(KeyCode::Char('q')));
        // `q` closes the overlay first, then quits on the next press.
        assert!(!app.quit);
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn failed_poll_marks_the_daemon_offline_and_keeps_the_snapshot() {
        let mut app = app_with_agents();
        assert!(app.online);
        app.apply(AppMsg::Error("cannot reach the Kern daemon".into()));
        assert!(!app.online);
        assert_eq!(app.agents.len(), 3, "stale data is kept, not blanked");
        assert!(app.status.as_ref().unwrap().error);
    }

    #[test]
    fn events_append_only_for_the_selected_agent_and_bound_the_buffer() {
        let mut app = app_with_agents();
        app.apply(AppMsg::Events {
            agent_id: "id-1".into(),
            events: vec![serde_json::json!({ "seq": 4, "ts": "2026-11-03T00:00:00Z", "kind": "agent.started", "payload": {} })],
        });
        assert_eq!(app.events.len(), 1);
        assert_eq!(app.event_cursor, 4);

        // A late response for another agent is ignored.
        app.apply(AppMsg::Events {
            agent_id: "id-2".into(),
            events: vec![serde_json::json!({ "seq": 99, "ts": "2026-11-03T00:00:00Z", "kind": "agent.started", "payload": {} })],
        });
        assert_eq!(app.events.len(), 1);
        assert_eq!(app.event_cursor, 4);
    }

    #[test]
    fn dashboard_renders_logo_agents_and_tabs() {
        let mut app = app_with_agents();
        app.apply(AppMsg::Usage(
            serde_json::json!({ "input_tokens": 1200, "output_tokens": 340 }),
        ));
        app.apply(AppMsg::Detail {
            agent_id: "id-1".into(),
            agent: serde_json::json!({
                "id": "id-1",
                "name": "researcher",
                "lifecycle_state": "running",
                "updated_at": "2026-11-03T00:00:00Z",
                "config": { "model": { "provider": "openai", "model": "gpt-4o-mini" }, "tools": ["http"], "runtime": { "max_steps": 20 } }
            }),
        });

        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("KERN"), "wordmark missing:\n{text}");
        assert!(text.contains('█'), "logo art missing:\n{text}");
        assert!(text.contains("researcher"), "agent list missing:\n{text}");
        assert!(text.contains("Overview"), "tabs missing:\n{text}");
        assert!(
            text.contains("openai / gpt-4o-mini"),
            "overview detail missing:\n{text}"
        );
        assert!(
            text.contains("1200 in / 340 out"),
            "usage header missing:\n{text}"
        );
        assert!(text.contains("online"), "daemon state missing:\n{text}");
    }

    #[test]
    fn events_tab_renders_a_transcript_line() {
        let mut app = app_with_agents();
        app.tab = Tab::Events;
        app.apply(AppMsg::Events {
            agent_id: "id-1".into(),
            events: vec![serde_json::json!({
                "seq": 7,
                "ts": "2026-11-03T00:00:00Z",
                "kind": "tool.completed",
                "payload": { "tool_name": "http", "tool_call_id": "c1", "result": { "status": 200 } }
            })],
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("tool.completed"), "{text}");
        assert!(text.contains("follow:on"), "{text}");
    }

    #[test]
    fn help_overlay_shows_the_logo_and_keymap() {
        let mut app = app_with_agents();
        app.help = true;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("durable agent runtime"), "{text}");
        assert!(text.contains("terminate"), "{text}");
        assert!(text.contains("message the selected agent"), "{text}");
    }

    #[test]
    fn confirm_overlay_names_the_agent() {
        let mut app = app_with_agents();
        app.confirm = Some(Confirm {
            agent_id: "id-1".into(),
            agent_name: "researcher".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(90, 26)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("terminate agent"), "{text}");
        assert!(text.contains("researcher"), "{text}");
    }
}
