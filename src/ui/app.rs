//! The interface's state: what has been printed, what the live region shows, and
//! what each key and turn event does to it. Nothing here touches the terminal, so
//! it is driven directly in tests; `mod.rs` owns the terminal and the turn thread.
//!
//! The behavior follows Python's `tui/app.py`: the streamed reply is committed a
//! paragraph at a time, a tool call becomes one block when its result lands, the
//! approval request takes the place of the input box, and Escape or Ctrl+C cancels
//! a running turn at its next safe point.

use super::input::Editor;
use super::screen::Caret;
use super::status::{fit_status_line, StatusState};
use super::text::{ellipsize, sanitize, text_lines, wrap_words, Line};
use crate::agent::{preview_arguments, summarize, TurnEvent};
use crate::approval::{ApprovalPreview, APPROVAL_DETAIL_LINES};
use crate::repl::{notice_text, Style, Text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How much of the argument JSON fits on a tool block's first line.
pub const ARG_PREVIEW_LIMIT: usize = 72;
/// How much of the result summary fits on a tool block's second line.
pub const SUMMARY_LIMIT: usize = 100;
/// The longest reply tail held back without a paragraph break, as Python's
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
        preview: String,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        summary: String,
    },
    /// The gate asks whether a side-effecting call may run; the worker waits for
    /// the answer on `reply`. Dropping `reply` unanswered declines.
    Approval {
        tool: String,
        preview: ApprovalPreview,
        reply: mpsc::Sender<bool>,
    },
    /// The turn ended: `None`, or the Python exception name and the message.
    Finished(Option<(String, String)>),
}

impl From<TurnEvent<'_>> for UiEvent {
    fn from(event: TurnEvent<'_>) -> Self {
        match event {
            TurnEvent::Text(text) => Self::Text(text.to_owned()),
            TurnEvent::ToolCall { call, .. } => Self::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                preview: preview_arguments(&call.arguments),
            },
            TurnEvent::ToolResult { call, result, .. } => Self::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                ok: result.ok,
                summary: summarize(result),
            },
        }
    }
}

/// What the interface wants its driver to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Dispatch this line: a command or a turn.
    Submit(String),
    Exit,
    /// The running turn finished.
    TurnDone,
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

struct PendingTool {
    id: String,
    name: String,
    preview: String,
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

pub struct App {
    pub editor: Editor,
    pub status: StatusState,
    /// The session id, for the error hint.
    pub session_id: String,
    /// The `[ui] spinner` setting: animate the working indicator.
    spinner: bool,
    /// Lines finished since the last frame, waiting to be printed into scrollback.
    committed: Vec<Line>,
    /// The reply's tail: text streamed but not yet committed.
    stream: String,
    /// The current reply has committed its first chunk (with the `⏺` marker).
    reply_started: bool,
    /// The tail was split off at a paragraph break, so a blank line comes first.
    blank_before_chunk: bool,
    tools: Vec<PendingTool>,
    running: Option<Running>,
    approval: Option<ApprovalPanel>,
    exit_armed: Option<Instant>,
    request: Option<Request>,
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
            committed: Vec::new(),
            stream: String::new(),
            reply_started: false,
            blank_before_chunk: false,
            tools: Vec::new(),
            running: None,
            approval: None,
            exit_armed: None,
            request: None,
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
        self.request.take()
    }

    pub fn take_committed(&mut self) -> Vec<Line> {
        std::mem::take(&mut self.committed)
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
    }

    // ---- printing into scrollback ---------------------------------------

    /// Print harness-owned lines.
    pub fn commit(&mut self, lines: impl IntoIterator<Item = Line>) {
        self.committed.extend(lines);
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
                self.drain_paragraphs();
            }
            UiEvent::ToolCall { id, name, preview } => {
                self.finish_reply();
                self.status.phase = format!("running {name}");
                self.tools.push(PendingTool { id, name, preview });
            }
            UiEvent::ToolResult {
                id,
                name,
                ok,
                summary,
            } => {
                let index = self
                    .tools
                    .iter()
                    .position(|tool| tool.id == id)
                    .or_else(|| self.tools.iter().position(|tool| tool.name == name));
                let preview = index
                    .map(|index| self.tools.remove(index).preview)
                    .unwrap_or_default();
                let block = self.tool_block(&name, &preview, Some((ok, &summary)));
                self.commit(block);
            }
            UiEvent::Approval {
                tool,
                preview,
                reply,
            } => {
                self.approval = Some(ApprovalPanel {
                    tool,
                    preview,
                    reply,
                    selected: 0,
                });
            }
            UiEvent::Finished(error) => {
                self.finish_reply();
                // Announced but never observed (a cancel mid-batch): shown, not dropped.
                for tool in std::mem::take(&mut self.tools) {
                    let block = self.tool_block(&tool.name, &tool.preview, None);
                    self.commit(block);
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
        }
    }

