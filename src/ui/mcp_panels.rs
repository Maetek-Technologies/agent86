//! The `/config mcp` panels, drawn in place of the input box: Python's
//! `MCPManagerModal`, `MCPServerFormModal`, and `MCPTestModal`. Each ends with an
//! explicit answer, never a hang.

use super::mcp_config::{build_manual_config, command_line, parse_server_json, McpDraft, McpRow};
use super::panels::framed;
use super::text::{sanitize, wrap_words, Line};
use crate::repl::Style;
use crate::secrets::redact;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Most rows of a list or the JSON box shown at once.
const ROWS: usize = 10;

fn is_cancel(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
}

fn marked(selected: bool, label: String) -> Line {
    if selected {
        Line::styled(Style::BoldCyan, format!("❯ {label}"))
    } else {
        Line::plain(format!("  {label}"))
    }
}

// ---- the server list --------------------------------------------------------

/// What was chosen in the server list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAction {
    AddJson,
    AddManual,
    Edit(String),
    Remove(String),
    Toggle(String),
}

/// The configured servers, then the two ways to add one. Enter edits, `d`
/// removes, `t` enables or disables.
pub struct McpList {
    rows: Vec<McpRow>,
    selected: usize,
}

impl McpList {
    pub fn new(rows: Vec<McpRow>) -> Self {
        Self { rows, selected: 0 }
    }

    fn highlighted(&self) -> Option<String> {
        self.rows.get(self.selected).map(|row| row.name.clone())
    }

    /// `None` while open; then the action, or `None` for a cancel.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Option<McpAction>> {
        if is_cancel(&key) {
            return Some(None);
        }
        let count = self.rows.len() + 2;
        match key.code {
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(count - 1),
            KeyCode::Down => self.selected = (self.selected + 1) % count,
            KeyCode::Enter => {
                return Some(Some(match self.selected.checked_sub(self.rows.len()) {
                    Some(0) => McpAction::AddJson,
                    Some(_) => McpAction::AddManual,
                    None => McpAction::Edit(self.highlighted()?),
                }))
            }
            KeyCode::Char('d') => {
                return self.highlighted().map(|name| Some(McpAction::Remove(name)))
            }
            KeyCode::Char('t') => {
                return self.highlighted().map(|name| Some(McpAction::Toggle(name)))
            }
            _ => {}
        }
        None
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let mut rows = vec![
            Line::styled(
                Style::Bold,
                "MCP servers — enter to edit, d to remove, t to enable/disable",
            ),
            Line::default(),
        ];
        if self.rows.is_empty() {
            rows.push(Line::styled(Style::Dim, "No MCP servers configured yet."));
        }
        let mut labels: Vec<String> = self.rows.iter().map(|row| sanitize(&row.label())).collect();
        labels.push("+ Add from JSON (paste a server block)".to_owned());
        labels.push("+ Add manually (name, command or URL)".to_owned());
        let first = self.selected.saturating_sub(ROWS - 1);
        let end = labels.len().min(first + ROWS);
        for (offset, label) in labels[first..end].iter().enumerate() {
            rows.push(marked(first + offset == self.selected, label.clone()));
        }
        if end < labels.len() {
            rows.push(Line::styled(
                Style::Dim,
                format!("  … {} more", labels.len() - end),
            ));
        }
        framed(
            rows,
            width,
            "↑↓ to choose · enter to edit or add · d remove · t enable/disable · esc to cancel",
        )
    }
}

// ---- the add and edit form --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Json,
    Command,
    Url,
    Env,
    Headers,
    Transport,
    Buttons,
}

/// One form for both add paths and editing: a pasted JSON block, or the fields.
/// The config model validates on every change; its message shows under the
/// fields and Continue stays off until the whole server parses. Nothing typed is
/// thrown away.
pub struct McpForm {
    json_mode: bool,
    title: &'static str,
    existing: Vec<String>,
    original: Option<String>,
    name: String,
    json: String,
    command: String,
    url: String,
    env: String,
    headers: String,
    sse: bool,
    focus: Field,
    /// 0 is Continue, 1 is Cancel.
    button: usize,
    current: Result<McpDraft, String>,
}

