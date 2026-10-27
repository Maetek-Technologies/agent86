//! Model catalogs and the `/config model` chain, as the catalog and model-manager
//! parts of Python's `tui/app.py`.
//!
//! Catalogs are fetched on a worker and cached for the session, once per provider
//! (no disk cache, no expiry). They feed the model picker, the catalog picker, and
//! the typed `/model <id>` fallback.
//!
//! `/config model` runs: the provider list, key entry when the provider has no key,
//! the catalog picker, a live connection test, then the save panel. A typed key is
//! held in memory only and reaches the keyring only after the test passes (or the
//! user saves anyway); the config file only ever names the environment variable.

use super::app::{App, CatalogPurpose, Panel, UiEvent};
use super::manage_mcp::McpFlow;
use super::panels::{
    ConnectionTest, KeyEntry, KeyPurpose, SaveDiff, SavePurpose, TestOutcome, TestResult,
};
use super::picker::Picker;
use super::text::sanitize;
use crate::config::ConfigPaths;
use crate::config_writer::{apply_edit, Change, ConfigEdit};
use crate::provider::catalog::{catalog_has_ref, fetch_catalog, prefix_catalog_refs, CatalogEntry};
use crate::provider::{provider_for_ref_with_key, provider_for_ref_with_store, Provider};
use crate::repl::{model_choices, Repl, Style, Text};
use crate::secrets::{resolve_api_key, SERVICE_NAME};
use crate::types::{CompletionRequest, Message, ModelRef, Role};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::Duration;

/// The connection test's hard cap.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Provider names that build without a `[providers.X]` block. Used only to judge
/// whether a failed `/model` looks like a bare model id; see [`is_bare_ref_candidate`].
const BUILTIN_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "openai-compatible",
    "ollama",
    "llamacpp",
];

/// One row of the provider list. `keyless` means the endpoint needs no key at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub name: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub has_key: bool,
    pub keyless: bool,
}

impl ProviderRow {
    pub fn status(&self) -> &'static str {
        if self.keyless {
            "local, no key needed"
        } else if self.has_key {
            "key ok"
        } else {
            "no key"
        }
    }

    fn label(&self) -> String {
        format!(
            "{}  {}  ({})",
            self.name,
            self.base_url.as_deref().unwrap_or("-"),
            self.status()
        )
    }
}

/// Every configured provider, in config order, with its key status as a turn
/// would resolve it (environment, then keyring). The key itself is never read out.
pub fn provider_rows(repl: &Repl) -> Vec<ProviderRow> {
    repl.config
        .providers
        .iter()
        .map(|(name, section)| {
            let keyless = section.api_key_env.is_none();
            ProviderRow {
                name: name.clone(),
                base_url: section.base_url.clone(),
                api_key_env: section.api_key_env.clone(),
                has_key: !keyless && provider_key(repl, name).is_some(),
                keyless,
            }
        })
        .collect()
}

fn provider_key(repl: &Repl, provider: &str) -> Option<String> {
    let env = repl
        .config
        .providers
        .get(provider)
        .and_then(|section| section.api_key_env.clone());
    resolve_api_key(provider, env.as_deref(), &*repl.secrets)
}

/// The active provider's config section: the catalog and the key are keyed on it,
/// not on the adapter, which calls itself `openai` for every compatible gateway.
fn active_provider(repl: &Repl) -> String {
    repl.model_ref()
        .split_once(':')
        .map_or_else(String::new, |(provider, _)| provider.to_owned())
}

/// Is a failed `/model <arg>` plausibly a bare model id that the first-colon split
/// broke? Only then may the catalog be asked; a ref naming a real provider that
/// failed to build (no key, bad URL) gets its own error at once, with no fetch.
fn is_bare_ref_candidate(arg: &str, active: &str, repl: &Repl) -> bool {
    match ModelRef::parse(arg) {
        Err(_) => true,
        Ok(parsed) if parsed.provider == active => false,
        Ok(parsed) => {
            !repl.config.providers.contains_key(&parsed.provider)
                && !BUILTIN_PROVIDERS.contains(&parsed.provider.as_str())
        }
    }
}

fn line(app: &mut App, text: Text) {
    app.commit_text(&text);
}

