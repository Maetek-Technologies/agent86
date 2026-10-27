//! The pickers a bare `needs_choice` command opens, drawn as a panel in place of
//! the input box: Python's `ModePickerModal`, `ModelPickerModal`, and
//! `SessionPickerModal`. Each ends with a value or a cancel, never a hang.

use super::text::{sanitize, Line};
use crate::repl::Style;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Most rows of choices shown at once; the rest scroll.
const PICKER_ROWS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Dispatches `/mode <value>`.
    Mode,
    /// Dispatches `/model <value>`.
    Model,
    /// Loads the session with this id and rebuilds the transcript.
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Open,
    Picked(PickerKind, String),
    Cancelled,
}

pub struct Picker {
    pub kind: PickerKind,
    title: &'static str,
    /// (label, value) pairs.
    choices: Vec<(String, String)>,
    /// The type-to-filter text, for pickers that have one (sessions).
    filter: Option<String>,
    /// Indices into `choices` that pass the filter.
    shown: Vec<usize>,
    /// Index into `shown`.
    selected: usize,
}

impl Picker {
    fn new(
        kind: PickerKind,
        title: &'static str,
        choices: Vec<(String, String)>,
        filter: bool,
    ) -> Self {
        let shown = (0..choices.len()).collect();
        Self {
            kind,
            title,
            choices,
            filter: filter.then(String::new),
            shown,
            selected: 0,
        }
    }

    /// `ask`, `auto`, `deny`, with the current mode highlighted.
    pub fn mode(current: &str) -> Self {
        let choices: Vec<(String, String)> = ["ask", "auto", "deny"]
            .into_iter()
            .map(|mode| (mode.to_owned(), mode.to_owned()))
            .collect();
        let mut picker = Self::new(PickerKind::Mode, "Approval mode", choices, false);
        picker.selected = picker
            .choices
            .iter()
            .position(|(_, value)| value == current)
            .unwrap_or(0);
        picker
    }

    pub fn model(choices: Vec<(String, String)>) -> Self {
        Self::new(PickerKind::Model, "Switch model", choices, false)
    }

