//! The `/config mcp` chain, as the MCP-manager part of Python's `tui/app.py`.
//!
//! Adding or editing runs: the server list, the form, a masked prompt for each
//! `${VAR}` nothing else supplies, a live test that starts the server, the save
//! panel, then mounting its tools. Removing or disabling runs through the save
//! panel, then unmounts at once, so a deleted server's tools are never left
//! callable. Re-enabling is an add: it is tested before it is mounted.
//!
//! A typed `${VAR}` value is kept in memory and stored in the keyring (under the
//! variable's name) only after the test passes or the user saves anyway; the
//! config keeps the reference. A server a test started is stopped if the add is
//! cancelled, so nothing is left running unseen.

use super::app::{App, Panel, UiEvent};
use super::mcp_config::{mcp_changes, mcp_rows, McpDraft};
use super::mcp_panels::{McpAction, McpForm, McpList, McpTest, McpTestOutcome};
use super::panels::{KeyEntry, KeyPurpose, SaveDiff, SavePurpose};
use super::text::sanitize;
use crate::config::ConfigPaths;
use crate::config_writer::{apply_edit, Change, ConfigEdit};
use crate::mcp::{unresolved_var_refs, var_ref_names};
use crate::repl::{Repl, Style, Text};
use crate::secrets::{redact, resolve_api_key, NoSecretStore, SERVICE_NAME};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};

/// The MCP test's hard cap: a first-run stdio server may download its package
/// (`npx`) before it can answer.
pub const MCP_TEST_TIMEOUT_S: f64 = 30.0;

/// What a confirmed unmount does to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unmount {
    Remove,
    Disable,
}

/// The state of the `/config mcp` chain in flight.
pub struct McpFlow {
    events: mpsc::Sender<UiEvent>,
    draft: Option<McpDraft>,
    /// `${VAR}` values typed this pass, by name; stored only once the test passes.
    typed: HashMap<String, String>,
    /// The server a passing test left running, to mount after the save.
    started: Option<String>,
    unmount: Option<(String, Unmount)>,
    tests: u64,
}

fn line(app: &mut App, style: Style, lead: &str, rest: &str) {
    let mut text = Text::styled(style, lead);
    text.push(Style::Plain, format!(" {}", sanitize(&redact(rest, &[]))));
    app.commit_text(&text);
}

fn paths(repl: &Repl) -> ConfigPaths {
    repl.config_paths
        .clone()
        .unwrap_or_else(ConfigPaths::discover)
}

impl McpFlow {
    pub fn new(events: mpsc::Sender<UiEvent>) -> Self {
        Self {
            events,
            draft: None,
            typed: HashMap::new(),
            started: None,
            unmount: None,
            tests: 0,
        }
    }

    /// The server list: config facts only, so opening it never starts anything.
    pub fn open(&mut self, app: &mut App, repl: &Repl) {
        self.draft = None;
        self.typed.clear();
        self.started = None;
        self.unmount = None;
        app.open_panel(Panel::McpList(McpList::new(mcp_rows(&repl.config))));
    }

    pub fn on_action(&mut self, app: &mut App, repl: &mut Repl, action: McpAction) {
        let names: Vec<String> = repl.config.mcp_servers.keys().cloned().collect();
        let (name, kind) = match action {
            McpAction::AddJson => {
                return app.open_panel(Panel::McpForm(Box::new(McpForm::add(true, names))))
            }
            McpAction::AddManual => {
                return app.open_panel(Panel::McpForm(Box::new(McpForm::add(false, names))))
            }
            McpAction::Edit(name) => (name, None),
            McpAction::Remove(name) => (name, Some(Unmount::Remove)),
            McpAction::Toggle(name) => (name, Some(Unmount::Disable)),
        };
        let Some(server) = repl.config.mcp_servers.get(&name).cloned() else {
            return;
        };
        match kind {
            // Editing always uses the field form.
            None => {
                let draft = McpDraft {
                    name: name.clone(),
                    config: server,
                    original_name: Some(name),
                };
                app.open_panel(Panel::McpForm(Box::new(McpForm::edit(&draft, names))));
            }
            // Re-enabling is an add: test it before mounting it.
            Some(Unmount::Disable) if !server.enabled => {
                let mut config = server;
                config.enabled = true;
                self.draft = Some(McpDraft {
                    name,
                    config,
                    original_name: None,
                });
                self.prompt_next_var(app, repl);
            }
            // No extra confirmation: the save panel shows exactly what changes.
            Some(unmount) => {
                let base = ["mcp", "servers", name.as_str()].map(str::to_owned);
                let change: Change = match unmount {
                    Unmount::Remove => (base.to_vec(), None),
                    Unmount::Disable => (
                        [base.as_slice(), &["enabled".to_owned()]].concat(),
                        Some(toml_edit::Value::from(false)),
                    ),
                };
                self.unmount = Some((name, unmount));
                app.open_panel(Panel::Save(SaveDiff::new(
                    SavePurpose::McpUnmount,
                    vec![change],
                    paths(repl),
                )));
            }
        }
    }

