//! The interface's state: what has been printed, what the live region shows, and
//! what each key and turn event does to it. Nothing here touches the terminal, so
//! it is driven directly in tests; `mod.rs` owns the terminal and the turn thread.
//!
//! The behavior follows Python's `tui/app.py`: the streamed reply is committed a
//! block at a time (rendered as Markdown with `[ui] markdown`), a tool call becomes
//! one block when its result lands, the approval request takes the place of the
//! input box, and Escape or Ctrl+C cancels a running turn at its next safe point.
//! Everything printed is also kept as [`Entry`]s for the Ctrl+O transcript view.

use super::diff::detail_line;
use super::entries::{view_lines, Entry, ToolEntry, ARG_PREVIEW_LIMIT};
use super::input::Editor;
use super::limits::LimitPanel;
use super::markdown;
use super::mcp_config::McpDraft;
use super::mcp_panels::{McpAction, McpForm, McpList, McpTest, McpTestOutcome};
use super::menu::{mention_text, Menu, MenuKind};
use super::panels::{
    ConnectionTest, KeyEntry, KeyPurpose, SaveDiff, SavePurpose, TestOutcome, TestResult,
};
use super::picker::{Picker, PickerKind, PickerOutcome};
use super::screen::Caret;
use super::status::{fit_status_line, StatusState};
use super::text::{ellipsize, sanitize, text_lines, wrap_words, Line};
use crate::agent::{preview_arguments, summarize, TurnEvent};
use crate::approval::{ApprovalPreview, APPROVAL_DETAIL_LINES};
use crate::config_writer::ConfigEdit;
use crate::limits::LimitHit;
use crate::provider::catalog::CatalogEntry;
use crate::pyjson::{python_json_dumps, python_json_dumps_indented};
use crate::repl::{notice_text, Style, Text};
use crate::types::{Message, Role};
use crate::vault::SecretRequest;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The longest reply tail held back without a block boundary, as Python's
/// `_STREAM_TAIL_LIMIT`; past it the tail is committed at its last newline.
pub const STREAM_TAIL_LIMIT: usize = 2000;
/// How long the "press Ctrl+C again" offer stays open.
pub const EXIT_WINDOW: Duration = Duration::from_secs(2);

const SPINNER: [&str; 6] = ["·", "✢", "✳", "∗", "✻", "✽"];

/// What a turn running on the worker thread reports to the interface.
pub enum UiEvent {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        /// The arguments on one line, for the collapsed block.
        preview: String,
        /// The arguments as indented JSON, for the transcript view.
        args: String,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        summary: String,
        /// What the model was given back.
        result: String,
    },
    /// The gate asks whether a side-effecting call may run; the worker waits for
    /// the answer on `reply`. Dropping `reply` unanswered declines.
    Approval {
        tool: String,
        preview: ApprovalPreview,
        reply: mpsc::Sender<bool>,
    },
    /// The turn reached a limit and asks whether to raise it; the worker waits for
    /// the new value on `reply`. Dropping `reply` unanswered stops.
    Limit {
        hit: LimitHit,
        reply: mpsc::Sender<Option<u32>>,
    },
    /// `request_secret` asks the user for a secret (beyond Python); the worker waits
    /// for the value on `reply`. Dropping `reply` unanswered declines.
    Secret {
        request: SecretRequest,
        reply: mpsc::Sender<Option<String>>,
    },
    /// The turn ended: `None`, or the Python exception name and the message.
    Finished(Option<(String, String)>),
    /// A model catalog fetch finished: the entries, or why there are none.
    Catalog {
        provider: String,
        result: Result<Vec<CatalogEntry>, String>,
        purpose: CatalogPurpose,
    },
    /// The connection test with this id finished.
    Tested {
        id: u64,
        result: TestResult,
    },
    /// The MCP test with this id finished: the server's tools, or why it failed.
    McpTested {
        id: u64,
        result: Result<Vec<(String, String)>, String>,
    },
}

/// What a catalog was fetched for, as Python's `CatalogReady.purpose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogPurpose {
    /// A bare `/model`: open the model picker with it.
    ModelPicker,
    /// A typed `/model <id>` that failed: see whether the catalog vouches for it.
    ModelFallback,
    /// `/config model`: open the catalog picker.
    Manager,
}

impl From<TurnEvent<'_>> for UiEvent {
    fn from(event: TurnEvent<'_>) -> Self {
        match event {
            TurnEvent::Text(text) => Self::Text(text.to_owned()),
            TurnEvent::ToolCall { call, .. } => Self::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                preview: preview_arguments(&call.arguments),
                args: pretty_arguments(&call.arguments),
            },
            TurnEvent::ToolResult {
                call,
                result,
                observation,
                ..
            } => Self::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                ok: result.ok,
                summary: summarize(result),
                result: observation.to_owned(),
            },
        }
    }
}

/// The arguments as Python's `pretty_json` writes them (two-space indent).
fn pretty_arguments(arguments: &Map<String, Value>) -> String {
    python_json_dumps_indented(&Value::Object(arguments.clone()), false, 2)
}

/// What the interface wants its driver to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Dispatch this line: a command or a turn.
    Submit(String),
    Exit,
    /// The running turn finished.
    TurnDone,
    /// A picker was answered.
    Picked(PickerKind, String),
    /// The key entry was answered: the key, or `None` for nothing entered.
    Key(KeyPurpose, Option<String>),
    /// `/secret NAME` was answered: the name, and the value or `None`.
    Secret(String, Option<String>),
    /// The connection test ended.
    Tested(TestOutcome),
    /// The save panel was answered: the edit to apply, or `None` for a cancel.
    Saved(SavePurpose, Option<ConfigEdit>),
    /// Something was chosen in the MCP server list.
    Mcp(McpAction),
    /// The MCP form was completed with a valid server.
    McpForm(Box<McpDraft>),
    /// The MCP test ended.
    McpTested(McpTestOutcome),
    /// A catalog arrived. Held until no turn runs and no panel is open.
    Catalog {
        provider: String,
        result: Result<Vec<CatalogEntry>, String>,
        purpose: CatalogPurpose,
    },
}

/// What takes the input box's place until it is answered.
pub enum Panel {
    Picker(Picker),
    Key(KeyEntry),
    Test(ConnectionTest),
    Save(SaveDiff),
    McpList(McpList),
    McpForm(Box<McpForm>),
    McpTest(McpTest),
}

/// What a key asks of the harness while a turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Stop the turn at its next safe point.
    Cancel,
    /// Switch to the next approval mode.
    CycleMode,
}

struct Running {
    started: Instant,
    cancel_requested: bool,
    /// Ctrl+C was pressed again while cancelling: quit once the turn ends.
    quit_after: bool,
}

struct ApprovalPanel {
    tool: String,
    preview: ApprovalPreview,
    reply: mpsc::Sender<bool>,
    /// 0 runs it, 1 denies.
    selected: usize,
}

/// The Ctrl+O transcript view's position.
struct View {
    /// The first line shown.
    top: usize,
    /// Stay at the bottom as the transcript grows.
    follow: bool,
    /// Tool calls shown in full.
    expanded: bool,
}