/// The catalog cache and the state of the `/config model` chain in flight.
pub struct Manager {
    events: mpsc::Sender<UiEvent>,
    /// The `/config mcp` chain.
    pub mcp: McpFlow,
    catalogs: HashMap<String, Vec<CatalogEntry>>,
    /// Typed `/model` lines waiting for a catalog: (arg, the strict error, the
    /// provider that was active when it was typed).
    pending_model: Vec<(String, Option<Text>, String)>,
    /// Providers with a fallback fetch outstanding: one per provider per burst.
    inflight: HashSet<String>,
    row: Option<ProviderRow>,
    /// The key typed this pass, in memory only until the test passes.
    key: Option<String>,
    reference: Option<String>,
    tests: u64,
}

impl Manager {
    pub fn new(events: mpsc::Sender<UiEvent>) -> Self {
        Self {
            mcp: McpFlow::new(events.clone()),
            events,
            catalogs: HashMap::new(),
            pending_model: Vec::new(),
            inflight: HashSet::new(),
            row: None,
            key: None,
            reference: None,
            tests: 0,
        }
    }

    // ---- catalogs ------------------------------------------------------------

    /// Serve the catalog from the cache, or fetch it on a worker; the answer comes
    /// back through [`Manager::on_catalog`].
    fn request_catalog(
        &mut self,
        app: &mut App,
        repl: &Repl,
        provider: &str,
        key: Option<String>,
        purpose: CatalogPurpose,
    ) {
        if let Some(entries) = self.catalogs.get(provider).cloned() {
            self.on_catalog(app, repl, provider, Ok(entries), purpose);
            return;
        }
        app.note(Style::Dim, &format!("fetching {provider} model catalog…"));
        let section = repl
            .config
            .providers
            .get(provider)
            .cloned()
            .unwrap_or_default();
        let events = self.events.clone();
        let provider = provider.to_owned();
        std::thread::spawn(move || {
            let result =
                fetch_catalog(&provider, &section, key.as_deref()).map_err(|error| error.0);
            let _ = events.send(UiEvent::Catalog {
                provider,
                result,
                purpose,
            });
        });
    }

    /// A catalog arrived (or failed): cache it, then do what it was fetched for.
    /// Returns the refs that queued `/model` lines resolved to, for the driver to
    /// retry with [`Manager::retry_model`].
    pub fn on_catalog(
        &mut self,
        app: &mut App,
        repl: &Repl,
        provider: &str,
        result: Result<Vec<CatalogEntry>, String>,
        purpose: CatalogPurpose,
    ) -> Vec<String> {
        let entries = match result {
            Ok(entries) => {
                self.catalogs.insert(provider.to_owned(), entries.clone());
                entries
            }
            Err(error) => {
                let mut text = Text::styled(Style::Yellow, "catalog unavailable:");
                text.push(Style::Plain, format!(" {} ", sanitize(&error)));
                text.push(Style::Dim, "(enter a model name directly)");
                line(app, text);
                Vec::new()
            }
        };
        match purpose {
            CatalogPurpose::ModelPicker => self.open_model_picker(app, repl, &entries),
            CatalogPurpose::Manager => app.open_picker(Picker::catalog(provider, &entries)),
            CatalogPurpose::ModelFallback => {
                return self.resolve_pending(app, repl, provider, &entries)
            }
        }
        Vec::new()
    }

    // ---- /model --------------------------------------------------------------

    /// A bare `/model`: the picker, once the active provider's catalog is in.
    pub fn model_picker(&mut self, app: &mut App, repl: &Repl) {
        let active = active_provider(repl);
        match self.catalogs.get(&active).cloned() {
            Some(entries) => self.open_model_picker(app, repl, &entries),
            None => {
                let key = provider_key(repl, &active);
                self.request_catalog(app, repl, &active, key, CatalogPurpose::ModelPicker);
            }
        }
    }

    fn open_model_picker(&self, app: &mut App, repl: &Repl, entries: &[CatalogEntry]) {
        let extra = prefix_catalog_refs(&active_provider(repl), entries);
        let choices = model_choices(&repl.config, &extra);
        if choices.is_empty() {
            // Nothing to offer: start the typed form instead.
            app.editor.set_text("/model ");
        } else {
            app.open_picker(Picker::model(choices));
        }
    }