    pub fn on_form(&mut self, app: &mut App, repl: &mut Repl, draft: McpDraft) {
        self.draft = Some(draft);
        self.prompt_next_var(app, repl);
    }

    /// Ask for each `${VAR}` nothing supplies, through the one masked entry, then test.
    fn prompt_next_var(&mut self, app: &mut App, repl: &mut Repl) {
        let Some(draft) = &self.draft else {
            return;
        };
        match unresolved_var_refs(&draft.config, &self.typed, &*repl.secrets).first() {
            Some(name) => app.open_panel(Panel::Key(KeyEntry::new(
                KeyPurpose::McpVar,
                name,
                repl.secrets.available(),
            ))),
            None => self.start_test(app, repl),
        }
    }

    pub fn on_var(&mut self, app: &mut App, repl: &mut Repl, value: Option<String>) {
        let (Some(value), Some(draft)) = (value, &self.draft) else {
            // Connecting with an empty credential would only fail misleadingly.
            app.note(Style::Dim, "mcp: cancelled");
            self.draft = None;
            return;
        };
        if let Some(name) = unresolved_var_refs(&draft.config, &self.typed, &*repl.secrets)
            .into_iter()
            .next()
        {
            self.typed.insert(name, value);
        }
        self.prompt_next_var(app, repl);
    }

    /// Start the server on a worker. Every `${VAR}` is looked up here, where the
    /// keyring is, so the worker needs nothing but the values.
    fn start_test(&mut self, app: &mut App, repl: &mut Repl) {
        let Some(draft) = self.draft.clone() else {
            return;
        };
        let manager = repl.ensure_mcp();
        let mut values = self.typed.clone();
        let config = &draft.config;
        let refs = config
            .args
            .iter()
            .chain(config.env.values())
            .chain(config.url.iter())
            .chain(config.headers.values());
        for text in refs {
            for name in var_ref_names(text) {
                if let std::collections::hash_map::Entry::Vacant(slot) = values.entry(name) {
                    if let Some(value) =
                        resolve_api_key(slot.key(), Some(slot.key()), &*repl.secrets)
                    {
                        slot.insert(value);
                    }
                }
            }
        }
        self.tests += 1;
        let test = McpTest::new(self.tests, &draft.name, config.transport());
        let (id, abandoned) = (test.id, Arc::clone(&test.abandoned));
        app.open_panel(Panel::McpTest(test));
        let events = self.events.clone();
        std::thread::spawn(move || {
            let result = manager.start_server(
                &draft.name,
                &draft.config,
                &values,
                &NoSecretStore,
                MCP_TEST_TIMEOUT_S,
            );
            // Closed before it connected: nothing may stay running unseen.
            if abandoned.load(Ordering::SeqCst) && result.is_ok() {
                manager.remove_server(&draft.name);
            }
            let _ = events.send(UiEvent::McpTested { id, result });
        });
    }

    pub fn on_tested(&mut self, app: &mut App, repl: &mut Repl, outcome: McpTestOutcome) {
        let Some(draft) = self.draft.clone() else {
            return;
        };
        let error = outcome.error.as_deref().unwrap_or("None");
        if outcome.ok {
            // The test left it running; it is mounted after the save.
            self.started = Some(draft.name.clone());
            app.note(
                Style::Dim,
                &format!(
                    "mcp: {} connected, {} tools",
                    draft.name,
                    outcome.tools.len()
                ),
            );
        } else if outcome.override_ {
            line(app, Style::Yellow, "saving anyway despite:", error);
        } else {
            line(app, Style::Red, "mcp connection test failed:", error);
            // A test closed after it connected leaves nothing running either.
            if let Some(mcp) = &repl.harness.mcp {
                mcp.remove_server(&draft.name);
            }
            self.draft = None;
            return;
        }
        // A typed secret becomes persistent only now, under the variable's name.
        for (name, value) in std::mem::take(&mut self.typed) {
            match repl.secrets.set_password(SERVICE_NAME, &name, &value) {
                Ok(()) => app.note(Style::Dim, &format!("stored ${{{name}}} in the OS keyring")),
                Err(error) => line(
                    app,
                    Style::Red,
                    &format!("could not store ${{{name}}}:"),
                    &format!("Could not store the key for '{name}': {error}"),
                ),
            }
        }
        app.open_panel(Panel::Save(SaveDiff::new(
            SavePurpose::McpServer,
            mcp_changes(&draft),
            paths(repl),
        )));
    }

