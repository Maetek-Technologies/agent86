//! The pickers drawn as a panel in place of the input box: Python's
//! `ModePickerModal`, `ModelPickerModal`, `SessionPickerModal`, and, for
//! `/config model`, `ProviderManagerModal` and `CatalogPickerModal`. Each ends with
//! a value or a cancel, never a hang.

use super::panels::framed;
use super::text::{sanitize, Line};
use crate::provider::catalog::CatalogEntry;
use crate::repl::Style;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Most rows of choices shown at once; the rest scroll.
const PICKER_ROWS: usize = 10;

/// A list longer than this can be narrowed by typing (numbers stop at nine).
const FILTER_OVER: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Dispatches `/mode <value>`.
    Mode,
    /// Dispatches `/model <value>`.
    Model,
    /// Loads the session with this id and rebuilds the transcript.
    Session,
    /// The provider to configure, by config section name.
    Provider,
    /// A `provider:model` ref from the catalog, or typed.
    Catalog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Open,
    Picked(PickerKind, String),
    Cancelled,
}

pub struct Picker {
    pub kind: PickerKind,
    title: String,
    /// (label, value) pairs.
    choices: Vec<(String, String)>,
    /// The type-to-filter text, for pickers that have one.
    filter: Option<String>,
    /// Indices into `choices` that pass the filter.
    shown: Vec<usize>,
    /// Index into `shown`.
    selected: usize,
    /// The catalog picker's provider: values are bare ids, picked as
    /// `provider:id`, and typed text is a model name when nothing else fits.
    provider: Option<String>,
    /// The highlight was moved since the filter last changed.
    navigated: bool,
}

impl Picker {
    fn new(
        kind: PickerKind,
        title: impl Into<String>,
        choices: Vec<(String, String)>,
        filter: bool,
    ) -> Self {
        let shown = (0..choices.len()).collect();
        Self {
            kind,
            title: title.into(),
            choices,
            filter: filter.then(String::new),
            shown,
            selected: 0,
            provider: None,
            navigated: false,
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

    /// The role slots, then the active provider's catalog. A catalog can run to
    /// hundreds of models, so a long list can be narrowed by typing.
    pub fn model(choices: Vec<(String, String)>) -> Self {
        let filter = choices.len() > FILTER_OVER;
        Self::new(PickerKind::Model, "Switch model", choices, filter)
    }

    /// Recent sessions as (label, session id), newest first, narrowed by typing.
    pub fn sessions(choices: Vec<(String, String)>) -> Self {
        Self::new(PickerKind::Session, "Resume a session", choices, true)
    }

    /// Every configured provider as (label, section name), with its key status.
    pub fn providers(choices: Vec<(String, String)>) -> Self {
        Self::new(
            PickerKind::Provider,
            "Providers — select one to configure",
            choices,
            false,
        )
    }

    /// `provider`'s models, narrowed by typing. With no catalog (none exists, or
    /// the fetch failed), what is typed is taken as the model name.
    pub fn catalog(provider: &str, entries: &[CatalogEntry]) -> Self {
        let choices = entries
            .iter()
            .map(|(id, label)| (label.clone(), id.clone()))
            .collect();
        let mut picker = Self::new(
            PickerKind::Catalog,
            format!("Models — {provider}"),
            choices,
            true,
        );
        picker.provider = Some(provider.to_owned());
        picker
    }

    pub fn on_key(&mut self, key: KeyEvent) -> PickerOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.shown.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => return PickerOutcome::Cancelled,
            KeyCode::Char('c') if ctrl => return PickerOutcome::Cancelled,
            KeyCode::Enter | KeyCode::Tab => return self.submit(),
            KeyCode::Up if !self.shown.is_empty() => {
                self.selected = self.selected.checked_sub(1).unwrap_or(last);
                self.navigated = true;
            }
            KeyCode::Down if !self.shown.is_empty() => {
                self.selected = (self.selected + 1) % self.shown.len();
                self.navigated = true;
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(PICKER_ROWS);
                self.navigated = true;
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + PICKER_ROWS).min(last);
                self.navigated = true;
            }
            KeyCode::Home if self.filter.is_none() => self.selected = 0,
            KeyCode::End if self.filter.is_none() => self.selected = last,
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

    /// Enter. The catalog picker follows Python's `on_input_submitted` for what
    /// was typed: the one model it matches, or else the typed text as the model.
    /// A highlight moved by hand is picked as it is.
    fn submit(&self) -> PickerOutcome {
        let typed = self.filter.as_deref().unwrap_or_default().trim();
        match &self.provider {
            Some(provider) if !typed.is_empty() && !self.navigated && self.shown.len() != 1 => {
                // Already naming this provider, or a bare model name for it.
                let reference = match typed.split_once(':') {
                    Some((named, _)) if named == provider => typed.to_owned(),
                    _ => format!("{provider}:{typed}"),
                };
                PickerOutcome::Picked(self.kind, reference)
            }
            _ => self.pick(self.selected),
        }
    }

    fn pick(&self, index: usize) -> PickerOutcome {
        match self.shown.get(index) {
            Some(&choice) => {
                let value = &self.choices[choice].1;
                let value = match &self.provider {
                    Some(provider) => format!("{provider}:{value}"),
                    None => value.clone(),
                };
                PickerOutcome::Picked(self.kind, value)
            }
            // Enter with nothing left to pick cancels, as Python's filter does.
            None if self.shown.is_empty() => PickerOutcome::Cancelled,
            None => PickerOutcome::Open,
        }
    }

    /// Python's `_filtered`: the needle in the label or the value, case-insensitive.
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
        self.navigated = false;
    }