    /// The two lines of one tool call: `⏺ name(args)` and `⎿ summary`.
    fn tool_block(&self, name: &str, preview: &str, outcome: Option<(bool, &str)>) -> Vec<Line> {
        let (ok, summary) = outcome.unwrap_or((true, "(no result)"));
        let marker = if ok { Style::Green } else { Style::Red };
        let head = Line::styled(marker, "⏺ ")
            .push(Style::Bold, sanitize(name))
            .push(
                Style::Plain,
                format!("({})", ellipsize(&sanitize(preview), ARG_PREVIEW_LIMIT)),
            );
        let body = Line::styled(Style::Dim, "  ⎿  ").push(
            if ok { Style::Dim } else { Style::DimRed },
            ellipsize(&sanitize(summary), SUMMARY_LIMIT),
        );
        vec![
            Line::default(),
            head.truncate(self.width),
            body.truncate(self.width),
        ]
    }

    /// Commit whole paragraphs of the reply, keeping the unfinished tail live.
    fn drain_paragraphs(&mut self) {
        loop {
            if let Some(at) = self.stream.rfind("\n\n") {
                let tail = self.stream.split_off(at + 2);
                let head = std::mem::replace(&mut self.stream, tail);
                self.commit_chunk(&head);
                self.blank_before_chunk = true;
                continue;
            }
            if self.stream.chars().count() <= STREAM_TAIL_LIMIT {
                return;
            }
            // One runaway paragraph: break at the last newline, or anywhere.
            let at = match self.stream.rfind('\n') {
                Some(at) => at + 1,
                None => self
                    .stream
                    .char_indices()
                    .nth(STREAM_TAIL_LIMIT)
                    .map_or(self.stream.len(), |(at, _)| at),
            };
            let tail = self.stream.split_off(at);
            let head = std::mem::replace(&mut self.stream, tail);
            self.commit_chunk(&head);
            self.blank_before_chunk = false;
        }
    }

    /// Commit one chunk of the reply, word-wrapped under the `⏺` marker.
    fn commit_chunk(&mut self, chunk: &str) {
        let mut body = chunk.trim_end_matches('\n');
        if !self.reply_started {
            body = body.trim_start_matches('\n');
        }
        if body.trim().is_empty() {
            return;
        }
        let mut lines = Vec::new();
        if !self.reply_started || self.blank_before_chunk {
            lines.push(Line::default());
        }
        for (index, row) in self.reply_rows(body).into_iter().enumerate() {
            let lead = if index == 0 && !self.reply_started {
                Line::styled(Style::Plain, "⏺ ")
            } else {
                Line::plain("  ")
            };
            lines.push(lead.push(Style::Plain, row));
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

    // ---- keys -------------------------------------------------------------

    /// Handle one key press; `history` is the shared prompt history.
    pub fn on_key(&mut self, key: KeyEvent, history: &[String]) -> Effect {
        self.dirty = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return self.interrupt();
        }
        if self.approval.is_some() {
            self.approval_key(key);
            return Effect::None;
        }
        if key.code == KeyCode::BackTab {
            return Effect::CycleMode;
        }
        if let Some(running) = &mut self.running {
            if key.code == KeyCode::Esc && !running.cancel_requested {
                return self.request_cancel();
            }
            // The prompt is disabled while a turn runs, as in Python.
            return Effect::None;
        }
        self.exit_armed = None;
        let editor = &mut self.editor;
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
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
        Effect::None
    }

    pub fn on_paste(&mut self, text: &str) {
        if self.running.is_none() {
            self.editor.insert(&text.replace("\r\n", "\n"));
            self.dirty = true;
        }
    }

    /// Ctrl+C: deny a pending approval and cancel; cancel a running turn; quit
    /// after it if pressed again; when idle, clear the draft, then offer to quit.
    fn interrupt(&mut self) -> Effect {
        if let Some(panel) = self.approval.take() {
            let _ = panel.reply.send(false);
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
            for tool in &self.tools {
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
        } else {
            let (lines, (row, col)) = self.editor.render(width, self.running.is_none());
            caret_base = bottom.len();
            if self.running.is_none() {
                caret = Some((row, col));
            }
            bottom.extend(lines);
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
        for row in sanitize(detail).split('\n').take(APPROVAL_DETAIL_LINES) {
            content.push(Line::styled(Style::Dim, row));
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
    lines.push(Line::styled(
        Style::Dim,
        "  y to run it · n or esc to deny · ↑↓ and enter to choose",
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn status() -> StatusState {
        StatusState {
            model: "gpt-4o-mini".into(),
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
        });
        assert_eq!(committed_text(&mut app), ["", "⏺ Let me look."]);
        assert!(live_text(&mut app).contains(&r#"⏺ list_dir({"path": "."})"#.to_owned()));
        assert_eq!(app.status.phase, "running list_dir");
        app.on_turn_event(UiEvent::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            ok: false,
            summary: "error: No such directory".into(),
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
    fn the_approval_panel_answers_the_worker() {
        let mut app = app_running();
        let (reply, answer) = mpsc::channel();
        app.on_turn_event(UiEvent::Approval {
            tool: "write_file".into(),
            preview: ApprovalPreview {
                summary: r#"{"path": "a.txt"}"#.into(),
                detail: Some("--- a.txt\n+++ a.txt\n+hello".into()),
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
