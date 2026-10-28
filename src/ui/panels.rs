//! The `/config model` panels, drawn in place of the input box like the pickers:
//! Python's `KeyEntryModal`, `ConnectionTestModal`, and `SaveDiffModal`. Each ends
//! with an explicit answer, never a hang.

use super::diff::detail_line;
use super::text::{sanitize, wrap_words, Line};
use crate::config::ConfigPaths;
use crate::config_writer::{plan_edit, Change, ConfigEdit, Scope};
use crate::repl::Style;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Most diff rows the save panel shows at once; the rest scroll.
const DIFF_ROWS: usize = 12;

/// Rows inside a rounded cyan border, `width` cells wide, then a dim key hint.
pub fn framed(rows: Vec<Line>, width: usize, hint: &str) -> Vec<Line> {
    let inner = width.saturating_sub(4).max(1);
    let border = Style::Cyan;
    let mut lines = vec![Line::styled(
        border,
        format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
    )];
    for row in rows {
        let row = row.truncate(inner);
        let pad = inner.saturating_sub(row.width());
        let mut line = Line::styled(border, "│ ");
        line.spans.extend(row.spans);
        lines.push(line.push(Style::Plain, " ".repeat(pad)).push(border, " │"));
    }
    lines.push(Line::styled(
        border,
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
    ));
    lines.push(Line::styled(Style::Dim, format!("  {hint}")).truncate(width));
    lines
}

/// Two numbered choices with the highlighted one marked.
fn choices(labels: &[&str], selected: usize) -> Vec<Line> {
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            if index == selected {
                Line::styled(Style::BoldCyan, format!("❯ {}. {label}", index + 1))
            } else {
                Line::plain(format!("  {}. {label}", index + 1))
            }
        })
        .collect()
}

fn is_cancel(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
}

// ---- key entry -------------------------------------------------------------

/// Which chain a key entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPurpose {
    /// A provider's API key (`/config model`).
    Provider,
    /// A `${VAR}` an MCP server needs (`/config mcp`).
    McpVar,
}

/// Which chain a save belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavePurpose {
    /// `/config model`.
    Model,
    /// An MCP server added, edited, or re-enabled.
    McpServer,
    /// An MCP server removed or disabled.
    McpUnmount,
    /// `/limits save`.
    Limits,
}

/// Masked entry of one API key. The key is never drawn: only one dot per
/// character. It goes back to the caller and is held in memory until the
/// connection test passes.
pub struct KeyEntry {
    pub purpose: KeyPurpose,
    pub provider: String,
    keyring_ok: bool,
    text: String,
}

impl KeyEntry {
    pub fn new(purpose: KeyPurpose, provider: &str, keyring_ok: bool) -> Self {
        Self {
            purpose,
            provider: provider.to_owned(),
            keyring_ok,
            text: String::new(),
        }
    }

    /// `None` while open; then `Some(key)`, where an empty or cancelled entry is `None`.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Option<String>> {
        if is_cancel(&key) {
            return Some(None);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let value = self.text.trim().to_owned();
                return Some((!value.is_empty()).then_some(value));
            }
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Char('u') if ctrl => self.text.clear(),
            KeyCode::Char(ch) if !ctrl => self.text.push(ch),
            _ => {}
        }
        None
    }

    pub fn on_paste(&mut self, text: &str) {
        self.text.extend(text.chars().filter(|ch| !ch.is_control()));
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let field = if self.text.is_empty() {
            Line::styled(Style::Cyan, "> ").push(Style::Dim, "paste key (never shown)")
        } else {
            Line::styled(Style::Cyan, "> ")
                .push(Style::Plain, "•".repeat(self.text.chars().count()))
        };
        let note = if self.keyring_ok {
            "Stored in the OS keyring after a successful connection test."
        } else {
            "OS keyring unavailable — this key will only be used for this test."
        };
        framed(
            vec![
                Line::styled(
                    Style::Bold,
                    format!("API key for {}", sanitize(&self.provider)),
                ),
                Line::default(),
                field,
                Line::default(),
                Line::styled(Style::Dim, note),
            ],
            width,
            "enter to continue · esc to cancel",
        )
    }
}

