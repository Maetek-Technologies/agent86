//! The interface's side of `crate::limits`, beyond Python: the offer to raise a
//! limit a running turn reached (a boxed panel in place of the input box, answered
//! like the approval panel), and `/limits save` through the save panel.

use super::app::{App, Panel};
use super::panels::{SaveDiff, SavePurpose};
use super::text::{sanitize, Line};
use crate::config::ConfigPaths;
use crate::config_writer::{apply_edit, ConfigEdit};
use crate::limits::{save_changes, LimitHit};
use crate::repl::{Repl, Style, Text};
use crossterm::event::{KeyCode, KeyEvent};
use std::sync::mpsc;

/// `/limits save`: the session's overrides as a config diff, each written to the
/// setting it replaces, in the save panel.
pub fn open_save(app: &mut App, repl: &Repl) {
    let overrides = repl.harness.session_limits.overrides();
    if overrides.is_empty() {
        app.note(
            Style::Dim,
            "no session limits to save; set one with /limits <name> <n>",
        );
        return;
    }
    let changes = save_changes(overrides, &repl.config, &repl.provider_section());
    let paths = repl
        .config_paths
        .clone()
        .unwrap_or_else(ConfigPaths::discover);
    app.open_panel(Panel::Save(SaveDiff::new(
        SavePurpose::Limits,
        changes,
        paths,
    )));
}

/// The save panel was answered: write the edit and take the limits from the
/// reloaded config. An override the reload disagrees with (another layer sets the
/// limit) stays in force, and is named.
pub fn on_saved(app: &mut App, repl: &mut Repl, edit: Option<ConfigEdit>) {
    let Some(edit) = edit else {
        app.note(
            Style::Dim,
            "not saved — the limits apply to this session only",
        );
        return;
    };
    let paths = repl
        .config_paths
        .clone()
        .unwrap_or_else(ConfigPaths::discover);
    match apply_edit(&edit, &paths) {
        Ok(config) => {
            repl.config = config;
            let section = repl.provider_section().to_owned();
            repl.harness.reload_limits(&repl.config, &section);
            app.note(Style::Dim, &format!("saved to {}", edit.path.display()));
            let overrides = repl.harness.session_limits.overrides();
            if !overrides.is_empty() {
                app.note(
                    Style::DimYellow,
                    "another config layer sets a different value; the session keeps its own (see /limits)",
                );
            }
        }
        Err(error) => {
            let mut text = Text::styled(Style::Red, "could not save:");
            text.push(Style::Plain, format!(" {}", sanitize(&error.0)));
            app.commit_text(&text);
        }
    }
}

/// The question and where the turn's thread waits for the answer: the new value,
/// or `None` to stop. Dropping it unanswered stops.
pub struct LimitPanel {
    pub hit: LimitHit,
    reply: mpsc::Sender<Option<u32>>,
    /// An offer's index, or `offers.len()` for "Stop".
    selected: usize,
}

impl LimitPanel {
    pub fn new(hit: LimitHit, reply: mpsc::Sender<Option<u32>>) -> Self {
        Self {
            hit,
            reply,
            selected: 0,
        }
    }