pub struct App {
    pub editor: Editor,
    pub status: StatusState,
    /// The session id, for the error hint.
    pub session_id: String,
    /// The `[ui] spinner` setting: animate the working indicator.
    spinner: bool,
    /// The `[ui] markdown` setting: render replies as Markdown.
    pub markdown: bool,
    /// Lines finished since the last frame, waiting to be printed into scrollback.
    committed: Vec<Line>,
    /// Everything printed since the screen was last cleared, for the transcript view.
    entries: Vec<Entry>,
    /// The reply's tail: text streamed but not yet committed.
    stream: String,
    /// The current reply has committed its first chunk (with the `⏺` marker).
    reply_started: bool,
    /// The tail was split off at a block boundary, so a blank line comes first.
    blank_before_chunk: bool,
    /// Tool calls announced and still running, by call id.
    tools: Vec<(String, ToolEntry)>,
    running: Option<Running>,
    approval: Option<ApprovalPanel>,
    /// The offer to raise a limit the turn reached.
    limit: Option<LimitPanel>,
    /// A secret the turn asks for, and where its thread waits for the value.
    secret: Option<(KeyEntry, mpsc::Sender<Option<String>>)>,
    /// A picker or a `/config model` panel.
    panel: Option<Panel>,
    /// The `/` or `@` completion menu under the input box.
    menu: Option<Menu>,
    /// The Ctrl+O transcript view, when open.
    view: Option<View>,
    /// Where `@` paths are completed from: the workspace.
    pub workspace: Option<PathBuf>,
    exit_armed: Option<Instant>,
    request: Option<Request>,
    /// Requests from workers that wait for a quiet moment (no turn, no panel).
    deferred: VecDeque<Request>,
    width: usize,
    height: usize,
    /// Something changed since the last frame.
    dirty: bool,
}

impl App {
    pub fn new(status: StatusState, session_id: String, spinner: bool) -> Self {
        Self {
            editor: Editor::default(),
            status,
            session_id,
            spinner,
            markdown: true,
            committed: Vec::new(),
            entries: Vec::new(),
            stream: String::new(),
            reply_started: false,
            blank_before_chunk: false,
            tools: Vec::new(),
            running: None,
            approval: None,
            limit: None,
            secret: None,
            panel: None,
            menu: None,
            view: None,
            workspace: None,
            exit_armed: None,
            request: None,
            deferred: VecDeque::new(),
            width: 80,
            height: 24,
            dirty: true,
        }
    }

    /// The terminal's size. Rows are drawn one cell narrower than the terminal: a
    /// row reaching the last column wraps at once on some consoles, which would put
    /// the live region's row count off by one.
    pub fn set_size(&mut self, width: usize, height: usize) {
        self.width = width.saturating_sub(1).max(10);
        self.height = height.max(5);
        self.dirty = true;
    }

    pub fn take_request(&mut self) -> Option<Request> {
        if self.request.is_none() && self.running.is_none() && self.panel.is_none() {
            self.request = self.deferred.pop_front();
        }
        self.request.take()
    }

    pub fn take_committed(&mut self) -> Vec<Line> {
        std::mem::take(&mut self.committed)
    }

    /// Forget what was printed, as the screen is about to be cleared.
    pub fn clear_transcript(&mut self) {
        self.committed.clear();
        self.entries.clear();
        self.dirty = true;
    }