    /// Recent sessions as (label, session id), newest first, narrowed by typing.
    pub fn sessions(choices: Vec<(String, String)>) -> Self {
        Self::new(PickerKind::Session, "Resume a session", choices, true)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> PickerOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return PickerOutcome::Cancelled,
            KeyCode::Char('c') if ctrl => return PickerOutcome::Cancelled,
            KeyCode::Enter | KeyCode::Tab => return self.pick(self.selected),
            KeyCode::Up if !self.shown.is_empty() => {
                self.selected = self.selected.checked_sub(1).unwrap_or(self.shown.len() - 1);
            }
            KeyCode::Down if !self.shown.is_empty() => {
                self.selected = (self.selected + 1) % self.shown.len();
            }
            KeyCode::Backspace => {
                if let Some(filter) = &mut self.filter {
                    filter.pop();
                    self.refilter();
                }
            }
            KeyCode::Char(ch) if !ctrl => match &mut self.filter {
                Some(filter) => {
                    filter.push(ch);
                    self.refilter();
                }
                None => {
                    if let Some(index) = ch.to_digit(10).filter(|&digit| digit > 0) {
                        return self.pick(index as usize - 1);
                    }
                }
            },
            _ => {}
        }
        PickerOutcome::Open
    }

    fn pick(&self, index: usize) -> PickerOutcome {
        match self.shown.get(index) {
            Some(&choice) => PickerOutcome::Picked(self.kind, self.choices[choice].1.clone()),
            // Enter with nothing left to pick cancels, as Python's filter does.
            None if self.shown.is_empty() => PickerOutcome::Cancelled,
            None => PickerOutcome::Open,
        }
    }

    /// Python's `_filtered`: the needle in the title or the id, case-insensitive.
    fn refilter(&mut self) {
        let needle = self
            .filter
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        self.shown = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, (label, value))| {
                needle.is_empty()
                    || label.to_lowercase().contains(&needle)
                    || value.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    /// The panel, `width` cells wide.
    pub fn render(&self, width: usize) -> Vec<Line> {
        let inner = width.saturating_sub(4).max(1);
        let mut rows = vec![Line::styled(Style::Bold, self.title)];
        if let Some(filter) = &self.filter {
            rows.push(if filter.is_empty() {
                Line::styled(Style::Dim, "type to filter…")
            } else {
                Line::styled(Style::Dim, "filter: ").push(Style::Plain, sanitize(filter))
            });
        }
        rows.push(Line::default());
        if self.shown.is_empty() {
            rows.push(Line::styled(Style::Dim, "no matches"));
        }
        let numbered = self.filter.is_none();
        let first = self.selected.saturating_sub(PICKER_ROWS - 1);
        let end = self.shown.len().min(first + PICKER_ROWS);
        for (offset, &choice) in self.shown[first..end].iter().enumerate() {
            let position = first + offset;
            // Session titles are the user's own prompts: drawn plain.
            let label = sanitize(&self.choices[choice].0);
            let label = if numbered {
                format!("{}. {label}", position + 1)
            } else {
                label
            };
            rows.push(if position == self.selected {
                Line::styled(Style::BoldCyan, format!("❯ {label}"))
            } else {
                Line::plain(format!("  {label}"))
            });
        }
        if end < self.shown.len() {
            rows.push(Line::styled(
                Style::Dim,
                format!("  … {} more", self.shown.len() - end),
            ));
        }

        let border = Style::Cyan;
        let mut lines = vec![Line::styled(border, format!("╭{}╮", "─".repeat(width - 2)))];
        for row in rows {
            let row = row.truncate(inner);
            let pad = inner.saturating_sub(row.width());
            let mut line = Line::styled(border, "│ ");
            line.spans.extend(row.spans);
            lines.push(line.push(Style::Plain, " ".repeat(pad)).push(border, " │"));
        }
        lines.push(Line::styled(border, format!("╰{}╯", "─".repeat(width - 2))));
        lines.push(
            Line::styled(
                Style::Dim,
                "  ↑↓ to choose · enter to select · esc to cancel",
            )
            .truncate(width),
        );
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain_text).collect()
    }

    #[test]
    fn the_mode_picker_starts_on_the_current_mode() {
        let mut picker = Picker::mode("auto");
        assert!(text(&picker.render(30)).contains(&format!("│ ❯ 2. auto{} │", " ".repeat(17))));
        assert_eq!(picker.on_key(key(KeyCode::Down)), PickerOutcome::Open);
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Mode, "deny".into())
        );
        assert_eq!(
            picker.on_key(key(KeyCode::Char('1'))),
            PickerOutcome::Picked(PickerKind::Mode, "ask".into())
        );
        assert_eq!(picker.on_key(key(KeyCode::Esc)), PickerOutcome::Cancelled);
    }

    #[test]
    fn the_session_picker_filters_by_title_or_id() {
        let mut picker = Picker::sessions(vec![
            (
                "fix the parser · 1a2b3c4d · 2h ago".into(),
                "1a2b3c4d5e".into(),
            ),
            ("write docs · 9f8e7d6c · 1d ago".into(), "9f8e7d6c5b".into()),
        ]);
        for ch in "DOCS".chars() {
            picker.on_key(key(KeyCode::Char(ch)));
        }
        let lines = text(&picker.render(50));
        assert!(lines.iter().any(|line| line.contains("filter: DOCS")));
        assert!(lines.iter().any(|line| line.contains("❯ write docs")));
        assert!(!lines.iter().any(|line| line.contains("fix the parser")));
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Session, "9f8e7d6c5b".into())
        );
        picker.on_key(key(KeyCode::Char('z')));
        assert!(text(&picker.render(50))
            .iter()
            .any(|line| line.contains("no matches")));
        assert_eq!(picker.on_key(key(KeyCode::Enter)), PickerOutcome::Cancelled);
        for _ in 0..5 {
            picker.on_key(key(KeyCode::Backspace));
        }
        picker.on_key(key(KeyCode::Char('1')));
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Session, "1a2b3c4d5e".into()),
            "digits filter (ids hold them) rather than pick"
        );
    }

    #[test]
    fn long_lists_scroll_and_rows_fit_the_width() {
        let choices = (0..30)
            .map(|n| (format!("model-{n} with a long label"), format!("p:m{n}")))
            .collect();
        let mut picker = Picker::model(choices);
        for _ in 0..15 {
            picker.on_key(key(KeyCode::Down));
        }
        let lines = text(&picker.render(24));
        assert!(lines.iter().any(|line| line.contains("❯ 16. model-15")));
        assert!(lines.iter().any(|line| line.contains("… 14 more")));
        assert!(picker.render(24).iter().all(|line| line.width() <= 24));
    }
}