    /// The panel, `width` cells wide.
    pub fn render(&self, width: usize) -> Vec<Line> {
        let mut rows = vec![Line::styled(Style::Bold, sanitize(&self.title))];
        if let Some(filter) = &self.filter {
            rows.push(if !filter.is_empty() {
                Line::styled(Style::Dim, "filter: ").push(Style::Plain, sanitize(filter))
            } else if self.choices.is_empty() {
                let provider = sanitize(self.provider.as_deref().unwrap_or_default());
                Line::styled(
                    Style::Dim,
                    format!("no catalog — type a model name for {provider} and press enter"),
                )
            } else {
                Line::styled(Style::Dim, "type to filter…")
            });
        }
        rows.push(Line::default());
        if self.shown.is_empty() && !self.choices.is_empty() {
            rows.push(Line::styled(
                Style::Dim,
                if self.provider.is_some() {
                    "no matches — enter uses what you typed"
                } else {
                    "no matches"
                },
            ));
        }
        let numbered = self.filter.is_none();
        let first = self.selected.saturating_sub(PICKER_ROWS - 1);
        let end = self.shown.len().min(first + PICKER_ROWS);
        for (offset, &choice) in self.shown[first..end].iter().enumerate() {
            let position = first + offset;
            // Labels come from config, servers, and the user's own prompts: drawn plain.
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
        framed(
            rows,
            width,
            "↑↓ to choose · enter to select · esc to cancel",
        )
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

    fn typed(picker: &mut Picker, text: &str) {
        for ch in text.chars() {
            picker.on_key(key(KeyCode::Char(ch)));
        }
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
        typed(&mut picker, "DOCS");
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
    fn long_lists_scroll_filter_and_rows_fit_the_width() {
        let choices = (0..30)
            .map(|n| (format!("model-{n} with a long label"), format!("p:m{n}")))
            .collect();
        let mut picker = Picker::model(choices);
        for _ in 0..15 {
            picker.on_key(key(KeyCode::Down));
        }
        let lines = text(&picker.render(24));
        assert!(lines.iter().any(|line| line.contains("❯ model-15")));
        assert!(lines.iter().any(|line| line.contains("… 14 more")));
        assert!(picker.render(24).iter().all(|line| line.width() <= 24));
        picker.on_key(key(KeyCode::PageDown));
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Model, "p:m25".into())
        );
        typed(&mut picker, "m7");
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Model, "p:m7".into())
        );
    }

    #[test]
    fn the_catalog_picker_takes_a_unique_match_or_the_typed_name() {
        let entries = vec![
            ("gpt-4o".to_owned(), "gpt-4o".to_owned()),
            ("gpt-4o-mini".to_owned(), "GPT-4o mini".to_owned()),
            ("o3".to_owned(), "o3".to_owned()),
        ];
        let mut picker = Picker::catalog("openai", &entries);
        assert!(text(&picker.render(50))
            .iter()
            .any(|line| line.contains("Models — openai")));
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Catalog, "openai:gpt-4o".into())
        );
        typed(&mut picker, "mini");
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Catalog, "openai:gpt-4o-mini".into()),
            "the one match"
        );
        let mut picker = Picker::catalog("openai", &entries);
        typed(&mut picker, "gpt-4o");
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Catalog, "openai:gpt-4o".into()),
            "two matches: the text itself"
        );
        picker.on_key(key(KeyCode::Down));
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Catalog, "openai:gpt-4o-mini".into()),
            "a highlight moved by hand"
        );

        let mut empty = Picker::catalog("llamacpp", &[]);
        assert!(text(&empty.render(80))
            .iter()
            .any(|line| line.contains("no catalog — type a model name for llamacpp")));
        assert_eq!(empty.on_key(key(KeyCode::Enter)), PickerOutcome::Cancelled);
        typed(&mut empty, "llamacpp:qwen3");
        assert_eq!(
            empty.on_key(key(KeyCode::Enter)),
            PickerOutcome::Picked(PickerKind::Catalog, "llamacpp:qwen3".into())
        );
    }
}