    /// Whether a frame should be drawn: something changed, or the spinner moved.
    pub fn needs_frame(&mut self) -> bool {
        std::mem::take(&mut self.dirty) || (self.running.is_some() && self.spinner)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Give up on the turn's pending approval, which then reads as declined.
    pub fn abandon(&mut self) {
        self.approval = None;
        self.limit = None;
        self.secret = None;
    }

    /// Show a picker in place of the input box until it is answered or cancelled.
    pub fn open_picker(&mut self, picker: Picker) {
        self.open_panel(Panel::Picker(picker));
    }

    /// Show a panel in place of the input box until it is answered.
    pub fn open_panel(&mut self, panel: Panel) {
        self.menu = None;
        self.panel = Some(panel);
        self.dirty = true;
    }

    #[cfg(test)]
    pub fn panel(&self) -> Option<&Panel> {
        self.panel.as_ref()
    }

    /// Print a saved conversation, as Python's `_entries_for`: user prompts, replies,
    /// and one block per tool call with the first line of its result.
    pub fn replay(&mut self, messages: &[Message]) {
        let results: HashMap<&str, &str> = messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .filter_map(|message| {
                let id = message.tool_call_id.as_deref()?;
                Some((id, message.content.as_str()))
            })
            .collect();
        for message in messages {
            match message.role {
                Role::User => self.echo_user(&message.content),
                Role::Assistant => {
                    if !message.content.trim().is_empty() {
                        self.stream = sanitize(&message.content);
                        self.finish_reply();
                    }
                    for call in &message.tool_calls {
                        // Python's `compact_json`: nothing at all for no arguments.
                        let preview = if call.arguments.is_empty() {
                            String::new()
                        } else {
                            python_json_dumps(&Value::Object(call.arguments.clone()), false)
                        };
                        let result = results.get(call.id.as_str()).copied();
                        let summary = result.map(|content| {
                            content
                                .trim()
                                .lines()
                                .next()
                                .unwrap_or("(no output)")
                                .to_owned()
                        });
                        self.commit_tool(ToolEntry {
                            name: call.name.clone(),
                            preview,
                            args: pretty_arguments(&call.arguments),
                            summary,
                            result: result.unwrap_or_default().to_owned(),
                            ok: true,
                        });
                    }
                }
                Role::System | Role::Tool => {}
            }
        }
    }

    // ---- printing into scrollback ---------------------------------------

    /// Print harness-owned lines.
    pub fn commit(&mut self, lines: impl IntoIterator<Item = Line>) {
        let lines: Vec<Line> = lines.into_iter().collect();
        self.committed.extend(lines.iter().cloned());
        self.entries.push(Entry::Lines(lines));
        self.dirty = true;
    }

    /// Print a tool call, collapsed; the transcript view can show it in full.
    fn commit_tool(&mut self, tool: ToolEntry) {
        self.committed.extend(tool.collapsed(self.width));
        self.entries.push(Entry::Tool(tool));
        self.dirty = true;
    }

    /// Print a command's output.
    pub fn commit_text(&mut self, text: &Text) {
        self.commit(text_lines(text));
    }

    /// One dim line, made safe to draw.
    pub fn note(&mut self, style: Style, text: &str) {
        let lines: Vec<Line> = sanitize(text)
            .split('\n')
            .map(|line| Line::styled(style, line))
            .collect();
        self.commit(lines);
    }

    /// The user's line, echoed as they typed it.
    pub fn echo_user(&mut self, line: &str) {
        let mut lines = vec![Line::default()];
        for (index, row) in sanitize(line).split('\n').enumerate() {
            let lead = if index == 0 { "> " } else { "  " };
            lines.push(Line::styled(Style::Bold, lead).push(Style::Bold, row));
        }
        self.commit(lines);
    }

    // ---- turns ------------------------------------------------------------

    /// A turn is starting.
    pub fn start_turn(&mut self) {
        self.running = Some(Running {
            started: Instant::now(),
            cancel_requested: false,
            quit_after: false,
        });
        self.stream.clear();
        self.reply_started = false;
        self.tools.clear();
        self.menu = None;
        self.status.working = true;
        self.status.phase = "thinking".into();
        self.dirty = true;
    }

    pub fn on_turn_event(&mut self, event: UiEvent) {
        self.dirty = true;
        match event {
            UiEvent::Text(text) => {
                if let Some(notice) = notice_text(&text) {
                    // The harness talking about the conversation, not the model answering.
                    self.finish_reply();
                    let notice = notice.to_owned();
                    self.note(Style::Dim, &notice);
                    return;
                }
                self.stream.push_str(&sanitize(&text));
                self.status.phase = "thinking".into();
                self.drain_blocks();
            }
            UiEvent::ToolCall {
                id,
                name,
                preview,
                args,
            } => {
                self.finish_reply();
                self.status.phase = format!("running {name}");
                let tool = ToolEntry {
                    name,
                    preview,
                    args,
                    summary: None,
                    result: String::new(),
                    ok: true,
                };
                self.tools.push((id, tool));
            }
            UiEvent::ToolResult {
                id,
                name,
                ok,
                summary,
                result,
            } => {
                let index = self
                    .tools
                    .iter()
                    .position(|(pending, _)| *pending == id)
                    .or_else(|| self.tools.iter().position(|(_, tool)| tool.name == name));
                let mut tool = index.map_or_else(
                    || ToolEntry {
                        name,
                        ..ToolEntry::default()
                    },
                    |index| self.tools.remove(index).1,
                );
                tool.ok = ok;
                tool.summary = Some(summary);
                tool.result = result;
                self.commit_tool(tool);
            }
            UiEvent::Approval {
                tool,
                preview,
                reply,
            } => {
                // The question has to be seen: the transcript view gives way to it.
                self.view = None;
                self.approval = Some(ApprovalPanel {
                    tool,
                    preview,
                    reply,
                    selected: 0,
                });
            }
            UiEvent::Limit { hit, reply } => {
                self.view = None;
                self.limit = Some(LimitPanel::new(hit, reply));
            }
            UiEvent::Secret { request, reply } => {
                self.view = None;
                let entry = KeyEntry::secret(&request.name, &request.reason, request.replacing);
                self.secret = Some((entry, reply));
            }
            UiEvent::Finished(error) => {
                self.finish_reply();
                // Announced but never observed (a cancel mid-batch): shown, not dropped.
                for (_, tool) in std::mem::take(&mut self.tools) {
                    self.commit_tool(tool);
                }
                if let Some((kind, message)) = error {
                    let message = sanitize(&message);
                    let hint = if self.session_id.is_empty() {
                        "see agent86 trace show".to_owned()
                    } else {
                        format!("see agent86 trace show -s {}", self.session_id)
                    };
                    self.commit([
                        Line::default(),
                        Line::styled(Style::Red, "⏺ ")
                            .push(Style::BoldRed, format!("error: {kind}"))
                            .push(Style::Red, format!(": {message}")),
                        Line::styled(Style::Dim, format!("  {hint}")),
                    ]);
                }
                // An approval nobody answered is declined when its sender is dropped.
                self.approval = None;
                self.limit = None;
                self.secret = None;
                let quit = self
                    .running
                    .take()
                    .is_some_and(|running| running.quit_after);
                self.status.working = false;
                self.status.phase.clear();
                self.request = Some(if quit {
                    Request::Exit
                } else {
                    Request::TurnDone
                });
            }
            UiEvent::Catalog {
                provider,
                result,
                purpose,
            } => self.deferred.push_back(Request::Catalog {
                provider,
                result,
                purpose,
            }),
            UiEvent::McpTested { id, result } => {
                if let Some(Panel::McpTest(test)) = &mut self.panel {
                    if test.id == id {
                        test.finish(result);
                    }
                }
            }
            UiEvent::Tested { id, result } => {
                // A late answer for a test already closed is dropped.
                if let Some(Panel::Test(test)) = &mut self.panel {
                    if test.id == id {
                        if let Some(outcome) = test.finish(result) {
                            self.panel = None;
                            self.request = Some(Request::Tested(outcome));
                        }
                    }
                }
            }
        }
    }

    /// Commit the complete blocks of the reply, keeping the unfinished tail live:
    /// Markdown blocks with `[ui] markdown`, otherwise paragraphs.
    fn drain_blocks(&mut self) {
        loop {
            let boundary = if self.markdown {
                markdown::commit_point(&self.stream)
            } else {
                self.stream.rfind("\n\n").map(|at| at + 2)
            };
            if let Some(at) = boundary {
                let tail = self.stream.split_off(at);
                let head = std::mem::replace(&mut self.stream, tail);
                self.commit_chunk(&head);
                self.blank_before_chunk = true;
                continue;
            }
            if self.stream.chars().count() <= STREAM_TAIL_LIMIT {
                return;
            }
            // One runaway block: break at the last newline, or anywhere. A code block
            // cut in two is closed and reopened, so both halves still render as code.
            let at = match self.stream.rfind('\n') {
                Some(at) => at + 1,
                None => self
                    .stream
                    .char_indices()
                    .nth(STREAM_TAIL_LIMIT)
                    .map_or(self.stream.len(), |(at, _)| at),
            };
            let (head, tail) = if self.markdown {
                markdown::split_inside(&self.stream, at)
            } else {
                let tail = self.stream.split_off(at);
                (std::mem::take(&mut self.stream), tail)
            };
            self.stream = tail;
            self.commit_chunk(&head);
            self.blank_before_chunk = false;
        }
    }

    /// Commit one chunk of the reply under the `⏺` marker.
    fn commit_chunk(&mut self, chunk: &str) {
        let mut body = chunk.trim_end_matches('\n');
        if !self.reply_started {
            body = body.trim_start_matches('\n');
        }
        if body.trim().is_empty() {
            return;
        }
        let width = self.width.saturating_sub(2).max(1);
        let rows: Vec<Line> = if self.markdown {
            markdown::render(body, width)
        } else {
            body.split('\n')
                .flat_map(|line| wrap_words(line, width))
                .map(Line::plain)
                .collect()
        };
        let mut lines = Vec::new();
        if !self.reply_started || self.blank_before_chunk {
            lines.push(Line::default());
        }
        for (index, row) in rows.into_iter().enumerate() {
            let mut line = if index == 0 && !self.reply_started {
                Line::plain("⏺ ")
            } else {
                Line::plain("  ")
            };
            line.spans.extend(row.spans);
            lines.push(line);
        }
        self.reply_started = true;
        self.blank_before_chunk = false;
        self.commit(lines);
    }

    fn reply_rows(&self, text: &str) -> Vec<String> {
        let width = self.width.saturating_sub(2).max(1);
        text.split('\n')
            .flat_map(|line| wrap_words(line, width))
            .collect()
    }

    /// End the reply in progress: commit its tail.
    fn finish_reply(&mut self) {
        let tail = std::mem::take(&mut self.stream);
        self.commit_chunk(&tail);
        self.reply_started = false;
        self.blank_before_chunk = false;
    }

    // ---- the transcript view ---------------------------------------------

    pub fn view_open(&self) -> bool {
        self.view.is_some()
    }

    /// The transcript view's page: as much of the transcript as fits, with every
    /// tool call expanded (unless toggled), and a key hint on the last row.
    pub fn view_page(&mut self) -> Vec<Line> {
        let width = self.width;
        let rows = self.height.saturating_sub(1).max(1);
        let Some(view) = &mut self.view else {
            return Vec::new();
        };
        let lines = view_lines(&self.entries, width, view.expanded);
        let bottom = lines.len().saturating_sub(rows);
        if view.follow || view.top >= bottom {
            view.top = bottom;
            view.follow = true;
        }
        let mut page: Vec<Line> = lines[view.top..lines.len().min(view.top + rows)].to_vec();
        page.resize(rows, Line::default());
        let hint = if view.expanded {
            "a to collapse tool calls"
        } else {
            "a to expand tool calls"
        };
        page.push(
            Line::styled(
                Style::Dim,
                format!("  transcript · esc to return · {hint} · ↑↓ PgUp PgDn Home End to scroll"),
            )
            .truncate(width),
        );
        page
    }

    fn view_key(&mut self, key: KeyEvent) {
        let page = self.height.saturating_sub(2).max(1);
        let Some(view) = &mut self.view else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.view = None;
                // Back on the main screen, the live region is drawn again.
                self.dirty = true;
            }
            KeyCode::Char('o' | 'O') if ctrl && key.modifiers.contains(KeyModifiers::SHIFT) => {
                view.expanded = !view.expanded;
            }
            KeyCode::Char('o') if ctrl => {
                self.view = None;
                self.dirty = true;
            }
            KeyCode::Char('a') => view.expanded = !view.expanded,
            KeyCode::Up | KeyCode::Char('k') => {
                view.top = view.top.saturating_sub(1);
                view.follow = false;
            }
            KeyCode::Down | KeyCode::Char('j') => view.top += 1,
            KeyCode::PageUp => {
                view.top = view.top.saturating_sub(page);
                view.follow = false;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => view.top += page,
            KeyCode::Home | KeyCode::Char('g') => {
                view.top = 0;
                view.follow = false;
            }
            KeyCode::End | KeyCode::Char('G') => view.follow = true,
            _ => {}
        }
    }