impl McpForm {
    /// An empty form; `json_mode` for the paste-a-block path.
    pub fn add(json_mode: bool, existing: Vec<String>) -> Self {
        let title = if json_mode {
            "Add MCP server — paste JSON"
        } else {
            "Add MCP server — enter fields"
        };
        let mut form = Self {
            json_mode,
            title,
            existing,
            original: None,
            name: String::new(),
            json: String::new(),
            command: String::new(),
            url: String::new(),
            env: String::new(),
            headers: String::new(),
            sse: false,
            focus: Field::Name,
            button: 0,
            current: Err(String::new()),
        };
        form.revalidate();
        form
    }

    /// The field form, filled in from a configured server.
    pub fn edit(draft: &McpDraft, existing: Vec<String>) -> Self {
        let mut form = Self::add(false, existing);
        form.title = "Edit MCP server";
        form.original = draft.original_name.clone();
        form.name = draft.name.clone();
        let config = &draft.config;
        form.command = command_line(config);
        form.url = config.url.clone().unwrap_or_default();
        let joined = |pairs: &indexmap::IndexMap<String, String>| {
            pairs
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        form.env = joined(&config.env);
        form.headers = joined(&config.headers);
        form.sse = config.transport() == "sse";
        form.revalidate();
        form
    }

    /// The fields in order; the transport choice only once a URL is typed.
    fn fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Name];
        if self.json_mode {
            fields.push(Field::Json);
        } else {
            fields.extend([Field::Command, Field::Url, Field::Env, Field::Headers]);
            if !self.url.trim().is_empty() {
                fields.push(Field::Transport);
            }
        }
        fields.push(Field::Buttons);
        fields
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Field::Name => Some(&mut self.name),
            Field::Json => Some(&mut self.json),
            Field::Command => Some(&mut self.command),
            Field::Url => Some(&mut self.url),
            Field::Env => Some(&mut self.env),
            Field::Headers => Some(&mut self.headers),
            Field::Transport | Field::Buttons => None,
        }
    }

    fn move_focus(&mut self, step: isize) {
        let fields = self.fields();
        let at = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0) as isize;
        let next = (at + step).rem_euclid(fields.len() as isize) as usize;
        self.focus = fields[next];
    }

    fn revalidate(&mut self) {
        self.current = self.build();
    }

    fn build(&self) -> Result<McpDraft, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Enter a name for this server.".to_owned());
        }
        let taken = |candidate: &str| {
            self.existing.iter().any(|known| known == candidate)
                && self.original.as_deref() != Some(candidate)
        };
        if taken(name) {
            return Err(format!(
                "A server named '{name}' already exists. Pick another name."
            ));
        }
        if self.json_mode {
            let (found, config) = parse_server_json(&self.json, name)?;
            if taken(&found) {
                return Err(format!(
                    "A server named '{found}' already exists. Pick another name."
                ));
            }
            return Ok(McpDraft {
                name: found,
                config,
                original_name: self.original.clone(),
            });
        }
        let transport =
            (!self.url.trim().is_empty()).then_some(if self.sse { "sse" } else { "http" });
        let config = build_manual_config(
            &self.command,
            &self.url,
            &self.env,
            &self.headers,
            transport,
        )?;
        Ok(McpDraft {
            name: name.to_owned(),
            config,
            original_name: self.original.clone(),
        })
    }

    /// `None` while open; then the validated server, or `None` for a cancel.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Option<McpDraft>> {
        if is_cancel(&key) {
            return Some(None);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match (self.focus, key.code) {
            (_, KeyCode::Tab) => self.move_focus(1),
            (_, KeyCode::BackTab) => self.move_focus(-1),
            (_, KeyCode::Down) => self.move_focus(1),
            (_, KeyCode::Up) => self.move_focus(-1),
            // The JSON box takes newlines; Enter in a one-line field submits.
            (Field::Json, KeyCode::Enter) => self.json.push('\n'),
            (Field::Buttons, KeyCode::Left | KeyCode::Right) => self.button = 1 - self.button,
            (Field::Buttons, KeyCode::Enter) if self.button == 1 => return Some(None),
            (Field::Transport, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) => {
                self.sse = !self.sse;
            }
            (_, KeyCode::Enter) if !shift => {
                if let Ok(draft) = &self.current {
                    return Some(Some(draft.clone()));
                }
            }
            (_, KeyCode::Backspace) => {
                if let Some(text) = self.text_mut() {
                    text.pop();
                }
            }
            (_, KeyCode::Char('u')) if ctrl => {
                if let Some(text) = self.text_mut() {
                    text.clear();
                }
            }
            (_, KeyCode::Char(ch)) if !ctrl => {
                if let Some(text) = self.text_mut() {
                    text.push(ch);
                }
            }
            _ => {}
        }
        // The transport choice disappears with the URL.
        if !self.fields().contains(&self.focus) {
            self.focus = Field::Url;
        }
        self.revalidate();
        None
    }

    /// A paste goes into the focused field; only the JSON box keeps its newlines.
    pub fn on_paste(&mut self, text: &str) {
        let json = self.focus == Field::Json;
        if let Some(field) = self.text_mut() {
            let text = text.replace("\r\n", "\n");
            if json {
                field.push_str(&text);
            } else {
                field.push_str(&text.replace('\n', " "));
            }
        }
        self.revalidate();
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let inner = width.saturating_sub(4).max(1);
        let mut rows = vec![Line::styled(Style::Bold, self.title), Line::default()];
        let field = |focus: Field, label: &str, value: &str, placeholder: &str| {
            let lead = if self.focus == focus { "❯ " } else { "  " };
            let line = Line::styled(Style::Cyan, lead).push(Style::Dim, format!("{label:<8} "));
            if value.is_empty() {
                line.push(Style::Dim, placeholder.to_owned())
            } else {
                line.push(Style::Plain, sanitize(value))
            }
        };
        rows.push(field(
            Field::Name,
            "name",
            &self.name,
            "server name (required)",
        ));
        if self.json_mode {
            let lead = if self.focus == Field::Json {
                "❯ "
            } else {
                "  "
            };
            rows.push(Line::styled(Style::Cyan, lead).push(Style::Dim, "json"));
            // Long lines wrap inside the box rather than run off its edge.
            let lines: Vec<String> = sanitize(&self.json)
                .split('\n')
                .flat_map(|line| {
                    Line::plain(line)
                        .hard_wrap(inner.saturating_sub(4).max(1))
                        .into_iter()
                        .map(|row| row.spans.into_iter().map(|(_, text)| text).collect())
                        .collect::<Vec<_>>()
                })
                .collect();
            if self.json.is_empty() {
                rows.push(Line::styled(Style::Dim, "  │ paste the server's JSON here"));
            }
            let skip = lines.len().saturating_sub(ROWS);
            if skip > 0 {
                rows.push(Line::styled(
                    Style::Dim,
                    format!("  │ … {skip} lines above"),
                ));
            }
            if !self.json.is_empty() {
                for line in &lines[skip..] {
                    rows.push(Line::styled(Style::Dim, "  │ ").push(Style::Plain, line.clone()));
                }
            }
        } else {
            rows.push(field(
                Field::Command,
                "command",
                &self.command,
                "command (e.g. npx -y server-name)",
            ));
            rows.push(field(
                Field::Url,
                "url",
                &self.url,
                "url (for sse/http transports)",
            ));
            rows.push(field(
                Field::Env,
                "env",
                &self.env,
                "KEY=value, OTHER=${VAR}",
            ));
            rows.push(field(Field::Headers, "headers", &self.headers, "KEY=value"));
            if !self.url.trim().is_empty() {
                let lead = if self.focus == Field::Transport {
                    "❯ "
                } else {
                    "  "
                };
                let (http, sse) = if self.sse {
                    ("( )", "(•)")
                } else {
                    ("(•)", "( )")
                };
                rows.push(
                    Line::styled(Style::Cyan, lead)
                        .push(Style::Dim, "transport ")
                        .push(Style::Plain, format!("{http} http  {sse} sse")),
                );
            }
        }
        rows.push(Line::default());
        match &self.current {
            Err(error) if !error.is_empty() => {
                for row in sanitize(error).split('\n') {
                    rows.extend(
                        wrap_words(row, inner)
                            .into_iter()
                            .map(|row| Line::styled(Style::Red, row)),
                    );
                }
            }
            _ => rows.push(Line::default()),
        }
        let on_buttons = self.focus == Field::Buttons;
        let button = |index: usize, label: &str, enabled: bool| {
            let style = if !enabled {
                Style::Dim
            } else if on_buttons && self.button == index {
                Style::BoldCyan
            } else {
                Style::Plain
            };
            (style, format!("[ {label} ]"))
        };
        let (continue_style, continue_label) = button(0, "Continue", self.current.is_ok());
        let (cancel_style, cancel_label) = button(1, "Cancel", true);
        rows.push(
            Line::styled(Style::Cyan, if on_buttons { "❯ " } else { "  " })
                .push(continue_style, continue_label)
                .push(Style::Plain, "  ")
                .push(cancel_style, cancel_label),
        );
        framed(
            rows,
            width,
            "tab or ↑↓ between fields · enter to continue · esc to cancel",
        )
    }
}