    /// Run `/model <arg>`. When it fails and `arg` looks like a bare model id, retry
    /// it as `<active provider>:<arg>`, but only if that provider's catalog lists
    /// `arg` exactly. Interface only: the plain loop and `run` stay strict.
    pub fn dispatch_model(&mut self, app: &mut App, repl: &mut Repl, arg: &str) {
        let before = repl.model_switches;
        let result = repl.handle_command(&format!("/model {arg}"));
        if repl.model_switches != before {
            if let Some(render) = &result.render {
                app.commit_text(render);
            }
            return;
        }
        let provider = active_provider(repl);
        if !is_bare_ref_candidate(arg, &provider, repl) {
            if let Some(render) = &result.render {
                app.commit_text(render);
            }
            return;
        }
        if let Some(entries) = self.catalogs.get(&provider).cloned() {
            self.finish_fallback(app, repl, arg, result.render.as_ref(), &provider, &entries);
            return;
        }
        // Never block: queue it with the provider active now, and make sure exactly
        // one fetch for that provider is outstanding.
        self.pending_model
            .push((arg.to_owned(), result.render.clone(), provider.clone()));
        self.ensure_fetch(app, repl, &provider);
    }

    fn ensure_fetch(&mut self, app: &mut App, repl: &Repl, provider: &str) {
        if self.inflight.insert(provider.to_owned()) {
            let key = provider_key(repl, provider);
            self.request_catalog(app, repl, provider, key, CatalogPurpose::ModelFallback);
        }
    }