    /// `None` while open; then the answer to send with [`LimitPanel::answer`].
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Option<u32>> {
        let choices = self.hit.offers.len() + 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => Some(None),
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                (index < choices).then(|| self.hit.offers.get(index).copied())
            }
            KeyCode::Enter => Some(self.hit.offers.get(self.selected).copied()),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1) % choices;
                None
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = (self.selected + choices - 1) % choices;
                None
            }
            _ => None,
        }
    }

    /// Send the answer to the waiting turn.
    pub fn answer(self, value: Option<u32>) {
        let _ = self.reply.send(value);
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let inner = width.saturating_sub(4).max(1);
        let unit = self.hit.kind.unit();
        let mut rows = vec![
            Line::styled(Style::Bold, sanitize(&self.hit.headline())),
            Line::plain(self.hit.question()),
            Line::default(),
        ];
        let labels = self
            .hit
            .offers
            .iter()
            .map(|value| format!("Raise to {value} {unit}"))
            .chain(std::iter::once("Stop".to_owned()));
        for (index, label) in labels.enumerate() {
            let label = format!("{}. {label}", index + 1);
            rows.push(if index == self.selected {
                Line::styled(Style::BoldCyan, format!("❯ {label}"))
            } else {
                Line::plain(format!("  {label}"))
            });
        }

        let border = Style::Yellow;
        let mut lines = vec![Line::styled(border, format!("╭{}╮", "─".repeat(width - 2)))];
        for row in rows.iter().flat_map(|row| row.hard_wrap(inner)) {
            let pad = inner.saturating_sub(row.width());
            let mut line = Line::styled(border, "│ ");
            line.spans.extend(row.spans);
            lines.push(line.push(Style::Plain, " ".repeat(pad)).push(border, " │"));
        }
        lines.push(Line::styled(border, format!("╰{}╯", "─".repeat(width - 2))));
        lines.push(
            Line::styled(
                Style::Dim,
                "  a number or ↑↓ and enter to choose · n or esc to stop",
            )
            .truncate(width),
        );
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::LimitKind;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn panel() -> (LimitPanel, mpsc::Receiver<Option<u32>>) {
        let (reply, answer) = mpsc::channel();
        let hit = LimitHit {
            kind: LimitKind::Steps,
            agent: None,
            current: 40,
            offers: vec![80, 160],
        };
        (LimitPanel::new(hit, reply), answer)
    }

    #[test]
    fn keys_choose_an_offer_or_stop() {
        let (mut limit, _answer) = panel();
        assert_eq!(limit.on_key(key(KeyCode::Char('2'))), Some(Some(160)));
        assert_eq!(limit.on_key(key(KeyCode::Char('3'))), Some(None));
        assert_eq!(limit.on_key(key(KeyCode::Char('4'))), None);
        assert_eq!(limit.on_key(key(KeyCode::Esc)), Some(None));
        assert_eq!(limit.on_key(key(KeyCode::Enter)), Some(Some(80)));
        assert_eq!(limit.on_key(key(KeyCode::Up)), None);
        assert_eq!(
            limit.on_key(key(KeyCode::Enter)),
            Some(None),
            "Up wraps to Stop"
        );
    }

    #[test]
    fn the_answer_reaches_the_turn() {
        let (limit, answer) = panel();
        limit.answer(Some(80));
        assert_eq!(answer.recv(), Ok(Some(80)));
    }

    #[test]
    fn saving_writes_each_override_where_it_takes_effect_and_settles_it() {
        use crate::config::{load_config, Overrides, ProviderConfig};
        use crate::config_writer::{plan_edit, Scope};
        use crate::secrets::NoSecretStore;
        use crate::ui::status::StatusState;

        let root = std::env::temp_dir().join(format!("agent86-ui-limits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = ConfigPaths {
            user: Some(root.join("user.toml")),
            project: root.join("project.toml"),
            project_trust: crate::config::ProjectTrust::Trusted,
        };
        std::fs::write(
            &paths.project,
            "# project settings\n[limits]\nmax_steps = 40\n",
        )
        .unwrap();
        let mut config = load_config(&paths, &Overrides::default()).unwrap();
        config.memory.enabled = false;
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                base_url: Some("http://127.0.0.1:9/v1".into()),
                ..Default::default()
            },
        );
        config.model.default = "local:m".into();
        let mut repl = Repl::new(config, None, None, Box::new(NoSecretStore)).unwrap();
        repl.config_paths = Some(paths.clone());
        let mut app = App::new(StatusState::default(), "s1".into(), false);
        app.set_size(100, 40);
        let printed = |app: &mut App| -> Vec<String> {
            app.take_committed().iter().map(Line::plain_text).collect()
        };

        open_save(&mut app, &repl);
        assert!(app.panel().is_none());
        assert_eq!(
            printed(&mut app),
            ["no session limits to save; set one with /limits <name> <n>"]
        );

        let limits = &repl.harness.session_limits;
        limits.set(crate::limits::LimitKind::Steps, Some(60));
        limits.set(crate::limits::LimitKind::Output, Some(16000));
        open_save(&mut app, &repl);
        assert!(matches!(app.panel(), Some(Panel::Save(_))));
        on_saved(&mut app, &mut repl, None);
        assert_eq!(
            printed(&mut app),
            ["not saved — the limits apply to this session only"]
        );
        assert_eq!(repl.harness.max_steps(), 60, "still the session's");

        let changes = save_changes(
            repl.harness.session_limits.overrides(),
            &repl.config,
            "local",
        );
        let edit = plan_edit(Scope::Project, &paths, &changes).unwrap();
        on_saved(&mut app, &mut repl, Some(edit));
        let written = std::fs::read_to_string(&paths.project).unwrap();
        assert_eq!(
            written,
            "# project settings\n[limits]\nmax_steps = 60\nmax_output_tokens = 16000\n"
        );
        // The config now says what the session did, so the overrides are gone.
        assert!(repl.harness.session_limits.overrides().is_empty());
        assert_eq!(repl.harness.max_steps(), 60);
        assert_eq!(repl.harness.output_cap(), 16000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_panel_names_the_limit_and_its_choices_within_the_width() {
        let (limit, _answer) = panel();
        let lines = limit.render(40);
        let text: Vec<String> = lines.iter().map(Line::plain_text).collect();
        assert!(text
            .iter()
            .any(|row| row.contains("Step budget reached (40 steps)")));
        assert!(text
            .iter()
            .any(|row| row.contains("❯ 1. Raise to 80 steps")));
        assert!(text.iter().any(|row| row.contains("  3. Stop")));
        assert!(lines.iter().all(|line| line.width() <= 40), "{text:#?}");
    }
}