// ---- the MCP test -------------------------------------------------------------

/// How an MCP test ended. `override_` is the explicit "Save anyway".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTestOutcome {
    pub ok: bool,
    /// (tool name, description), shown before anything is saved.
    pub tools: Vec<(String, String)>,
    pub error: Option<String>,
    pub override_: bool,
}

enum TestState {
    Running,
    Connected(Vec<(String, String)>),
    Failed(String),
}

/// Start the server for real and list its tools before anything is saved. The
/// driver's worker does the starting; a test closed before it finishes raises
/// `abandoned`, so the worker stops the server it started.
pub struct McpTest {
    pub id: u64,
    name: String,
    transport: String,
    pub abandoned: Arc<AtomicBool>,
    state: TestState,
    selected: usize,
}

impl McpTest {
    pub fn new(id: u64, name: &str, transport: &str) -> Self {
        Self {
            id,
            name: name.to_owned(),
            transport: transport.to_owned(),
            abandoned: Arc::new(AtomicBool::new(false)),
            state: TestState::Running,
            selected: 0,
        }
    }

    pub fn finish(&mut self, result: Result<Vec<(String, String)>, String>) {
        self.state = match result {
            Ok(tools) => TestState::Connected(tools),
            Err(error) => TestState::Failed(error),
        };
        self.selected = 0;
    }