    /// Answer the queued lines typed under `provider`. Lines queued under another
    /// provider (a switch can come in between) wait for their own catalog.
    fn resolve_pending(
        &mut self,
        app: &mut App,
        repl: &Repl,
        provider: &str,
        entries: &[CatalogEntry],
    ) -> Vec<String> {
        self.inflight.remove(provider);
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_model)
            .into_iter()
            .partition(|(_, _, queued)| queued == provider);
        self.pending_model = others;
        let retries = mine
            .into_iter()
            .filter_map(|(arg, strict, _)| {
                self.fallback_ref(app, &arg, strict.as_ref(), provider, entries)
            })
            .collect();
        // A safety net: every queued line keeps a fetch outstanding.
        let stranded: Vec<String> = self
            .pending_model
            .iter()
            .map(|(_, _, queued)| queued.clone())
            .collect();
        for queued in stranded {
            self.ensure_fetch(app, repl, &queued);
        }
        retries
    }

    fn finish_fallback(
        &mut self,
        app: &mut App,
        repl: &mut Repl,
        arg: &str,
        strict: Option<&Text>,
        provider: &str,
        entries: &[CatalogEntry],
    ) {
        if let Some(full) = self.fallback_ref(app, arg, strict, provider, entries) {
            if let Some(render) = repl.handle_command(&format!("/model {full}")).render {
                app.commit_text(&render);
            }
        }
    }

    /// The ref to retry, after saying so; or the strict error when the catalog
    /// does not list `arg` exactly.
    fn fallback_ref(
        &self,
        app: &mut App,
        arg: &str,
        strict: Option<&Text>,
        provider: &str,
        entries: &[CatalogEntry],
    ) -> Option<String> {
        let full = prefix_catalog_refs(provider, &[(arg.to_owned(), arg.to_owned())])
            .remove(0)
            .0;
        if !catalog_has_ref(arg, entries) || full == arg {
            if let Some(strict) = strict {
                app.commit_text(strict);
            }
            return None;
        }
        app.note(Style::Dim, &format!("resolved to {full}"));
        Some(full)
    }

    /// The retry for a queued `/model` line, run once its catalog arrives.
    pub fn retry_model(app: &mut App, repl: &mut Repl, full: &str) {
        if let Some(render) = repl.handle_command(&format!("/model {full}")).render {
            app.commit_text(&render);
        }
    }

    // ---- /config model -------------------------------------------------------

    pub fn open_provider_manager(&mut self, app: &mut App, repl: &Repl) {
        self.row = None;
        self.key = None;
        self.reference = None;
        let choices = provider_rows(repl)
            .into_iter()
            .map(|row| (row.label(), row.name))
            .collect();
        app.open_picker(Picker::providers(choices));
    }

    /// A provider was chosen: its catalog when it has a key or needs none, else
    /// key entry first. A provider without a key is never a dead end.
    pub fn on_provider(&mut self, app: &mut App, repl: &Repl, name: &str) {
        let Some(row) = provider_rows(repl).into_iter().find(|row| row.name == name) else {
            return;
        };
        self.row = Some(row.clone());
        if row.keyless || row.has_key {
            let key = provider_key(repl, &row.name);
            self.request_catalog(app, repl, &row.name, key, CatalogPurpose::Manager);
        } else {
            app.open_panel(Panel::Key(KeyEntry::new(
                KeyPurpose::Provider,
                &row.name,
                repl.secrets.available(),
            )));
        }
    }

    pub fn on_key_entered(&mut self, app: &mut App, repl: &Repl, key: Option<String>) {
        let Some(key) = key else {
            return;
        };
        let Some(row) = self.row.clone() else {
            // Nothing to test it against; keeping it could leak it into a later save.
            app.note(Style::Dim, "no provider selected; the key was discarded");
            return;
        };
        self.key = Some(key.clone());
        self.request_catalog(app, repl, &row.name, Some(key), CatalogPurpose::Manager);
    }

    /// A model was picked: test it for real, with the typed key if there is one.
    pub fn on_catalog_picked(&mut self, app: &mut App, repl: &Repl, reference: &str) {
        let parsed = match ModelRef::parse(reference) {
            Ok(parsed) => parsed,
            Err(error) => {
                let mut text = Text::styled(Style::Red, "error:");
                text.push(Style::Plain, format!(" {error}"));
                line(app, text);
                return;
            }
        };
        self.reference = Some(reference.to_owned());
        self.tests += 1;
        let id = self.tests;
        app.open_panel(Panel::Test(ConnectionTest::new(id, reference)));
        // Built here, where the keyring is; only the request runs on the worker.
        let built = match &self.key {
            Some(key) => provider_for_ref_with_key(&parsed, &repl.config, key),
            None => provider_for_ref_with_store(&parsed, &repl.config, &*repl.secrets),
        };
        let events = self.events.clone();
        std::thread::spawn(move || {
            let result = match built {
                Err(error) => TestResult::Failed(error.to_string()),
                Ok(provider) => probe(provider.as_ref(), &parsed.model),
            };
            let _ = events.send(UiEvent::Tested { id, result });
        });
    }

    /// The test ended. Returns the `/model` line to dispatch when it passed, so the
    /// switch applies to the next turn at once; the save panel is open by then.
    pub fn on_tested(
        &mut self,
        app: &mut App,
        repl: &Repl,
        outcome: TestOutcome,
    ) -> Option<String> {
        let error = outcome.error.as_deref().unwrap_or("None");
        if !outcome.ok && !outcome.override_ {
            let mut text = Text::styled(Style::Red, "connection test failed:");
            text.push(Style::Plain, format!(" {}", sanitize(error)));
            line(app, text);
            return None;
        }
        if !outcome.ok {
            let mut text = Text::styled(Style::Yellow, "saving anyway despite:");
            text.push(Style::Plain, format!(" {}", sanitize(error)));
            line(app, text);
        }
        // Only now does the key become persistent.
        if let (Some(key), Some(row)) = (self.key.take(), &self.row) {
            match repl.secrets.set_password(SERVICE_NAME, &row.name, &key) {
                Ok(()) => app.note(
                    Style::Dim,
                    &format!("key stored in the OS keyring for {}", row.name),
                ),
                Err(error) => {
                    let mut text = Text::styled(Style::Red, "could not store the key:");
                    text.push(
                        Style::Plain,
                        format!(" Could not store the key for '{}': {error}", row.name),
                    );
                    line(app, text);
                }
            }
        }
        let paths = repl
            .config_paths
            .clone()
            .unwrap_or_else(ConfigPaths::discover);
        app.open_panel(Panel::Save(SaveDiff::new(
            SavePurpose::Model,
            self.persist_changes(),
            paths,
        )));
        let reference = self.reference.clone().unwrap_or_default();
        outcome.ok.then(|| format!("/model {reference}"))
    }

    /// What a save writes: never a secret, only where the provider lives.
    fn persist_changes(&self) -> Vec<Change> {
        let path = |parts: &[&str]| parts.iter().map(|part| (*part).to_owned()).collect();
        let mut changes: Vec<Change> = vec![(
            path(&["model", "default"]),
            Some(self.reference.clone().unwrap_or_default().into()),
        )];
        if let Some(row) = &self.row {
            if !row.keyless && row.api_key_env.is_none() {
                changes.push((
                    path(&["providers", &row.name, "api_key_env"]),
                    Some(format!("{}_API_KEY", row.name.to_uppercase()).into()),
                ));
            }
            if let Some(base_url) = &row.base_url {
                changes.push((
                    path(&["providers", &row.name, "base_url"]),
                    Some(base_url.clone().into()),
                ));
            }
        }
        changes
    }

    pub fn on_saved(app: &mut App, repl: &mut Repl, edit: Option<ConfigEdit>) {
        let Some(edit) = edit else {
            app.note(
                Style::Dim,
                "not saved — the model switch applies to this session only",
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
            }
            Err(error) => {
                let mut text = Text::styled(Style::Red, "could not save:");
                text.push(Style::Plain, format!(" {}", sanitize(&error.0)));
                line(app, text);
            }
        }
    }
}