// ---- connection test -------------------------------------------------------

/// How a connection test ended. `override_` is the explicit "Save anyway".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestOutcome {
    pub ok: bool,
    pub error: Option<String>,
    pub override_: bool,
}

/// A live one-token completion against the chosen ref, run by the driver on a
/// worker. The panel waits for its result, then either closes (a pass or a
/// timeout) or shows the error and offers "Save anyway".
pub struct ConnectionTest {
    /// Matches the worker's result to this panel, so a late answer for a panel
    /// already closed is ignored.
    pub id: u64,
    reference: String,
    error: Option<String>,
    selected: usize,
}

impl ConnectionTest {
    pub fn new(id: u64, reference: &str) -> Self {
        Self {
            id,
            reference: reference.to_owned(),
            error: None,
            selected: 0,
        }
    }

    /// The worker answered: a pass or a timeout ends the test; a failure is shown.
    pub fn finish(&mut self, result: TestResult) -> Option<TestOutcome> {
        match result {
            TestResult::Passed => Some(TestOutcome {
                ok: true,
                error: None,
                override_: false,
            }),
            // Nothing for the user to act on, so it resolves at once.
            TestResult::TimedOut(message) => Some(TestOutcome {
                ok: false,
                error: Some(message),
                override_: false,
            }),
            TestResult::Failed(message) => {
                self.error = Some(message);
                None
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<TestOutcome> {
        let failed = |override_| TestOutcome {
            ok: false,
            error: self.error.clone(),
            override_,
        };
        if is_cancel(&key) {
            return Some(TestOutcome {
                ok: false,
                error: Some(self.error.clone().unwrap_or_else(|| "cancelled".to_owned())),
                override_: false,
            });
        }
        // The choices appear only once the test has failed.
        self.error.as_ref()?;
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                self.selected = 1 - self.selected;
                None
            }
            KeyCode::Char('1') => Some(failed(true)),
            KeyCode::Char('2') => Some(failed(false)),
            KeyCode::Enter => Some(failed(self.selected == 0)),
            _ => None,
        }
    }

    pub fn render(&self, width: usize, spinner: &str) -> Vec<Line> {
        let inner = width.saturating_sub(4).max(1);
        let mut rows = vec![
            Line::styled(
                Style::Bold,
                format!("Testing {}", sanitize(&self.reference)),
            ),
            Line::default(),
        ];
        match &self.error {
            None => rows.push(
                Line::styled(Style::Cyan, format!("{spinner} "))
                    .push(Style::Plain, "Sending a 1-token probe…"),
            ),
            Some(error) => {
                rows.push(Line::styled(Style::Red, "Test failed:"));
                for row in sanitize(error).split('\n') {
                    rows.extend(wrap_words(row, inner).into_iter().map(Line::plain));
                }
                rows.push(Line::default());
                rows.extend(choices(&["Save anyway", "Cancel"], self.selected));
            }
        }
        let hint = if self.error.is_some() {
            "↑↓ to choose · enter to select · esc to cancel"
        } else {
            "esc to cancel"
        };
        framed(rows, width, hint)
    }
}

/// What the test's worker reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestResult {
    Passed,
    Failed(String),
    TimedOut(String),
}

// ---- save diff -------------------------------------------------------------

/// The scope choice and the exact diff before anything is written: the user scope
/// is chosen first, the project one is a key away. Nothing touches the disk here;
/// the driver applies the edit this answers with.
pub struct SaveDiff {
    pub purpose: SavePurpose,
    changes: Vec<Change>,
    paths: ConfigPaths,
    scope: Scope,
    /// The planned edit, or why there is none (Save is then disabled).
    edit: Result<ConfigEdit, String>,
    /// The first diff row shown.
    top: usize,
}