    pub fn on_saved(&mut self, app: &mut App, repl: &mut Repl, edit: Option<ConfigEdit>) {
        let (draft, started) = (self.draft.take(), self.started.take());
        let stop = |repl: &Repl, name: &Option<String>| {
            if let (Some(mcp), Some(name)) = (&repl.harness.mcp, name) {
                mcp.remove_server(name);
            }
        };
        let Some(edit) = edit else {
            app.note(Style::Dim, "not saved");
            // A cancelled add must not leave the server it started running.
            stop(repl, &started);
            return;
        };
        match apply_edit(&edit, &paths(repl)) {
            Err(error) => {
                line(app, Style::Red, "could not save:", &error.0);
                stop(repl, &started);
                return;
            }
            Ok(config) => {
                repl.config = config;
                app.note(Style::Dim, &format!("saved to {}", edit.path.display()));
            }
        }
        let Some(draft) = draft else {
            return;
        };
        // A renamed server's old self goes; an edit in place was replaced by its test.
        if let Some(original) = draft
            .original_name
            .as_ref()
            .filter(|old| **old != draft.name)
        {
            stop(repl, &Some(original.clone()));
        }
        if started.as_deref() != Some(draft.name.as_str()) {
            // The config was written; the live mount was not. Both are said.
            line(
                app,
                Style::Yellow,
                &format!("saved, but {} is not running:", draft.name),
                "it will be available next launch",
            );
            return;
        }
        let (mounted, collisions) = repl.ensure_mcp().mount(&draft.name);
        let mut text = Text::styled(
            Style::Dim,
            format!(
                "mcp: {} mounted, {} tools live",
                sanitize(&draft.name),
                mounted.len()
            ),
        );
        if !collisions.is_empty() {
            text.push(
                Style::Yellow,
                format!(
                    " (name collisions, not mounted: {})",
                    sanitize(&collisions.join(", "))
                ),
            );
        }
        app.commit_text(&text);
    }