/// A real one-token completion, so a pass proves the key, the address, the model
/// name, and the completion path together. Capped at [`TEST_TIMEOUT`].
fn probe(provider: &dyn Provider, model: &str) -> TestResult {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return TestResult::Failed(error.to_string()),
    };
    let request = CompletionRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: "hi".to_owned(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: Some(1),
        stream: false,
    };
    // The timer belongs to the runtime, so it is made inside it.
    let completed = runtime
        .block_on(async { tokio::time::timeout(TEST_TIMEOUT, provider.complete(request)).await });
    match completed {
        Ok(Ok(_)) => TestResult::Passed,
        Ok(Err(error)) => TestResult::Failed(error.to_string()),
        Err(_) => TestResult::TimedOut(format!(
            "Timed out after {}s — no response.",
            TEST_TIMEOUT.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, Overrides, ProviderConfig};
    use crate::secrets::NoSecretStore;
    use crate::ui::status::StatusState;

    /// A session on a keyless local provider that nothing listens on, with memory
    /// off, so nothing reaches the home directory, the keyring, or the network.
    fn repl() -> Repl {
        let paths = ConfigPaths {
            user: None,
            project: std::path::PathBuf::from("no-such-dir/config.toml"),
        };
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
        Repl::new(config, None, None, Box::new(NoSecretStore)).unwrap()
    }

    fn app() -> App {
        let mut app = App::new(StatusState::default(), "s1".into(), false);
        app.set_size(100, 40);
        app
    }

    fn printed(app: &mut App) -> Vec<String> {
        app.take_committed()
            .iter()
            .map(|line| line.plain_text())
            .collect()
    }

    fn manager() -> (Manager, mpsc::Receiver<UiEvent>) {
        let (events, inbox) = mpsc::channel();
        (Manager::new(events), inbox)
    }

    #[test]
    fn a_bare_model_id_is_resolved_only_when_the_catalog_lists_it() {
        let (mut manager, _inbox) = manager();
        let (mut app, mut repl) = (app(), repl());
        manager.catalogs.insert(
            "local".into(),
            vec![("vendor/model".into(), "Vendor Model".into())],
        );
        manager.dispatch_model(&mut app, &mut repl, "vendor/model");
        assert_eq!(
            printed(&mut app),
            [
                "resolved to local:vendor/model",
                "model: openai:vendor/model"
            ]
        );
        assert_eq!(repl.model_ref(), "local:vendor/model");

        // A typo gets the strict error, unchanged.
        manager.dispatch_model(&mut app, &mut repl, "vendor/modle");
        let lines = printed(&mut app);
        assert!(lines[0].contains("vendor/modle"), "{lines:?}");
        assert_eq!(repl.model_ref(), "local:vendor/model");

        // A ref naming a configured provider fails on its own terms, with no fetch.
        manager.dispatch_model(&mut app, &mut repl, "openai:gpt-4o");
        assert!(!printed(&mut app)
            .iter()
            .any(|line| line.contains("fetching")));
    }

    #[test]
    fn a_cold_catalog_queues_the_line_until_it_arrives() {
        let (mut manager, _inbox) = manager();
        let (mut app, mut repl) = (app(), repl());
        manager.dispatch_model(&mut app, &mut repl, "vendor/model");
        manager.dispatch_model(&mut app, &mut repl, "other/model");
        assert_eq!(
            printed(&mut app),
            ["fetching local model catalog…"],
            "one fetch for both lines"
        );
        let entries = vec![("vendor/model".to_owned(), "vendor/model".to_owned())];
        let retries = manager.on_catalog(
            &mut app,
            &repl,
            "local",
            Ok(entries),
            CatalogPurpose::ModelFallback,
        );
        assert_eq!(retries, ["local:vendor/model"]);
        let lines = printed(&mut app);
        assert_eq!(lines[0], "resolved to local:vendor/model");
        assert!(
            lines[1].contains("other/model"),
            "the strict error: {lines:?}"
        );
        assert!(manager.pending_model.is_empty() && manager.inflight.is_empty());
    }

    #[test]
    fn a_failed_catalog_says_so_and_still_opens_the_picker() {
        let (mut manager, _inbox) = manager();
        let (mut app, repl) = (app(), repl());
        manager.on_catalog(
            &mut app,
            &repl,
            "llamacpp",
            Err("llama.cpp / LM Studio expose no model list".into()),
            CatalogPurpose::Manager,
        );
        assert_eq!(
            printed(&mut app),
            [
                "catalog unavailable: llama.cpp / LM Studio expose no model list \
                 (enter a model name directly)"
            ]
        );
        assert!(matches!(app.panel(), Some(Panel::Picker(_))));
        assert!(
            !manager.catalogs.contains_key("llamacpp"),
            "failures are not cached"
        );
    }

    #[test]
    fn the_provider_list_shows_key_status_in_config_order() {
        let repl = repl();
        let rows = provider_rows(&repl);
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "anthropic",
                "openai",
                "openrouter",
                "groq",
                "ollama",
                "llamacpp",
                "local"
            ]
        );
        let local = rows.last().unwrap();
        assert_eq!(
            local.label(),
            "local  http://127.0.0.1:9/v1  (local, no key needed)"
        );
        // The environment may hold real keys; only the shape is checked here.
        assert!(matches!(rows[1].status(), "key ok" | "no key"));
    }

    /// Answer one HTTP request with `response`, on a local port.
    fn serve_once(response: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut buffer = [0u8; 8192];
                let _ = socket.read(&mut buffer);
                let _ = socket.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/v1")
    }

    #[test]
    fn the_probe_is_a_real_completion() {
        let mut repl = repl();
        let probe_at = |repl: &mut Repl, base_url: String| {
            repl.config.providers.insert(
                "probe".into(),
                ProviderConfig {
                    base_url: Some(base_url),
                    max_retries: 0,
                    ..Default::default()
                },
            );
            let reference = ModelRef::parse("probe:m").unwrap();
            let provider =
                provider_for_ref_with_store(&reference, &repl.config, &NoSecretStore).unwrap();
            probe(provider.as_ref(), "m")
        };
        let passed = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"h\"},\"finish_reason\":\"length\"}]}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(probe_at(&mut repl, passed), TestResult::Passed);
        let refused = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\n\
             Content-Length: 21\r\nConnection: close\r\n\r\n{\"error\":\"bad key 1\"}",
        );
        match probe_at(&mut repl, refused) {
            TestResult::Failed(message) => assert!(message.contains("401"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_passing_test_switches_the_model_and_offers_the_save() {
        let (mut manager, _inbox) = manager();
        let (mut app, repl) = (app(), repl());
        manager.on_key_entered(&mut app, &repl, Some("gsk_typed".into()));
        assert_eq!(
            printed(&mut app),
            ["no provider selected; the key was discarded"]
        );

        manager.row = provider_rows(&repl).pop();
        manager.reference = Some("local:vendor/model".into());
        let failed = TestOutcome {
            ok: false,
            error: Some("HTTP 404".into()),
            override_: false,
        };
        assert_eq!(manager.on_tested(&mut app, &repl, failed), None);
        assert_eq!(printed(&mut app), ["connection test failed: HTTP 404"]);
        assert!(app.panel().is_none());

        let passed = TestOutcome {
            ok: true,
            error: None,
            override_: false,
        };
        assert_eq!(
            manager.on_tested(&mut app, &repl, passed),
            Some("/model local:vendor/model".into())
        );
        assert!(matches!(app.panel(), Some(Panel::Save(_))));
        let changes: Vec<(String, String)> = manager
            .persist_changes()
            .into_iter()
            .map(|(path, value)| (path.join("."), value.unwrap().as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(
            changes,
            [
                ("model.default".to_owned(), "local:vendor/model".to_owned()),
                (
                    "providers.local.base_url".to_owned(),
                    "http://127.0.0.1:9/v1".to_owned()
                ),
            ]
        );

        let anyway = TestOutcome {
            ok: false,
            error: Some("HTTP 500".into()),
            override_: true,
        };
        assert_eq!(manager.on_tested(&mut app, &repl, anyway), None);
        assert_eq!(printed(&mut app), ["saving anyway despite: HTTP 500"]);
        assert!(matches!(app.panel(), Some(Panel::Save(_))));
    }
}