impl SaveDiff {
    pub fn new(purpose: SavePurpose, changes: Vec<Change>, paths: ConfigPaths) -> Self {
        let mut panel = Self {
            purpose,
            changes,
            paths,
            scope: Scope::User,
            edit: Err(String::new()),
            top: 0,
        };
        panel.refresh(Scope::User);
        panel
    }

    fn refresh(&mut self, scope: Scope) {
        self.scope = scope;
        self.top = 0;
        self.edit = plan_edit(scope, &self.paths, &self.changes).map_err(|error| error.0);
    }

    fn body(&self) -> Vec<String> {
        match &self.edit {
            Err(error) => sanitize(error).split('\n').map(str::to_owned).collect(),
            Ok(edit) if edit.is_noop() => vec!["No changes.".to_owned()],
            Ok(edit) => sanitize(&edit.diff)
                .split('\n')
                .map(str::to_owned)
                .collect(),
        }
    }

    /// `None` while open; then the edit to apply, or `None` for a cancel.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Option<ConfigEdit>> {
        if is_cancel(&key) {
            return Some(None);
        }
        let rows = self.body().len();
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                let other = match self.scope {
                    Scope::User => Scope::Project,
                    Scope::Project => Scope::User,
                };
                self.refresh(other);
            }
            KeyCode::Char('1') => self.refresh(Scope::User),
            KeyCode::Char('2') => self.refresh(Scope::Project),
            KeyCode::Up => self.top = self.top.saturating_sub(1),
            KeyCode::Down => self.top = (self.top + 1).min(rows.saturating_sub(DIFF_ROWS)),
            KeyCode::PageUp => self.top = self.top.saturating_sub(DIFF_ROWS),
            KeyCode::PageDown => {
                self.top = (self.top + DIFF_ROWS).min(rows.saturating_sub(DIFF_ROWS));
            }
            // Save is disabled while the edit cannot be planned.
            KeyCode::Enter => return self.edit.as_ref().ok().map(|edit| Some(edit.clone())),
            _ => {}
        }
        None
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let radio = |scope: Scope, label: &str| {
            let (mark, style) = if self.scope == scope {
                ("(•)", Style::BoldCyan)
            } else {
                ("( )", Style::Plain)
            };
            Line::styled(style, format!("{mark} {label}"))
        };
        let mut rows = vec![
            Line::styled(Style::Bold, "Save configuration"),
            Line::default(),
            radio(Scope::User, "user  (~/.agent86/config.toml)"),
            radio(Scope::Project, "project  (./.agent86/config.toml)"),
            Line::default(),
        ];
        match &self.edit {
            Ok(edit) => rows.push(Line::styled(
                Style::Dim,
                format!("target: {}", sanitize(&edit.path.display().to_string())),
            )),
            Err(_) => rows.push(Line::default()),
        }
        let body = self.body();
        let end = body.len().min(self.top + DIFF_ROWS);
        if self.top > 0 {
            rows.push(Line::styled(Style::Dim, format!("… {} above", self.top)));
        }
        for row in &body[self.top..end] {
            rows.push(match &self.edit {
                Ok(edit) if !edit.is_noop() => detail_line(row, Some("diff")),
                Ok(_) => Line::styled(Style::Dim, row.as_str()),
                Err(_) => Line::styled(Style::Red, row.as_str()),
            });
        }
        if end < body.len() {
            rows.push(Line::styled(
                Style::Dim,
                format!("… {} more lines", body.len() - end),
            ));
        }
        let hint = if self.edit.is_ok() {
            "←→ scope · ↑↓ scroll · enter to save · esc to cancel"
        } else {
            "←→ scope · esc to cancel"
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

    #[test]
    fn a_key_is_never_drawn_and_an_empty_one_is_no_answer() {
        let mut entry = KeyEntry::new(KeyPurpose::Provider, "groq", true);
        assert!(text(&entry.render(70)).contains("paste key (never shown)"));
        entry.on_paste("gsk_secret\r\n");
        entry.on_key(key(KeyCode::Char('9')));
        let drawn = text(&entry.render(70));
        assert!(!drawn.contains("secret"), "{drawn}");
        assert!(drawn.contains(&"•".repeat(11)));
        assert!(drawn.contains("Stored in the OS keyring after a successful connection test."));
        assert_eq!(
            entry.on_key(key(KeyCode::Enter)),
            Some(Some("gsk_secret9".to_owned()))
        );
        let mut blank = KeyEntry::new(KeyPurpose::Provider, "groq", false);
        assert!(text(&blank.render(80)).contains("OS keyring unavailable"));
        blank.on_key(key(KeyCode::Char(' ')));
        assert_eq!(blank.on_key(key(KeyCode::Enter)), Some(None));
        assert_eq!(blank.on_key(key(KeyCode::Esc)), Some(None));
    }

    #[test]
    fn a_failed_test_offers_save_anyway() {
        let mut test = ConnectionTest::new(1, "groq:llama");
        assert!(text(&test.render(60, "✻")).contains("Sending a 1-token probe…"));
        assert_eq!(
            test.on_key(key(KeyCode::Enter)),
            None,
            "nothing to pick yet"
        );
        assert_eq!(test.finish(TestResult::Failed("HTTP 401".into())), None);
        let drawn = text(&test.render(60, "✻"));
        assert!(drawn.contains("Test failed:") && drawn.contains("HTTP 401"));
        assert!(drawn.contains("❯ 1. Save anyway"));
        assert_eq!(
            test.on_key(key(KeyCode::Enter)),
            Some(TestOutcome {
                ok: false,
                error: Some("HTTP 401".into()),
                override_: true
            })
        );
        assert_eq!(
            test.on_key(key(KeyCode::Char('2')))
                .map(|outcome| outcome.override_),
            Some(false)
        );

        let mut pending = ConnectionTest::new(2, "groq:llama");
        assert_eq!(
            pending
                .on_key(key(KeyCode::Esc))
                .and_then(|outcome| outcome.error),
            Some("cancelled".to_owned())
        );
        assert_eq!(
            pending.finish(TestResult::TimedOut(
                "Timed out after 15s — no response.".into()
            )),
            Some(TestOutcome {
                ok: false,
                error: Some("Timed out after 15s — no response.".into()),
                override_: false
            })
        );
        assert!(pending
            .finish(TestResult::Passed)
            .is_some_and(|outcome| outcome.ok));
    }

    #[test]
    fn the_save_panel_shows_the_diff_for_the_chosen_scope() {
        let root = std::env::temp_dir().join(format!("agent86-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = ConfigPaths {
            user: Some(root.join("user.toml")),
            project: root.join("project.toml"),
        };
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&paths.project, "[model]\ndefault = \"a:b\"\n").unwrap();
        let changes = vec![(
            vec!["model".to_owned(), "default".to_owned()],
            Some(toml_edit::Value::from("a:b")),
        )];
        let mut panel = SaveDiff::new(SavePurpose::Model, changes, paths.clone());
        let drawn = text(&panel.render(300));
        assert!(drawn.contains("(•) user"), "{drawn}");
        assert!(drawn.contains("+default = \"a:b\""));
        assert!(drawn.contains(&format!("target: {}", root.join("user.toml").display())));

        panel.on_key(key(KeyCode::Right));
        let drawn = text(&panel.render(70));
        assert!(drawn.contains("(•) project") && drawn.contains("No changes."));
        let edit = panel.on_key(key(KeyCode::Enter)).unwrap().unwrap();
        assert!(edit.is_noop());
        assert_eq!(panel.on_key(key(KeyCode::Esc)), Some(None));

        let refused = vec![(
            vec!["providers".to_owned(), "x".to_owned(), "token".to_owned()],
            Some(toml_edit::Value::from("plain")),
        )];
        let mut panel = SaveDiff::new(SavePurpose::Model, refused, paths);
        assert!(text(&panel.render(70)).contains("refusing to write 'providers.x.token'"));
        assert_eq!(panel.on_key(key(KeyCode::Enter)), None, "Save is disabled");
        let _ = std::fs::remove_dir_all(root);
    }
}
