//! `/thinking` and `/toolview`, beyond Python: how much of the model's reasoning
//! and of each tool call the interface shows (`[ui] thinking` and `[ui] tool_use`),
//! set for the session, and kept with `save` through the save panel, as `/limits
//! save` does.

use super::app::{App, Panel};
use super::panels::{SaveDiff, SavePurpose};
use super::text::sanitize;
use crate::config::{ConfigPaths, DisplayLevel};
use crate::config_writer::{apply_edit, Change, ConfigEdit};
use crate::repl::{Repl, Style, Text};

/// Which of the two settings a command is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Thinking,
    ToolUse,
}

impl Setting {
    /// The setting a command name stands for.
    pub fn of_command(name: &str) -> Option<Self> {
        match name {
            "/thinking" => Some(Self::Thinking),
            "/toolview" => Some(Self::ToolUse),
            _ => None,
        }
    }

    fn command(self) -> &'static str {
        match self {
            Self::Thinking => "/thinking",
            Self::ToolUse => "/toolview",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::ToolUse => "tool_use",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::ToolUse => "tool use",
        }
    }

    fn level(self, app: &App) -> DisplayLevel {
        match self {
            Self::Thinking => app.thinking_level(),
            Self::ToolUse => app.tool_level(),
        }
    }

    fn configured(self, repl: &Repl) -> DisplayLevel {
        match self {
            Self::Thinking => repl.config.ui.thinking,
            Self::ToolUse => repl.config.ui.tool_use,
        }
    }
}

fn choices() -> String {
    DisplayLevel::ALL.map(DisplayLevel::as_str).join("|")
}

/// `/thinking` or `/toolview` with `arg`: show the level, set it, or save.
pub fn command(app: &mut App, repl: &Repl, setting: Setting, arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() {
        let hint = match setting {
            Setting::Thinking => " · Ctrl+T cycles",
            Setting::ToolUse => "",
        };
        let shown = setting.level(app).as_str();
        app.note(
            Style::Dim,
            &format!(
                "{}: {shown} ({}){hint} · {} save keeps the choice",
                setting.label(),
                choices(),
                setting.command()
            ),
        );
    } else if arg == "save" {
        open_save(app, repl);
    } else if let Some(level) = DisplayLevel::parse(arg) {
        match setting {
            Setting::Thinking => app.set_thinking_level(level),
            Setting::ToolUse => app.set_tool_level(level),
        }
        app.note(
            Style::Dim,
            &format!("{}: {}", setting.label(), level.as_str()),
        );
    } else {
        app.note(
            Style::Dim,
            &format!(
                "'{}' is not a level; use {} {} or {} save",
                sanitize(arg),
                setting.command(),
                choices(),
                setting.command()
            ),
        );
    }
}

/// The config changes that keep the session's levels: only the ones that differ
/// from what the config says.
pub fn save_changes(app: &App, repl: &Repl) -> Vec<Change> {
    [Setting::Thinking, Setting::ToolUse]
        .into_iter()
        .filter(|setting| setting.level(app) != setting.configured(repl))
        .map(|setting| {
            (
                vec!["ui".to_owned(), setting.key().to_owned()],
                Some(toml_edit::Value::from(setting.level(app).as_str())),
            )
        })
        .collect()
}

/// `save`: the session's levels as a config diff, in the save panel.
pub fn open_save(app: &mut App, repl: &Repl) {
    let changes = save_changes(app, repl);
    if changes.is_empty() {
        app.note(
            Style::Dim,
            "nothing to save; thinking and tool use already match the config",
        );
        return;
    }
    let paths = repl
        .config_paths
        .clone()
        .unwrap_or_else(ConfigPaths::discover);
    app.open_panel(Panel::Save(SaveDiff::new(
        SavePurpose::Display,
        changes,
        paths,
    )));
}