    fn error(&self) -> Option<String> {
        match &self.state {
            TestState::Failed(error) => Some(error.clone()),
            _ => None,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<McpTestOutcome> {
        let failed = |error: Option<String>, override_| McpTestOutcome {
            ok: false,
            tools: Vec::new(),
            error,
            override_,
        };
        if is_cancel(&key) {
            self.abandoned.store(true, Ordering::SeqCst);
            return Some(failed(self.error(), false));
        }
        match &self.state {
            TestState::Running => None,
            TestState::Connected(tools) => match key.code {
                KeyCode::Enter | KeyCode::Char('1') => Some(McpTestOutcome {
                    ok: true,
                    tools: tools.clone(),
                    error: None,
                    override_: false,
                }),
                _ => None,
            },
            TestState::Failed(error) => match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                    self.selected = 1 - self.selected;
                    None
                }
                KeyCode::Char('1') => Some(failed(Some(error.clone()), true)),
                KeyCode::Char('2') => Some(failed(Some(error.clone()), false)),
                KeyCode::Enter => Some(failed(Some(error.clone()), self.selected == 0)),
                _ => None,
            },
        }
    }

    pub fn render(&self, width: usize, spinner: &str) -> Vec<Line> {
        let inner = width.saturating_sub(4).max(1);
        let mut rows = vec![
            Line::styled(
                Style::Bold,
                format!(
                    "Connecting to {} ({})…",
                    sanitize(&self.name),
                    sanitize(&self.transport)
                ),
            ),
            Line::default(),
        ];
        let wrapped = |text: &str, style: Style, rows: &mut Vec<Line>| {
            for row in sanitize(&redact(text, &[])).split('\n') {
                rows.extend(
                    wrap_words(row, inner)
                        .into_iter()
                        .map(|row| Line::styled(style, row)),
                );
            }
        };
        let hint = match &self.state {
            TestState::Running => {
                rows.push(
                    Line::styled(Style::Cyan, format!("{spinner} "))
                        .push(Style::Plain, "Starting the server and listing its tools…"),
                );
                "esc to cancel"
            }
            TestState::Connected(tools) => {
                let headline = match tools.len() {
                    0 => "Connected. No tools exposed.".to_owned(),
                    1 => "Connected. 1 tool:".to_owned(),
                    count => format!("Connected. {count} tools:"),
                };
                rows.push(Line::styled(Style::Green, headline));
                for (name, description) in tools.iter().take(ROWS) {
                    let first = description.lines().next().unwrap_or_default();
                    rows.push(Line::plain(sanitize(&redact(name, &[]))).push(
                        Style::Dim,
                        format!("  —  {}", sanitize(&redact(first, &[]))),
                    ));
                }
                if tools.len() > ROWS {
                    rows.push(Line::styled(
                        Style::Dim,
                        format!("… {} more", tools.len() - ROWS),
                    ));
                }
                rows.push(Line::default());
                rows.push(marked(true, "1. Continue".to_owned()));
                "enter to continue · esc to cancel"
            }
            TestState::Failed(error) => {
                rows.push(Line::styled(Style::Red, "Test failed:"));
                wrapped(error, Style::Plain, &mut rows);
                rows.push(Line::default());
                rows.push(marked(self.selected == 0, "1. Save anyway".to_owned()));
                rows.push(marked(self.selected == 1, "2. Cancel".to_owned()));
                "↑↓ to choose · enter to select · esc to cancel"
            }
        };
        framed(rows, width, hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(Line::plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn typed(form: &mut McpForm, text: &str) {
        for ch in text.chars() {
            form.on_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn the_list_offers_each_server_then_the_two_adds() {
        let row = McpRow {
            name: "gh".into(),
            transport: "stdio".into(),
            endpoint: "npx -y gh".into(),
            enabled: false,
        };
        let mut list = McpList::new(vec![row]);
        let drawn = text(&list.render(90));
        assert!(
            drawn.contains("❯ gh  stdio  npx -y gh  (disabled)"),
            "{drawn}"
        );
        assert!(drawn.contains("+ Add from JSON (paste a server block)"));
        assert_eq!(
            list.on_key(key(KeyCode::Char('t'))),
            Some(Some(McpAction::Toggle("gh".into())))
        );
        assert_eq!(
            list.on_key(key(KeyCode::Char('d'))),
            Some(Some(McpAction::Remove("gh".into())))
        );
        list.on_key(key(KeyCode::Down));
        assert_eq!(
            list.on_key(key(KeyCode::Char('d'))),
            None,
            "not on a server"
        );
        assert_eq!(
            list.on_key(key(KeyCode::Enter)),
            Some(Some(McpAction::AddJson))
        );
        list.on_key(key(KeyCode::Down));
        assert_eq!(
            list.on_key(key(KeyCode::Enter)),
            Some(Some(McpAction::AddManual))
        );
        assert_eq!(list.on_key(key(KeyCode::Esc)), Some(None));
        assert!(
            text(&McpList::new(Vec::new()).render(90)).contains("No MCP servers configured yet.")
        );
    }

    #[test]
    fn the_form_validates_as_it_goes_and_keeps_what_was_typed() {
        let mut form = McpForm::add(false, vec!["taken".into()]);
        assert!(text(&form.render(80)).contains("Enter a name for this server."));
        assert_eq!(form.on_key(key(KeyCode::Enter)), None, "Continue is off");
        typed(&mut form, "taken");
        assert!(text(&form.render(80)).contains("A server named 'taken' already exists."));
        for _ in 0..5 {
            form.on_key(key(KeyCode::Backspace));
        }
        typed(&mut form, "docs");
        assert!(text(&form.render(80)).contains("MCP server needs 'command' (stdio) or 'url'"));
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, "http://h/sse");
        assert!(text(&form.render(80)).contains("(•) http  ( ) sse"));
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Right));
        let Some(Some(draft)) = form.on_key(key(KeyCode::Enter)) else {
            panic!("the form should submit");
        };
        assert_eq!(
            (draft.name.as_str(), draft.config.transport()),
            ("docs", "sse")
        );

        // Clearing the URL takes the transport choice away and keeps the focus sane.
        let draft = McpDraft {
            original_name: Some("docs".into()),
            ..draft
        };
        let mut form = McpForm::edit(&draft, vec!["docs".into()]);
        assert!(text(&form.render(80)).contains("Edit MCP server"));
        assert!(form.current.is_ok(), "its own name is not taken");
        form.focus = Field::Url;
        form.on_key(key(KeyCode::Char('u')).with_ctrl());
        assert!(!form.fields().contains(&Field::Transport));
    }

    #[test]
    fn the_json_form_takes_a_pasted_block() {
        // Python asks for a name even when the block carries one; the block's wins.
        let mut form = McpForm::add(true, Vec::new());
        typed(&mut form, "typed");
        form.on_key(key(KeyCode::Tab));
        form.on_paste("{\"mcpServers\": {\"fs\": {\r\n  \"command\": \"npx\"\r\n}}}");
        let drawn = text(&form.render(80));
        assert!(drawn.contains("│   \"command\": \"npx\""), "{drawn}");
        // A long line wraps inside the box, so nothing is cut off.
        let narrow = form.render(30);
        assert!(narrow.iter().all(|line| line.width() <= 30));
        assert!(
            text(&narrow).contains("│ {\"mcpServers\": {\""),
            "{}",
            text(&narrow)
        );
        form.on_key(key(KeyCode::Enter));
        assert!(
            form.json.ends_with("}}}\n"),
            "Enter adds a line in the JSON box"
        );
        form.on_key(key(KeyCode::BackTab));
        assert_eq!(form.focus, Field::Name);
        let Some(Some(draft)) = form.on_key(key(KeyCode::Enter)) else {
            panic!("the wrapped block names the server");
        };
        assert_eq!(draft.name, "fs");
    }

    #[test]
    fn the_test_shows_the_tools_or_offers_save_anyway() {
        let mut test = McpTest::new(1, "gh", "stdio");
        assert!(text(&test.render(70, "✻")).contains("Connecting to gh (stdio)…"));
        assert_eq!(test.on_key(key(KeyCode::Enter)), None);
        test.finish(Ok(vec![("mcp__gh__echo".into(), "Echo it\nback".into())]));
        let drawn = text(&test.render(70, "✻"));
        assert!(
            drawn.contains("Connected. 1 tool:") && drawn.contains("mcp__gh__echo  —  Echo it")
        );
        assert_eq!(
            test.on_key(key(KeyCode::Enter))
                .map(|outcome| outcome.tools.len()),
            Some(1)
        );

        let mut failing = McpTest::new(2, "gh", "http");
        failing.finish(Err("401 for sk-abcdefghijklmnopqrstuvwxyz".into()));
        let drawn = text(&failing.render(70, "✻"));
        assert!(
            drawn.contains("Test failed:") && !drawn.contains("sk-abcdefghij"),
            "{drawn}"
        );
        assert_eq!(
            failing
                .on_key(key(KeyCode::Enter))
                .map(|outcome| outcome.override_),
            Some(true)
        );

        let mut cancelled = McpTest::new(3, "gh", "stdio");
        let outcome = cancelled.on_key(key(KeyCode::Esc)).unwrap();
        assert_eq!((outcome.ok, outcome.error), (false, None));
        assert!(cancelled.abandoned.load(Ordering::SeqCst));
    }

    trait Ctrl {
        fn with_ctrl(self) -> Self;
    }

    impl Ctrl for KeyEvent {
        fn with_ctrl(mut self) -> Self {
            self.modifiers = KeyModifiers::CONTROL;
            self
        }
    }
}