    // ---- keys -------------------------------------------------------------

    /// Handle one key press; `history` is the shared prompt history.
    pub fn on_key(&mut self, key: KeyEvent, history: &[String]) -> Effect {
        self.dirty = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') && self.view.take().is_some() {
            // Ctrl+C in the transcript view returns to the conversation first.
            return Effect::None;
        }
        if ctrl && key.code == KeyCode::Char('c') && self.panel.is_none() {
            return self.interrupt();
        }
        if self.approval.is_some() {
            self.approval_key(key);
            return Effect::None;
        }
        if let Some((entry, _)) = &mut self.secret {
            if let Some(answer) = entry.on_key(key) {
                if let Some((_, reply)) = self.secret.take() {
                    let _ = reply.send(answer);
                }
            }
            return Effect::None;
        }
        if let Some(panel) = &mut self.limit {
            if let Some(answer) = panel.on_key(key) {
                if let Some(panel) = self.limit.take() {
                    if let Some(value) = answer {
                        self.note(
                            Style::Dim,
                            &format!(
                                "{} raised to {value} for this session · /limits save keeps it",
                                panel.hit.kind.name()
                            ),
                        );
                    }
                    panel.answer(answer);
                }
            }
            return Effect::None;
        }
        // In the MCP form, Shift+Tab moves between fields instead.
        if key.code == KeyCode::BackTab && !matches!(self.panel, Some(Panel::McpForm(_))) {
            return Effect::CycleMode;
        }
        if self.view.is_some() {
            self.view_key(key);
            return Effect::None;
        }
        if ctrl && key.code == KeyCode::Char('o') && self.panel.is_none() {
            self.view = Some(View {
                top: 0,
                follow: true,
                expanded: true,
            });
            return Effect::None;
        }
        if let Some(panel) = &mut self.panel {
            let answer = match panel {
                Panel::Picker(picker) => match picker.on_key(key) {
                    PickerOutcome::Open => return Effect::None,
                    // Python's pickers answer `None`, and nothing follows.
                    PickerOutcome::Cancelled => None,
                    PickerOutcome::Picked(kind, value) => Some(Request::Picked(kind, value)),
                },
                Panel::Key(entry) => match entry.on_key(key) {
                    None => return Effect::None,
                    Some(answer) if entry.purpose == KeyPurpose::Secret => {
                        Some(Request::Secret(entry.provider.clone(), answer))
                    }
                    Some(answer) => Some(Request::Key(entry.purpose, answer)),
                },
                Panel::Test(test) => match test.on_key(key) {
                    None => return Effect::None,
                    Some(outcome) => Some(Request::Tested(outcome)),
                },
                Panel::Save(save) => match save.on_key(key) {
                    None => return Effect::None,
                    Some(edit) => Some(Request::Saved(save.purpose, edit)),
                },
                Panel::McpList(list) => match list.on_key(key) {
                    None => return Effect::None,
                    Some(action) => action.map(Request::Mcp),
                },
                Panel::McpForm(form) => match form.on_key(key) {
                    None => return Effect::None,
                    Some(draft) => draft.map(|draft| Request::McpForm(Box::new(draft))),
                },
                Panel::McpTest(test) => match test.on_key(key) {
                    None => return Effect::None,
                    Some(outcome) => Some(Request::McpTested(outcome)),
                },
            };
            self.panel = None;
            self.request = answer;
            return Effect::None;
        }
        if let Some(running) = &mut self.running {
            if key.code == KeyCode::Esc && !running.cancel_requested {
                return self.request_cancel();
            }
            // The prompt is disabled while a turn runs, as in Python.
            return Effect::None;
        }
        self.exit_armed = None;
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let Some(menu) = &mut self.menu {
            match key.code {
                KeyCode::Up => menu.up(),
                KeyCode::Down => menu.down(),
                KeyCode::Esc => self.menu = None,
                KeyCode::Tab => self.accept_menu(false),
                KeyCode::Enter if !shift && !alt => self.accept_menu(true),
                _ => return self.edit(key, history),
            }
            return Effect::None;
        }
        self.edit(key, history)
    }

    /// An editing key at the idle prompt; the menu follows the draft.
    fn edit(&mut self, key: KeyEvent, history: &[String]) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let before = self.editor.text();
        let editor = &mut self.editor;
        match key.code {
            KeyCode::Enter if shift || alt => editor.insert("\n"),
            KeyCode::Char('j') if ctrl => editor.insert("\n"),
            KeyCode::Enter => {
                let text = editor.text();
                if !text.trim().is_empty() {
                    editor.clear();
                    self.request = Some(Request::Submit(text));
                }
            }
            KeyCode::Char('d') if ctrl => {
                if editor.is_empty() {
                    self.request = Some(Request::Exit);
                } else {
                    editor.delete();
                }
            }
            KeyCode::Char('a') if ctrl => editor.home(),
            KeyCode::Char('e') if ctrl => editor.end(),
            KeyCode::Char('u') if ctrl => editor.delete_to_line_start(),
            KeyCode::Char('k') if ctrl => editor.delete_to_line_end(),
            KeyCode::Char('w') if ctrl => editor.delete_word(),
            KeyCode::Char(ch) if !ctrl => editor.insert(ch.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete(),
            KeyCode::Left if ctrl || alt => editor.word_left(),
            KeyCode::Right if ctrl || alt => editor.word_right(),
            KeyCode::Left => editor.left(),
            KeyCode::Right => editor.right(),
            KeyCode::Home => editor.home(),
            KeyCode::End => editor.end(),
            KeyCode::Up => editor.up(history),
            KeyCode::Down => editor.down(history),
            KeyCode::Esc => editor.clear(),
            _ => {}
        }
        // Python syncs its palette on every change to the text, and only then.
        if self.editor.text() != before {
            self.sync_menu();
        }
        Effect::None
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some((entry, _)) = &mut self.secret {
            entry.on_paste(text);
            self.dirty = true;
        } else if let Some(Panel::Key(entry)) = &mut self.panel {
            entry.on_paste(text);
            self.dirty = true;
        } else if let Some(Panel::McpForm(form)) = &mut self.panel {
            form.on_paste(text);
            self.dirty = true;
        } else if self.running.is_none() && self.panel.is_none() {
            self.editor.insert(&text.replace("\r\n", "\n"));
            self.sync_menu();
            self.dirty = true;
        }
    }

    /// Open, refresh, or close the menu to match the draft.
    fn sync_menu(&mut self) {
        self.menu = Menu::for_draft(
            &self.editor.text(),
            self.editor.cursor(),
            self.workspace.as_deref(),
        );
    }

    /// Take the highlighted menu item. A command runs (`run`, Enter) or is written
    /// out for its argument (Tab); a path replaces the `@` token, and a directory
    /// keeps the menu open on its contents.
    fn accept_menu(&mut self, run: bool) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let chosen = menu.chosen().to_owned();
        match menu.kind {
            MenuKind::Commands if run => {
                self.editor.clear();
                self.request = Some(Request::Submit(chosen));
            }
            MenuKind::Commands => self.editor.set_text(&format!("{chosen} ")),
            MenuKind::Paths { start, end } => {
                self.editor.replace(start, end, &mention_text(&chosen));
                if chosen.ends_with('/') && !chosen.contains(' ') {
                    self.sync_menu();
                }
            }
        }
    }

    /// Ctrl+C: deny a pending approval and cancel; cancel a running turn; quit
    /// after it if pressed again; when idle, clear the draft, then offer to quit.
    fn interrupt(&mut self) -> Effect {
        if let Some(panel) = self.approval.take() {
            let _ = panel.reply.send(false);
        }
        if let Some(panel) = self.limit.take() {
            panel.answer(None);
        }
        if let Some((_, reply)) = self.secret.take() {
            let _ = reply.send(None);
        }
        if let Some(running) = &mut self.running {
            if running.cancel_requested {
                running.quit_after = true;
                return Effect::None;
            }
            return self.request_cancel();
        }
        if !self.editor.is_empty() {
            self.editor.clear();
            self.exit_armed = None;
        } else if self
            .exit_armed
            .is_some_and(|armed| armed.elapsed() < EXIT_WINDOW)
        {
            self.request = Some(Request::Exit);
        } else {
            self.exit_armed = Some(Instant::now());
        }
        Effect::None
    }

    fn request_cancel(&mut self) -> Effect {
        if let Some(running) = &mut self.running {
            running.cancel_requested = true;
        }
        self.finish_reply();
        self.note(Style::Dim, "cancelling…");
        self.status.phase = "cancelling".into();
        Effect::Cancel
    }

    fn approval_key(&mut self, key: KeyEvent) {
        let Some(panel) = &mut self.approval else {
            return;
        };
        let answer = match key.code {
            KeyCode::Char('y' | 'Y' | '1') => Some(true),
            KeyCode::Char('n' | 'N' | '2') | KeyCode::Esc => Some(false),
            KeyCode::Enter => Some(panel.selected == 0),
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                panel.selected = 1 - panel.selected;
                None
            }
            _ => None,
        };
        if let Some(approved) = answer {
            if let Some(panel) = self.approval.take() {
                let _ = panel.reply.send(approved);
            }
        }
    }

    /// Expire the Ctrl+C exit offer.
    pub fn tick(&mut self) {
        if self
            .exit_armed
            .is_some_and(|armed| armed.elapsed() >= EXIT_WINDOW)
        {
            self.exit_armed = None;
            self.dirty = true;
        }
    }

    // ---- the live region --------------------------------------------------

    /// The terminal's width in cells.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The live region's lines, each fitting the width, and the caret.
    pub fn live(&mut self) -> (Vec<Line>, Caret) {
        let width = self.width;
        let mut top = Vec::new();

        if let Some(running) = &self.running {
            // Tools announced but still running, with the reply tail above them.
            if !self.stream.trim().is_empty() {
                let body = if self.reply_started {
                    self.stream.as_str()
                } else {
                    self.stream.trim_start_matches('\n')
                };
                let mut rows = Vec::new();
                if !self.reply_started || self.blank_before_chunk {
                    rows.push(Line::default());
                }
                for (index, row) in self.reply_rows(body).into_iter().enumerate() {
                    let lead = if index == 0 && !self.reply_started {
                        "⏺ "
                    } else {
                        "  "
                    };
                    rows.push(Line::plain(lead).push(Style::Plain, row));
                }
                top.extend(rows);
            }
            for (_, tool) in &self.tools {
                top.push(Line::default());
                top.push(
                    Line::styled(Style::Dim, "⏺ ")
                        .push(Style::Bold, sanitize(&tool.name))
                        .push(
                            Style::Plain,
                            format!(
                                "({})",
                                ellipsize(&sanitize(&tool.preview), ARG_PREVIEW_LIMIT)
                            ),
                        )
                        .truncate(width),
                );
            }
            top.push(Line::default());
            top.push(self.spinner_line(running).truncate(width));
        }

        let mut bottom = vec![Line::default()];
        let caret_base;
        let mut caret = None;
        if let Some(panel) = &self.approval {
            let budget = self
                .height
                .saturating_sub(top.len().min(self.height / 2) + 4);
            bottom.extend(approval_panel(panel, width, budget));
            caret_base = 0;
        } else if let Some((entry, _)) = &self.secret {
            bottom.extend(entry.render(width));
            caret_base = 0;
        } else if let Some(panel) = &self.limit {
            bottom.extend(panel.render(width));
            caret_base = 0;
        } else if let Some(panel) = &self.panel {
            bottom.extend(match panel {
                Panel::Picker(picker) => picker.render(width),
                Panel::Key(entry) => entry.render(width),
                Panel::Test(test) => test.render(width, SPINNER[4]),
                Panel::Save(save) => save.render(width),
                Panel::McpList(list) => list.render(width),
                Panel::McpForm(form) => form.render(width),
                Panel::McpTest(test) => test.render(width, SPINNER[4]),
            });
            caret_base = 0;
        } else {
            let (lines, (row, col)) =
                self.editor
                    .render(width, self.running.is_none(), &self.status.cwd);
            caret_base = bottom.len();
            if self.running.is_none() {
                caret = Some((row, col));
            }
            bottom.extend(lines);
            if let Some(menu) = &self.menu {
                bottom.extend(menu.render(width));
            }
        }
        let status = if self.exit_armed.is_some() {
            Line::styled(Style::Dim, "  Press Ctrl+C again to exit")
        } else {
            Line::styled(
                Style::Dim,
                format!(
                    "  {}",
                    fit_status_line(&self.status, width.saturating_sub(2))
                ),
            )
        };
        bottom.push(status.truncate(width));

        // The region must fit the screen: the reply tail gives way first, from the top.
        let room = self.height.saturating_sub(1);
        if top.len() + bottom.len() > room {
            let keep = room.saturating_sub(bottom.len());
            top.drain(..top.len() - keep.min(top.len()));
        }
        let caret = caret.map(|(row, col)| (top.len() + caret_base + row, col));
        let mut live = top;
        live.extend(bottom);
        if live.len() > room {
            live.drain(..live.len() - room);
        }
        (live, caret)
    }

    fn spinner_line(&self, running: &Running) -> Line {
        let elapsed = running.started.elapsed();
        let glyph = if self.spinner {
            SPINNER[(elapsed.as_millis() / 120) as usize % SPINNER.len()]
        } else {
            "✻"
        };
        let mut phase = self.status.phase.clone();
        if let Some(first) = phase.get(..1) {
            phase = format!("{}{}", first.to_uppercase(), &phase[1..]);
        }
        let hint = if running.cancel_requested {
            String::new()
        } else {
            " · esc to interrupt".into()
        };
        Line::styled(Style::BoldCyan, format!("{glyph} "))
            .push(Style::Plain, format!("{phase}…"))
            .push(Style::Dim, format!(" ({}s{hint})", elapsed.as_secs()))
    }
}