/// The save panel was answered: write the edit and take the config it reloads. A
/// level another config layer overrides is named; the session keeps its own.
pub fn on_saved(app: &mut App, repl: &mut Repl, edit: Option<ConfigEdit>) {
    let Some(edit) = edit else {
        app.note(
            Style::Dim,
            "not saved; the levels apply to this session only",
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
            app.note(Style::Dim, &format!("saved to {}", edit.path.display()));
            if !save_changes(app, repl).is_empty() {
                app.note(
                    Style::DimYellow,
                    "another config layer sets a different value; the session keeps its own",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, Overrides, ProviderConfig};
    use crate::config_writer::{plan_edit, Scope};
    use crate::secrets::NoSecretStore;
    use crate::ui::status::StatusState;
    use crate::ui::text::Line;

    #[test]
    fn the_commands_show_set_and_save_the_levels() {
        let root = std::env::temp_dir().join(format!("agent86-ui-display-{}", std::process::id()));
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

        // A bare command reports the level and how to change it.
        command(&mut app, &repl, Setting::Thinking, "");
        command(&mut app, &repl, Setting::ToolUse, "  ");
        assert_eq!(
            printed(&mut app),
            [
                "thinking: summary (full|summary|hidden) · Ctrl+T cycles · /thinking save keeps the choice",
                "tool use: summary (full|summary|hidden) · /toolview save keeps the choice",
            ]
        );
        // Nothing differs from the config yet, so there is nothing to save.
        command(&mut app, &repl, Setting::Thinking, "save");
        assert!(app.panel().is_none());
        assert_eq!(
            printed(&mut app),
            ["nothing to save; thinking and tool use already match the config"]
        );

        // A level is taken by name, in any case; anything else lists the choices.
        command(&mut app, &repl, Setting::Thinking, "FULL");
        command(&mut app, &repl, Setting::ToolUse, "hidden");
        command(&mut app, &repl, Setting::ToolUse, "loud");
        assert_eq!(app.thinking_level(), DisplayLevel::Full);
        assert_eq!(app.tool_level(), DisplayLevel::Hidden);
        assert_eq!(
            printed(&mut app),
            [
                "thinking: full",
                "tool use: hidden",
                "'loud' is not a level; use /toolview full|summary|hidden or /toolview save",
            ]
        );

        // Saving writes both changed keys and nothing else, after the diff panel.
        command(&mut app, &repl, Setting::ToolUse, "save");
        assert!(matches!(app.panel(), Some(Panel::Save(_))));
        on_saved(&mut app, &mut repl, None);
        assert_eq!(
            printed(&mut app),
            ["not saved; the levels apply to this session only"]
        );
        let changes = save_changes(&app, &repl);
        assert_eq!(changes.len(), 2);
        let edit = plan_edit(Scope::Project, &paths, &changes).unwrap();
        on_saved(&mut app, &mut repl, Some(edit));
        let written = std::fs::read_to_string(&paths.project).unwrap();
        assert!(
            written.starts_with("# project settings\n[limits]\nmax_steps = 40\n"),
            "{written}"
        );
        assert!(
            written.contains("[ui]\nthinking = \"full\"\ntool_use = \"hidden\"\n"),
            "{written}"
        );
        // The reloaded config says what the session does, with no warning.
        assert_eq!(repl.config.ui.thinking, DisplayLevel::Full);
        assert_eq!(repl.config.ui.tool_use, DisplayLevel::Hidden);
        let after = printed(&mut app);
        assert_eq!(after.len(), 1, "{after:?}");
        assert!(after[0].starts_with("saved to "), "{after:?}");
        command(&mut app, &repl, Setting::Thinking, "save");
        assert_eq!(
            printed(&mut app),
            ["nothing to save; thinking and tool use already match the config"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_two_commands_are_settings() {
        assert_eq!(Setting::of_command("/thinking"), Some(Setting::Thinking));
        assert_eq!(Setting::of_command("/toolview"), Some(Setting::ToolUse));
        assert_eq!(Setting::of_command("/tools"), None);
        assert_eq!(Setting::of_command("/limits"), None);
    }
}