    pub fn on_unmount_saved(&mut self, app: &mut App, repl: &mut Repl, edit: Option<ConfigEdit>) {
        let (Some(edit), Some((name, unmount))) = (edit, self.unmount.take()) else {
            app.note(Style::Dim, "not saved");
            return;
        };
        match apply_edit(&edit, &paths(repl)) {
            Err(error) => line(app, Style::Red, "could not save:", &error.0),
            Ok(config) => {
                repl.config = config;
                app.note(Style::Dim, &format!("saved to {}", edit.path.display()));
                // Unmounted at once: the model must not call what was just removed.
                if let Some(mcp) = &repl.harness.mcp {
                    mcp.remove_server(&name);
                }
                let verb = match unmount {
                    Unmount::Remove => "removed",
                    Unmount::Disable => "disabled",
                };
                app.note(
                    Style::Dim,
                    &format!("mcp: {name} {verb}; its tools are no longer available"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, Overrides};
    use crate::config_writer::{plan_edit, Scope};
    use crate::ui::app::Request;
    use crate::ui::status::StatusState;
    use std::path::PathBuf;
    use std::time::Duration;

    fn python() -> Option<&'static str> {
        ["python3", "python"].into_iter().find(|candidate| {
            std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
    }

    fn printed(app: &mut App) -> Vec<String> {
        app.take_committed()
            .iter()
            .map(|line| line.plain_text())
            .collect()
    }

    #[test]
    fn a_server_is_added_tested_saved_mounted_and_removed() {
        let Some(python) = python() else {
            return;
        };
        let root = std::env::temp_dir().join(format!("agent86-mcpflow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = ConfigPaths {
            user: Some(root.join("user.toml")),
            project: root.join("project.toml"),
            project_trust: crate::config::ProjectTrust::Trusted,
        };
        let mut config = load_config(&paths, &Overrides::default()).unwrap();
        config.memory.enabled = false;
        config.providers.insert(
            "local".into(),
            crate::config::ProviderConfig {
                base_url: Some("http://127.0.0.1:9/v1".into()),
                ..Default::default()
            },
        );
        config.model.default = "local:m".into();
        let mut repl = Repl::new(config, None, None, Box::new(NoSecretStore)).unwrap();
        repl.config_paths = Some(paths.clone());
        let builtins = repl.harness.registry.names().len();
        let (events, inbox) = mpsc::channel();
        let mut flow = McpFlow::new(events);
        let mut app = App::new(StatusState::default(), "s1".into(), false);
        app.set_size(100, 40);

        flow.open(&mut app, &repl);
        assert!(matches!(app.panel(), Some(Panel::McpList(_))));
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/server.py");
        let json = serde_json::json!({
            "command": python,
            "args": [script.display().to_string(), "normal"],
            "env": {"FIXTURE_TOKEN": "${AGENT86_TEST_FLOW_VAR}"},
        });
        let (_, server) =
            crate::ui::mcp_config::parse_server_json(&json.to_string(), "fx").unwrap();
        let draft = McpDraft {
            name: "fx".into(),
            config: server,
            original_name: None,
        };
        flow.on_form(&mut app, &mut repl, draft.clone());
        assert!(
            matches!(app.panel(), Some(Panel::Key(_))),
            "the ${{VAR}} is asked for"
        );
        flow.on_var(&mut app, &mut repl, Some("typed-value".into()));
        assert!(matches!(app.panel(), Some(Panel::McpTest(_))));

        let event = inbox.recv_timeout(Duration::from_secs(60)).unwrap();
        app.on_turn_event(event);
        app.on_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
            &[],
        );
        let Some(Request::McpTested(outcome)) = app.take_request() else {
            panic!("the test should pass");
        };
        assert!(outcome.ok);
        flow.on_tested(&mut app, &mut repl, outcome);
        assert_eq!(
            printed(&mut app),
            [
                "mcp: fx connected, 20 tools",
                "could not store ${AGENT86_TEST_FLOW_VAR}: Could not store the key for \
                 'AGENT86_TEST_FLOW_VAR': this secret store is read-only",
            ]
        );
        assert!(matches!(app.panel(), Some(Panel::Save(_))));
        assert_eq!(
            repl.harness.registry.names().len(),
            builtins,
            "not mounted yet"
        );

        let edit = plan_edit(Scope::Project, &paths, &mcp_changes(&draft)).unwrap();
        flow.on_saved(&mut app, &mut repl, Some(edit));
        let lines = printed(&mut app);
        assert_eq!(lines[1], "mcp: fx mounted, 20 tools live");
        assert_eq!(repl.harness.registry.names().len(), builtins + 20);
        let saved = std::fs::read_to_string(&paths.project).unwrap();
        assert!(
            saved.contains("FIXTURE_TOKEN = \"${AGENT86_TEST_FLOW_VAR}\""),
            "{saved}"
        );
        assert!(
            !saved.contains("typed-value"),
            "the typed value never reaches the file"
        );

        flow.on_action(&mut app, &mut repl, McpAction::Remove("fx".into()));
        assert!(matches!(app.panel(), Some(Panel::Save(_))));
        let removal = vec![(["mcp", "servers", "fx"].map(str::to_owned).to_vec(), None)];
        let edit = plan_edit(Scope::Project, &paths, &removal).unwrap();
        flow.on_unmount_saved(&mut app, &mut repl, Some(edit));
        assert_eq!(
            printed(&mut app)[1],
            "mcp: fx removed; its tools are no longer available"
        );
        assert_eq!(repl.harness.registry.names().len(), builtins);
        assert!(!repl.config.mcp_servers.contains_key("fx"));

        // Cancelling a save after a passing test stops the server it started.
        flow.draft = Some(draft);
        flow.started = Some("fx".into());
        repl.ensure_mcp()
            .start_server(
                "fx",
                &flow.draft.as_ref().unwrap().config,
                &HashMap::from([("AGENT86_TEST_FLOW_VAR".to_owned(), "x".to_owned())]),
                &NoSecretStore,
                30.0,
            )
            .unwrap();
        flow.on_saved(&mut app, &mut repl, None);
        assert_eq!(printed(&mut app), ["not saved"]);
        assert!(repl.harness.mcp.as_ref().unwrap().tools().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