/// The approval request, boxed, in place of the input box; at most `budget` rows.
fn approval_panel(panel: &ApprovalPanel, width: usize, budget: usize) -> Vec<Line> {
    let inner = width.saturating_sub(4).max(1);
    let mut content: Vec<Line> = vec![Line::styled(
        Style::Bold,
        format!("Approve tool: {}?", sanitize(&panel.tool)),
    )];
    for row in sanitize(&panel.preview.summary).split('\n') {
        content.push(Line::plain(row));
    }
    if let Some(detail) = &panel.preview.detail {
        content.push(Line::default());
        for row in detail.split('\n').take(APPROVAL_DETAIL_LINES) {
            content.push(detail_line(row, panel.preview.lexer));
        }
    }
    let mut rows: Vec<Line> = content
        .iter()
        .flat_map(|line| line.hard_wrap(inner))
        .collect();
    let detail_lines = panel
        .preview
        .detail
        .as_deref()
        .map_or(0, |detail| sanitize(detail).split('\n').count());
    let hidden_by_cap = detail_lines.saturating_sub(APPROVAL_DETAIL_LINES);
    // Two borders, a blank, two choices, and the key hint.
    let room = budget.saturating_sub(6).max(3);
    let mut dropped = 0;
    if rows.len() > room {
        dropped = rows.len() - (room - 1);
        rows.truncate(room - 1);
    }
    if dropped + hidden_by_cap > 0 {
        rows.push(Line::styled(
            Style::Dim,
            format!("… truncated ({} more lines)", dropped + hidden_by_cap),
        ));
    }
    rows.push(Line::default());
    for (index, label) in ["1. Run it", "2. Deny"].into_iter().enumerate() {
        rows.push(if index == panel.selected {
            Line::styled(Style::BoldCyan, format!("❯ {label}"))
        } else {
            Line::plain(format!("  {label}"))
        });
    }

    let border = Style::Yellow;
    let mut lines = vec![Line::styled(border, format!("╭{}╮", "─".repeat(width - 2)))];
    for row in rows {
        let pad = inner.saturating_sub(row.width());
        let mut line = Line::styled(border, "│ ");
        line.spans.extend(row.spans);
        lines.push(line.push(Style::Plain, " ".repeat(pad)).push(border, " │"));
    }
    lines.push(Line::styled(border, format!("╰{}╯", "─".repeat(width - 2))));
    // Every live row must fit, or the renderer's row count goes wrong.
    lines.push(
        Line::styled(
            Style::Dim,
            "  y to run it · n or esc to deny · ↑↓ and enter to choose",
        )
        .truncate(width),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn status() -> StatusState {
        StatusState {
            model: "gpt-4o-mini".into(),
            cwd: String::new(),
            used_tokens: 0,
            window: 128_000,
            output_tokens: 0,
            cost_usd: 0.0,
            priced: true,
            sandbox: "subprocess".into(),
            approval: "ask".into(),
            working: false,
            phase: String::new(),
            hotkey_hint: "Shift+Tab".into(),
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    fn app() -> App {
        let mut app = App::new(status(), "s1".into(), false);
        app.set_size(41, 30);
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new_with_kind(
            KeyCode::Char(ch),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        )
    }

    fn committed_text(app: &mut App) -> Vec<String> {
        app.take_committed().iter().map(Line::plain_text).collect()
    }

    fn live_text(app: &mut App) -> Vec<String> {
        app.live().0.iter().map(Line::plain_text).collect()
    }

    #[test]
    fn enter_submits_the_draft_and_shift_enter_adds_a_line() {
        let mut app = app();
        for ch in "hi".chars() {
            app.on_key(key(KeyCode::Char(ch)), &[]);
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &[]);
        app.on_key(key(KeyCode::Char('x')), &[]);
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(app.take_request(), Some(Request::Submit("hi\nx".into())));
        assert!(app.editor.is_empty());
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(app.take_request(), None, "an empty draft is not submitted");
    }

    #[test]
    fn a_reply_is_committed_a_paragraph_at_a_time() {
        let mut app = app();
        app.start_turn();
        app.on_turn_event(UiEvent::Text("First para".into()));
        assert!(committed_text(&mut app).is_empty());
        assert!(live_text(&mut app).contains(&"⏺ First para".to_owned()));
        app.on_turn_event(UiEvent::Text("graph.\n\nSecond".into()));
        assert_eq!(committed_text(&mut app), ["", "⏺ First paragraph."]);
        app.on_turn_event(UiEvent::Finished(None));
        assert_eq!(committed_text(&mut app), ["", "  Second"]);
        assert_eq!(app.take_request(), Some(Request::TurnDone));
    }

    #[test]
    fn long_replies_wrap_under_the_marker() {
        let mut app = app();
        app.start_turn();
        app.on_turn_event(UiEvent::Text(
            "one two three four five six seven eight nine ten eleven".into(),
        ));
        app.on_turn_event(UiEvent::Finished(None));
        let lines = committed_text(&mut app);
        assert_eq!(lines[1], "⏺ one two three four five six seven");
        assert_eq!(lines[2], "  eight nine ten eleven");
    }

    #[test]
    fn tool_calls_become_blocks_when_their_results_land() {
        let mut app = app();
        app.start_turn();
        app.on_turn_event(UiEvent::Text("Let me look.".into()));
        app.on_turn_event(UiEvent::ToolCall {
            id: "c1".into(),
            name: "list_dir".into(),
            preview: r#"{"path": "."}"#.into(),
            args: "{\n  \"path\": \".\"\n}".into(),
        });
        assert_eq!(committed_text(&mut app), ["", "⏺ Let me look."]);
        assert!(live_text(&mut app).contains(&r#"⏺ list_dir({"path": "."})"#.to_owned()));
        assert_eq!(app.status.phase, "running list_dir");
        app.on_turn_event(UiEvent::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            ok: false,
            summary: "error: No such directory".into(),
            result: "Error: No such directory: .".into(),
        });
        assert_eq!(
            committed_text(&mut app),
            [
                "",
                r#"⏺ list_dir({"path": "."})"#,
                "  ⎿  error: No such directory"
            ]
        );
    }

    #[test]
    fn markdown_replies_commit_whole_blocks() {
        let mut app = app();
        app.start_turn();
        app.on_turn_event(UiEvent::Text(
            "Steps:\n\n```sh\ncargo build\n\ncargo test\n".into(),
        ));
        assert_eq!(committed_text(&mut app), ["", "⏺ Steps:"]);
        app.on_turn_event(UiEvent::Text("```\n\n- **done**".into()));
        assert_eq!(
            committed_text(&mut app),
            ["", "  │ cargo build", "  │ ", "  │ cargo test"],
            "the code block waited for its closing fence"
        );
        app.on_turn_event(UiEvent::Finished(None));
        let lines = app.take_committed();
        assert_eq!(lines[1].plain_text(), "  • done");
        assert!(lines[1].spans.contains(&(Style::Bold, "done".to_owned())));

        let mut plain = self::app();
        plain.markdown = false;
        plain.start_turn();
        plain.on_turn_event(UiEvent::Text("- **raw**".into()));
        plain.on_turn_event(UiEvent::Finished(None));
        assert_eq!(committed_text(&mut plain), ["", "⏺ - **raw**"]);
    }

    #[test]
    fn the_transcript_view_shows_tool_calls_in_full() {
        let mut app = app();
        app.set_size(61, 12);
        app.echo_user("look");
        app.start_turn();
        app.on_turn_event(UiEvent::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            preview: r#"{"path": "a.txt"}"#.into(),
            args: "{\n  \"path\": \"a.txt\"\n}".into(),
        });
        app.on_turn_event(UiEvent::ToolResult {
            id: "c1".into(),
            name: "read_file".into(),
            ok: true,
            summary: "one".into(),
            result: "one\ntwo\nthree".into(),
        });
        app.on_key(ctrl('o'), &[]);
        assert!(app.view_open());
        let page: Vec<String> = app.view_page().iter().map(Line::plain_text).collect();
        assert_eq!(page.len(), 12);
        assert!(page.contains(&"    three".to_owned()), "{page:?}");
        assert!(page.last().unwrap().contains("esc to return"));

        app.on_key(key(KeyCode::Char('a')), &[]);
        let collapsed: Vec<String> = app.view_page().iter().map(Line::plain_text).collect();
        assert!(!collapsed.contains(&"    three".to_owned()));
        app.on_key(key(KeyCode::Home), &[]);
        assert_eq!(app.view_page()[1].plain_text(), "> look");

        // An approval needs an answer: the view gives way to it.
        let (reply, _answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Approval {
            tool: "write_file".into(),
            preview: ApprovalPreview {
                summary: "{}".into(),
                detail: None,
                lexer: None,
            },
            reply,
        });
        assert!(!app.view_open());
    }

    #[test]
    fn the_approval_panel_colours_a_diff() {
        let mut app = app_running();
        let (reply, _answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Approval {
            tool: "edit_file".into(),
            preview: ApprovalPreview {
                summary: "{}".into(),
                detail: Some("--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-old\n+new".into()),
                lexer: Some("diff"),
            },
            reply,
        });
        let (live, _) = app.live();
        let style_of = |text: &str| {
            live.iter()
                .flat_map(|line| line.spans.iter())
                .find(|(_, piece)| piece == text)
                .map(|(style, _)| *style)
        };
        assert_eq!(style_of("+new"), Some(Style::Green));
        assert_eq!(style_of("-old"), Some(Style::Red));
        assert_eq!(style_of("@@ -1 +1 @@"), Some(Style::Cyan));
    }

    #[test]
    fn harness_notices_are_set_apart() {
        let mut app = app();
        app.start_turn();
        app.on_turn_event(UiEvent::Text("partial".into()));
        app.on_turn_event(UiEvent::Text("\n[continuation 1/3]\n".into()));
        assert_eq!(
            committed_text(&mut app),
            ["", "⏺ partial", "[continuation 1/3]"]
        );
    }

    #[test]
    fn a_failed_turn_shows_python_s_error_and_the_trace_hint() {
        let mut app = app();
        app.start_turn();
        app.on_turn_event(UiEvent::Finished(Some((
            "ProviderError".into(),
            "HTTP 401".into(),
        ))));
        assert_eq!(
            committed_text(&mut app),
            [
                "",
                "⏺ error: ProviderError: HTTP 401",
                "  see agent86 trace show -s s1"
            ]
        );
    }

    #[test]
    fn escape_and_ctrl_c_cancel_and_a_second_ctrl_c_quits_after_the_turn() {
        let mut app = app();
        app.start_turn();
        assert_eq!(app.on_key(key(KeyCode::Esc), &[]), Effect::Cancel);
        assert_eq!(committed_text(&mut app), ["cancelling…"]);
        assert_eq!(app.status.phase, "cancelling");
        assert_eq!(app.on_key(key(KeyCode::Esc), &[]), Effect::None);
        assert_eq!(app.on_key(ctrl('c'), &[]), Effect::None);
        app.on_turn_event(UiEvent::Finished(None));
        assert_eq!(app.take_request(), Some(Request::Exit));

        let mut other = app_running();
        assert_eq!(other.on_key(ctrl('c'), &[]), Effect::Cancel);
    }

    fn app_running() -> App {
        let mut app = app();
        app.start_turn();
        app
    }

    #[test]
    fn idle_ctrl_c_clears_the_draft_then_asks_before_quitting() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('x')), &[]);
        app.on_key(ctrl('c'), &[]);
        assert!(app.editor.is_empty());
        assert_eq!(app.take_request(), None);
        app.on_key(ctrl('c'), &[]);
        assert!(live_text(&mut app)
            .last()
            .is_some_and(|line| line.contains("Press Ctrl+C again to exit")));
        app.on_key(ctrl('c'), &[]);
        assert_eq!(app.take_request(), Some(Request::Exit));

        let mut eof = self::app();
        eof.on_key(ctrl('d'), &[]);
        assert_eq!(eof.take_request(), Some(Request::Exit));
    }

    #[test]
    fn a_secret_is_entered_masked_and_answers_the_worker() {
        let request = || SecretRequest {
            name: "API_TOKEN".into(),
            reason: "to call the billing API".into(),
            replacing: false,
        };
        let mut app = app_running();
        app.set_size(81, 30);
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Secret {
            request: request(),
            reply,
        });
        for ch in "tok_".chars() {
            app.on_key(key(KeyCode::Char(ch)), &[]);
        }
        app.on_paste("s3cr3t\r\n");
        let live = live_text(&mut app).join("\n");
        assert!(live.contains("Secret API_TOKEN"), "{live}");
        assert!(live.contains("to call the billing API"), "{live}");
        assert!(live.contains(&"•".repeat(10)), "{live}");
        assert!(!live.contains("tok_") && !live.contains("s3cr3t"), "{live}");
        assert!(app.editor.is_empty(), "nothing reaches the draft");
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(answer.recv(), Ok(Some("tok_s3cr3t".to_owned())));
        assert!(!committed_text(&mut app).join("\n").contains("s3cr3t"));

        // Escape declines without cancelling the turn.
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Secret {
            request: request(),
            reply,
        });
        assert_eq!(app.on_key(key(KeyCode::Esc), &[]), Effect::None);
        assert_eq!(answer.recv(), Ok(None));

        // Ctrl+C declines and cancels the turn.
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Secret {
            request: request(),
            reply,
        });
        assert_eq!(app.on_key(ctrl('c'), &[]), Effect::Cancel);
        assert_eq!(answer.recv(), Ok(None));

        // A turn that ends drops an entry nobody finished, which declines.
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Secret {
            request: request(),
            reply,
        });
        app.on_key(key(KeyCode::Char('x')), &[]);
        app.on_turn_event(UiEvent::Finished(None));
        assert!(answer.recv().is_err());
    }

    #[test]
    fn slash_secret_answers_with_its_name() {
        let mut app = app();
        app.open_panel(Panel::Key(KeyEntry::secret("API_TOKEN", "", true)));
        let live = live_text(&mut app).join("\n");
        assert!(live.contains("Replaces the value entered before"), "{live}");
        app.on_paste("v4lue");
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(
            app.take_request(),
            Some(Request::Secret("API_TOKEN".into(), Some("v4lue".into())))
        );
    }

    #[test]
    fn the_limit_offer_answers_the_worker_and_says_what_was_raised() {
        let hit = || LimitHit {
            kind: crate::limits::LimitKind::AgentSteps,
            agent: Some("reviewer".into()),
            current: 8,
            offers: vec![16, 32],
        };
        let mut app = app_running();
        app.set_size(81, 30);
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Limit { hit: hit(), reply });
        let live = live_text(&mut app);
        assert!(live
            .iter()
            .any(|line| line.contains("Sub-agent 'reviewer' reached its step budget (8 steps)")));
        assert!(live
            .iter()
            .any(|line| line.contains("❯ 1. Raise to 16 steps")));
        assert_eq!(app.live().1, None, "no caret while the panel is open");
        app.on_key(key(KeyCode::Char('2')), &[]);
        assert_eq!(answer.recv(), Ok(Some(32)));
        assert!(committed_text(&mut app).iter().any(|line| line
            .contains("agent_steps raised to 32 for this session · /limits save keeps it")));

        // Escape stops without cancelling the turn.
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Limit { hit: hit(), reply });
        assert_eq!(app.on_key(key(KeyCode::Esc), &[]), Effect::None);
        assert_eq!(answer.recv(), Ok(None));

        // Ctrl+C stops it and cancels the turn.
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Limit { hit: hit(), reply });
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_c, &[]), Effect::Cancel);
        assert_eq!(answer.recv(), Ok(None));

        // A turn that ends drops an offer nobody answered, which stops.
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Limit { hit: hit(), reply });
        app.on_turn_event(UiEvent::Finished(None));
        assert!(answer.recv().is_err());
        assert!(!live_text(&mut app)
            .iter()
            .any(|line| line.contains("step budget")));
    }

    #[test]
    fn the_approval_panel_answers_the_worker() {
        let mut app = app_running();
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Approval {
            tool: "write_file".into(),
            preview: ApprovalPreview {
                summary: r#"{"path": "a.txt"}"#.into(),
                detail: Some("--- a.txt\n+++ a.txt\n+hello".into()),
                lexer: Some("diff"),
            },
            reply,
        });
        let live = live_text(&mut app);
        assert!(live
            .iter()
            .any(|line| line.contains("Approve tool: write_file?")));
        assert!(live.iter().any(|line| line.contains("+hello")));
        assert!(live.iter().any(|line| line.contains("❯ 1. Run it")));
        assert_eq!(app.live().1, None, "no caret while the panel is open");
        app.on_key(key(KeyCode::Down), &[]);
        app.on_key(key(KeyCode::Enter), &[]);
        assert!(!answer.recv().unwrap());

        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Approval {
            tool: "write_file".into(),
            preview: ApprovalPreview {
                summary: "{}".into(),
                detail: None,
                lexer: None,
            },
            reply,
        });
        app.on_key(key(KeyCode::Char('y')), &[]);
        assert!(answer.recv().unwrap());

        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Approval {
            tool: "run_command".into(),
            preview: ApprovalPreview {
                summary: "{}".into(),
                detail: None,
                lexer: None,
            },
            reply,
        });
        assert_eq!(app.on_key(ctrl('c'), &[]), Effect::Cancel);
        assert!(!answer.recv().unwrap(), "Ctrl+C denies and cancels");
    }

    #[test]
    fn a_long_approval_detail_is_cut_to_the_screen() {
        let mut app = app_running();
        let (reply, _answer) = mpsc::channel();
        let detail: Vec<String> = (0..100).map(|n| format!("+line {n}")).collect();
        app.on_turn_event(UiEvent::Approval {
            tool: "write_file".into(),
            preview: ApprovalPreview {
                summary: "{}".into(),
                detail: Some(detail.join("\n")),
                lexer: Some("diff"),
            },
            reply,
        });
        let live = live_text(&mut app);
        assert!(live.len() < 30, "{}", live.len());
        assert!(live.iter().any(|line| line.contains("… truncated (")));
    }

    #[test]
    fn the_live_region_never_outgrows_the_screen() {
        let mut app = app();
        app.set_size(41, 10);
        app.start_turn();
        app.on_turn_event(UiEvent::Text("word ".repeat(200)));
        let (live, _) = app.live();
        assert!(live.len() <= 9, "{}", live.len());
        assert!(live.iter().all(|line| line.width() <= 40));
    }

    fn typed(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.on_key(key(KeyCode::Char(ch)), &[]);
        }
    }

    #[test]
    fn the_command_menu_runs_or_writes_out_the_highlighted_command() {
        let mut app = app();
        typed(&mut app, "/he");
        assert!(live_text(&mut app)
            .iter()
            .any(|line| line.starts_with("  ❯ /help") && line.contains("Show this help")));
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(app.take_request(), Some(Request::Submit("/help".into())));
        assert!(app.editor.is_empty());

        typed(&mut app, "/mod");
        app.on_key(key(KeyCode::Down), &[]);
        app.on_key(key(KeyCode::Tab), &[]);
        assert_eq!(app.editor.text(), "/model ");
        assert!(app.menu.is_none(), "a space closes the menu");
        assert_eq!(app.take_request(), None);
    }

    #[test]
    fn escape_closes_the_menu_before_clearing_the_draft() {
        let mut app = app();
        typed(&mut app, "/c");
        assert!(app.menu.is_some());
        app.on_key(key(KeyCode::Esc), &[]);
        assert!(app.menu.is_none());
        assert_eq!(app.editor.text(), "/c");
        app.on_key(key(KeyCode::Esc), &[]);
        assert!(app.editor.is_empty());

        // Up and Down browse history again once the menu is gone.
        let history = vec!["earlier".to_owned()];
        app.on_key(key(KeyCode::Up), &history);
        assert_eq!(app.editor.text(), "earlier");
    }

    #[test]
    fn at_paths_complete_from_the_workspace_and_descend_into_directories() {
        let dir = std::env::temp_dir().join(format!("agent86-app-at-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("my notes.md"), "").unwrap();
        let mut app = app();
        app.workspace = Some(dir.clone());

        typed(&mut app, "read @s");
        assert!(live_text(&mut app).iter().any(|line| line == "  ❯ @src/"));
        app.on_key(key(KeyCode::Tab), &[]);
        assert_eq!(app.editor.text(), "read @src/");
        assert!(live_text(&mut app)
            .iter()
            .any(|line| line == "  ❯ @src/main.rs"));
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(app.editor.text(), "read @src/main.rs");
        assert!(app.menu.is_none());
        assert_eq!(app.take_request(), None, "Enter completed, it did not send");

        app.editor.clear();
        typed(&mut app, "@my");
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(app.editor.text(), "@\"my notes.md\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_answers_reach_the_right_panel_at_a_quiet_moment() {
        let mut app = app();
        // A catalog arriving mid-turn waits for the turn to end.
        app.start_turn();
        app.on_turn_event(UiEvent::Catalog {
            provider: "groq".into(),
            result: Ok(Vec::new()),
            purpose: CatalogPurpose::Manager,
        });
        assert_eq!(app.take_request(), None);
        app.on_turn_event(UiEvent::Finished(None));
        assert_eq!(app.take_request(), Some(Request::TurnDone));
        assert!(matches!(
            app.take_request(),
            Some(Request::Catalog { provider, .. }) if provider == "groq"
        ));

        // A test's answer closes only the panel it belongs to.
        app.open_panel(Panel::Test(ConnectionTest::new(2, "groq:llama")));
        app.on_turn_event(UiEvent::Tested {
            id: 1,
            result: TestResult::Passed,
        });
        assert!(app.panel.is_some(), "a stale answer is dropped");
        app.on_turn_event(UiEvent::Tested {
            id: 2,
            result: TestResult::Passed,
        });
        assert!(app.panel.is_none());
        assert!(matches!(app.take_request(), Some(Request::Tested(outcome)) if outcome.ok));

        // A pasted key goes to the key panel, never the draft, and is never drawn.
        app.open_panel(Panel::Key(KeyEntry::new(
            KeyPurpose::Provider,
            "groq",
            true,
        )));
        app.on_paste("gsk_pasted_secret");
        let (live, _) = app.live();
        assert!(!live.iter().any(|line| line.plain_text().contains("pasted")));
        assert!(app.editor.is_empty());
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(
            app.take_request(),
            Some(Request::Key(
                KeyPurpose::Provider,
                Some("gsk_pasted_secret".into())
            ))
        );
    }

    #[test]
    fn a_picker_takes_the_input_box_s_place_and_answers_once() {
        let mut app = app();
        app.open_picker(Picker::mode("ask"));
        let (live, caret) = app.live();
        assert!(live
            .iter()
            .any(|line| line.plain_text().contains("Approval mode")));
        assert_eq!(caret, None);
        app.on_key(key(KeyCode::Char('x')), &[]);
        assert!(app.editor.is_empty(), "typing goes to the picker");
        app.on_key(key(KeyCode::Down), &[]);
        app.on_key(key(KeyCode::Enter), &[]);
        assert_eq!(
            app.take_request(),
            Some(Request::Picked(PickerKind::Mode, "auto".into()))
        );
        assert!(app.panel.is_none());

        app.open_picker(Picker::mode("ask"));
        app.on_key(ctrl('c'), &[]);
        assert!(app.panel.is_none(), "Ctrl+C cancels the picker");
        assert_eq!(app.take_request(), None);
    }

    #[test]
    fn a_resumed_conversation_is_replayed_as_entries() {
        let messages: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role": "system", "content": "you are agent86"},
            {"role": "user", "content": "list the files"},
            {"role": "assistant", "content": "Looking.", "tool_calls": [
                {"id": "c1", "name": "list_dir", "arguments": {"path": "."}},
                {"id": "c2", "name": "read_file", "arguments": {}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "\nsrc/\nREADME.md"},
            {"role": "tool", "tool_call_id": "c2", "content": ""},
            {"role": "assistant", "content": "Two entries."}
        ]))
        .unwrap();
        let mut app = app();
        app.replay(&messages);
        assert_eq!(
            committed_text(&mut app),
            [
                "",
                "> list the files",
                "",
                "⏺ Looking.",
                "",
                r#"⏺ list_dir({"path": "."})"#,
                "  ⎿  src/",
                "",
                "⏺ read_file()",
                "  ⎿  (no output)",
                "",
                "⏺ Two entries.",
            ]
        );
    }

    #[test]
    fn the_caret_sits_in_the_input_box_after_the_prompt() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('a')), &[]);
        let (live, caret) = app.live();
        let (row, col) = caret.unwrap();
        assert!(live[row].plain_text().starts_with("│ > a"));
        assert_eq!(col, 5);
    }
}
