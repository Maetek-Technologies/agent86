//! The ACP [`Backend`] over agent86's own harness: each session is a harness built
//! by [`assemble_in`] for the session's directory, a saved [`AgentState`], and the
//! memory database's session store, so an ACP session is an ordinary agent86
//! session (`agent86 run --session <id>` can continue it).
//!
//! Nothing here writes to stdout; the protocol owns it. Notes for the user go to
//! stderr.
//!
//! Tool calls are reported to the editor as they happen. A call the approval mode
//! asks about becomes a `session/request_permission` request, through the gate's
//! per-call prompt: `ask` mode asks for every side-effecting call, `auto` asks for
//! none (except a write into agent86's own configuration, which is asked about in
//! every mode but `deny`), and `deny` refuses them. The model never sees a vault
//! secret in what the editor is shown: arguments and previews are redacted first.
//!
//! A turn is cancelled through the harness's own [`CancelToken`], as Ctrl+C does in
//! the interface: it stops at the next safe point, a running tool finishes, and every
//! call in the history gets an answer. A loaded session replays its saved messages
//! as updates; the saved state keeps no per-call status or reasoning, so a replayed
//! call is shown `completed` and thoughts are not replayed.
//!
//! When the editor offers `fs/read_text_file` and `fs/write_text_file`, the file
//! tools go through them (see [`AcpFiles`]): reads see unsaved buffers, diffs are
//! made against them, and writes land in the editor, which owns the file. Anything
//! else the agent runs (a command, a Python snippet) still sees the disk.
//!
//! The MCP servers the client names are started for the session and their tools
//! registered beside the built-ins and the configured servers', behind the same
//! approvals. They run as the user, like configured ones; the editor chose them.
//! A server that fails to start is reported on stderr and the session goes on.

use super::{
    Backend, BackendError, CancelHandle, ClientCapabilities, CommandInfo, ConfigOption, McpSpec,
    Notifier, Permission, PermissionRequest, Stop, ToolKind, TurnSink, Update,
};
use crate::agent::{Harness, HarnessError, TurnEvent};
use crate::approval::ApprovalPreview;
use crate::config::{Config, McpServerConfig};
use crate::escape::show_controls;
use crate::mcp::{var_ref_names, McpManager};
use crate::pyjson::python_json_dumps;
use crate::secrets::NoSecretStore;
use crate::session::{AgentState, SessionStore};
use crate::setup::{assemble_in, SetupError};
use crate::tools::EditorFiles;
use crate::types::{ApprovalMode, Role, ToolCall, ToolResult};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long the editor has to answer a file request before the tool goes on without
/// it (a read falls back to the disk; a write fails).
const FILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Model lists by provider, shared with the threads that fetch them.
type Catalogs = Arc<Mutex<BTreeMap<String, Vec<(String, String)>>>>;

/// How many characters of a call's arguments go into its title.
const TITLE_ARGS: usize = 100;

/// The running turn's sink, which the session's approval hook reads. Set for the
/// length of a turn; with none set, the hook declines.
type SinkSlot = Arc<Mutex<Option<Arc<dyn TurnSink>>>>;

struct Session {
    harness: Harness,
    /// The `provider:model` the session runs on: the config's default until `/model`
    /// or the editor's picker switches it. Kept as the ref the user wrote, not read
    /// back from the provider, whose own name is the adapter's (`openai` for every
    /// OpenAI-compatible provider).
    model_ref: String,
    store: Option<SessionStore>,
    state: AgentState,
    sink: SinkSlot,
}

/// Sessions on agent86's harness, driven on one single-threaded runtime.
pub struct HarnessBackend {
    config: Config,
    /// Model lists fetched from providers, by provider, as `provider:id` refs with
    /// their labels. Filled by background threads; read when options are built.
    catalogs: Catalogs,
    /// The sessions that are open, so a list arriving late can be sent to each.
    open: Arc<Mutex<Vec<String>>>,
    notifier: Option<Notifier>,
    /// Whether the lists have been asked for yet: once, on the first session.
    lists_started: bool,
    client: ClientCapabilities,
    runtime: tokio::runtime::Runtime,
    sessions: HashMap<String, Session>,
}

impl HarnessBackend {
    pub fn new(config: Config) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            config,
            catalogs: Arc::default(),
            open: Arc::default(),
            notifier: None,
            lists_started: false,
            client: ClientCapabilities::default(),
            runtime,
            sessions: HashMap::new(),
        })
    }
}

impl HarnessBackend {
    /// Build a session's harness for `cwd`, on `resume`'s saved state when given
    /// (`not_found` when the store has none), else on a fresh one, and keep it.
    fn open(
        &mut self,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
        resume: Option<&str>,
    ) -> Result<String, BackendError> {
        // No terminal prompt and no secret store: the editor is asked instead (below),
        // and the keyring is for a terminal. Without a terminal prompt,
        // `request_secret` is not offered.
        let mut assembly =
            assemble_in(&self.config, cwd, None, None).map_err(|error| match error {
                SetupError::Model(message) => BackendError::internal(message),
                SetupError::Workspace(message) => BackendError::invalid_params(message),
            })?;
        if let Some(note) = &assembly.memory_note {
            eprintln!("memory: {note}");
        }
        if let Some(note) = &assembly.sandbox_note {
            eprintln!("sandbox: {note}");
        }
        start_client_servers(&self.config, &mut assembly.harness, &mcp_servers);

        // Approvals go to the editor, through whichever sink the running turn has.
        let sink: SinkSlot = Arc::default();
        let secrets = assembly.harness.context.secrets.clone();
        let hook_sink = Arc::clone(&sink);
        assembly.harness.gate.call_prompt = Some(Box::new(
            move |call: &ToolCall, preview: &ApprovalPreview| {
                let running = hook_sink
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                let Some(running) = running else {
                    return false;
                };
                let request = PermissionRequest {
                    tool_call_id: call.id.clone(),
                    title: title(call, &secrets),
                    kind: tool_kind(&call.name),
                    raw_input: redacted_input(call, &secrets),
                    text: approval_text(preview),
                };
                running.ask(request) == Permission::Allowed
            },
        ));

        let state = match resume {
            None => AgentState::new(),
            Some(id) => match assembly.store.as_ref().map(|store| store.load(id)) {
                Some(Ok(Some(state))) => state,
                Some(Err(error)) => return Err(BackendError::internal(error.to_string())),
                _ => return Err(BackendError::not_found(format!("Session not found: {id}"))),
            },
        };
        let session_id = state.session_id.clone();
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(session_id.clone());
        self.start_model_lists();
        // The file tools use the editor's files when it offers any.
        if self.client.read_text_file || self.client.write_text_file {
            assembly.harness.context.editor = Some(Arc::new(AcpFiles {
                sink: Arc::clone(&sink),
                session_id: session_id.clone(),
                read: self.client.read_text_file,
                write: self.client.write_text_file,
            }));
        }
        let session = Session {
            model_ref: assembly.model_ref.to_string(),
            harness: assembly.harness,
            store: assembly.store,
            state,
            sink,
        };
        persist(&session.store, &session.state);
        self.sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }
}

/// The editor's files, reached through the running turn's sink. Between turns there
/// is no sink, so nothing is asked and the disk is used. Reads fall back to the disk
/// when the editor cannot give the text (it does not know the file, or does not
/// answer); a write the editor refuses fails the call instead, since writing the
/// disk behind its back could lose the buffer's unsaved changes.
struct AcpFiles {
    sink: SinkSlot,
    session_id: String,
    read: bool,
    write: bool,
}

impl std::fmt::Debug for AcpFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpFiles")
            .field("session_id", &self.session_id)
            .field("read", &self.read)
            .field("write", &self.write)
            .finish()
    }
}

impl AcpFiles {
    fn running(&self) -> Option<Arc<dyn TurnSink>> {
        self.sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl EditorFiles for AcpFiles {
    fn read_text(&self, path: &Path) -> Option<String> {
        if !self.read {
            return None;
        }
        let sink = self.running()?;
        let result = sink
            .call(
                "fs/read_text_file",
                serde_json::json!({ "sessionId": self.session_id, "path": path.to_str()? }),
                Some(FILE_TIMEOUT),
            )
            .ok()?;
        result
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn write_text(&self, path: &Path, content: &str) -> Option<Result<(), String>> {
        if !self.write {
            return None;
        }
        let sink = self.running()?;
        let path = path.to_str()?;
        Some(
            sink.call(
                "fs/write_text_file",
                serde_json::json!({
                    "sessionId": self.session_id,
                    "path": path,
                    "content": content
                }),
                Some(FILE_TIMEOUT),
            )
            .map(|_| ()),
        )
    }
}

/// The slash commands an editor session offers: the ones that make sense with the
/// editor owning the screen, and that act on this session alone. The rest of the
/// terminal's commands (`/resume`, `/clear`, `/thinking`, `/secret`, ...) are the
/// editor's to provide, or need a terminal.
const COMMANDS: [(&str, &str, Option<&str>); 4] = [
    (
        "mode",
        "Show or set the approval mode for this session",
        Some("ask, auto, or deny"),
    ),
    (
        "model",
        "Show or switch the model for this session",
        Some("provider:model"),
    ),
    ("cost", "Show token usage and cost this session", None),
    ("tools", "List the tools the agent can use", None),
];

impl HarnessBackend {
    /// `/model <ref>`, as the terminal's: the current model stays on a refusal, and a
    /// chosen model is pinned, which stops triage routing.
    fn switch_model(&self, session: &mut Session, arg: &str) -> String {
        if arg.is_empty() {
            return format!(
                "current model: {}\nusage: /model <provider:model>",
                session.model_ref
            );
        }
        match crate::setup::build_provider(&self.config, arg) {
            Err(message) => message,
            Ok((provider, reference)) => {
                let harness = &mut session.harness;
                harness.configure_output(&self.config, &reference.provider);
                harness.context_window =
                    crate::setup::model_context_window(&self.config, &reference.to_string());
                harness.provider = provider.into();
                harness.router.pin();
                session.model_ref = reference.to_string();
                format!("model: {}", session.model_ref)
            }
        }
    }

    /// `/cost`, as the terminal's.
    fn cost(&self, session: &Session) -> String {
        let usage = &session.state.usage;
        let harness = &session.harness;
        let reference = harness
            .routed_ref()
            .unwrap_or_else(|| format!("{}:{}", harness.provider.name(), harness.provider.model()));
        let priced = crate::pricing::PriceTable::from_config(&self.config).is_priced(&reference);
        let cost = crate::pricing::format_cost(usage.cost_usd, priced);
        let cost_text = if cost == crate::pricing::UNPRICED_LABEL {
            cost
        } else {
            format!("cost {cost}")
        };
        let mut line = format!(
            "steps {}  in {}  out {} tok  {cost_text}",
            session.state.step_count(),
            usage.input_tokens,
            usage.output_tokens
        );
        if usage.cache_read_tokens > 0 || usage.cache_creation_tokens > 0 {
            line.push_str(&format!(
                "\ncache read {}  written {} tok",
                usage.cache_read_tokens, usage.cache_creation_tokens
            ));
        }
        line
    }
}

impl HarnessBackend {
    /// Ask each provider that can answer for its model list, in the background, once.
    /// A provider is asked when its key resolves (the environment, then the keyring),
    /// or when it is Ollama, which has none. Each list that arrives is kept and every
    /// open session told, so the picker fills in after the session is already usable;
    /// a failure is a line on stderr and nothing else. `AGENT86_ACP_MODEL_LISTS`
    /// narrows it: `off` asks nobody (a picker of just the configured models), and a
    /// comma-separated list of provider names asks only those.
    fn start_model_lists(&mut self) {
        if std::mem::replace(&mut self.lists_started, true) {
            return;
        }
        let setting = std::env::var("AGENT86_ACP_MODEL_LISTS").unwrap_or_default();
        let only: Option<Vec<String>> = match setting.trim() {
            "" => None,
            "off" | "none" | "0" | "false" => return,
            names => Some(
                names
                    .split(',')
                    .map(|name| name.trim().to_owned())
                    .collect(),
            ),
        };
        for (name, section) in &self.config.providers {
            if name == "llamacpp" || only.as_ref().is_some_and(|only| !only.contains(name)) {
                continue;
            }
            // Resolved here: the keyring handle is not for other threads.
            let key = crate::secrets::resolve_api_key(
                name,
                section.api_key_env.as_deref(),
                &crate::secrets::OsKeyring,
            );
            if key.is_none() && name != "ollama" {
                continue;
            }
            let (name, section) = (name.clone(), section.clone());
            let catalogs = Arc::clone(&self.catalogs);
            let open = Arc::clone(&self.open);
            let notifier = self.notifier.clone();
            std::thread::spawn(move || {
                match crate::provider::catalog::fetch_catalog(&name, &section, key.as_deref()) {
                    Ok(entries) => {
                        let entries =
                            crate::provider::catalog::prefix_catalog_refs(&name, &entries);
                        catalogs
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(name, entries);
                        let sessions = open.lock().unwrap_or_else(PoisonError::into_inner).clone();
                        if let Some(notify) = notifier {
                            for id in sessions {
                                notify(&id);
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("acp: model list for '{name}' unavailable: {}", error.0)
                    }
                }
            });
        }
    }

    /// The selectors an editor shows: the approval mode, and the models the config
    /// names (the default and the two routes, which is all the config lists) with
    /// the one in use, which `/model` may have set to something else.
    fn options_for(&self, session: &Session) -> Vec<ConfigOption> {
        let mut models: Vec<String> = Vec::new();
        for name in [
            self.config.model.default.as_str(),
            self.config.model.route.cheap.as_str(),
            self.config.model.route.frontier.as_str(),
            session.model_ref.as_str(),
        ] {
            if !name.is_empty() && !models.iter().any(|known| known == name) {
                models.push(name.to_owned());
            }
        }
        let mode = session.harness.gate.mode();
        // The configured models first, then each provider's own list, minus what is
        // already above (a value must be listed once).
        let labelled: Vec<(String, String)> = models
            .iter()
            .map(|model| (model.clone(), model.clone()))
            .collect();
        let mut groups = vec![("Configured".to_owned(), labelled.clone())];
        let mut choices = labelled;
        for (provider, entries) in self
            .catalogs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let fresh: Vec<(String, String)> = entries
                .iter()
                .filter(|(value, _)| !choices.iter().any(|(known, _)| known == value))
                .cloned()
                .collect();
            if !fresh.is_empty() {
                choices.extend(fresh.iter().cloned());
                groups.push((provider.clone(), fresh));
            }
        }
        // One group would only add a heading to the short list.
        if groups.len() == 1 {
            groups.clear();
        }
        vec![
            ConfigOption {
                id: "mode".into(),
                name: "Approval mode".into(),
                description: Some(
                    "ask: confirm each change; auto: run without asking; deny: refuse changes"
                        .into(),
                ),
                category: "mode",
                current: mode.as_str().to_owned(),
                choices: ["ask", "auto", "deny"]
                    .iter()
                    .map(|value| ((*value).to_owned(), (*value).to_owned()))
                    .collect(),
                groups: Vec::new(),
            },
            ConfigOption {
                id: "model".into(),
                name: "Model".into(),
                description: Some(
                    "provider:model; /model switches to any other, for example one not listed"
                        .into(),
                ),
                category: "model",
                current: session.model_ref.clone(),
                choices,
                groups,
            },
        ]
    }
}

impl Backend for HarnessBackend {
    fn set_notifier(&mut self, notify: Notifier) {
        self.notifier = Some(notify);
    }

    fn config_options(&self, session_id: &str) -> Vec<ConfigOption> {
        self.sessions
            .get(session_id)
            .map(|session| self.options_for(session))
            .unwrap_or_default()
    }

    fn set_config_option(
        &mut self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<Vec<ConfigOption>, BackendError> {
        let mut session = self.sessions.remove(session_id).ok_or_else(|| {
            BackendError::invalid_params(format!("Unknown session: {session_id}"))
        })?;
        let outcome = match config_id {
            "mode" => match ApprovalMode::parse(value) {
                Some(mode) => {
                    session.harness.gate.set_mode(mode);
                    Ok(())
                }
                None => Err(format!("'{value}' is not one of ask, auto, deny")),
            },
            "model" => {
                // Only the listed choices: the picker offers them, and a free-form
                // ref is what `/model` is for.
                let listed = self.options_for(&session).iter().any(|option| {
                    option.id == "model" && option.choices.iter().any(|(v, _)| v == value)
                });
                if !listed {
                    Err(format!("'{value}' is not one of the listed models"))
                } else if value == session.model_ref {
                    Ok(())
                } else {
                    let said = self.switch_model(&mut session, value);
                    // A refusal leaves the model as it was, and says why.
                    if session.model_ref == value {
                        Ok(())
                    } else {
                        Err(said)
                    }
                }
            }
            other => Err(format!("Unknown config option: {other}")),
        };
        let options = self.options_for(&session);
        self.sessions.insert(session_id.to_owned(), session);
        outcome
            .map(|()| options)
            .map_err(BackendError::invalid_params)
    }

    fn commands(&self) -> Vec<CommandInfo> {
        COMMANDS
            .iter()
            .map(|(name, description, hint)| CommandInfo {
                name: (*name).to_owned(),
                description: (*description).to_owned(),
                hint: hint.map(str::to_owned),
            })
            .collect()
    }

    fn run_command(&mut self, session_id: &str, line: &str) -> Option<String> {
        let (name, arg) = match line.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (line, ""),
        };
        let name = name.strip_prefix('/')?;
        if !COMMANDS.iter().any(|(known, _, _)| *known == name) {
            return None;
        }
        // Take the session out so `self` can be borrowed beside it, and put it back.
        let mut session = self.sessions.remove(session_id)?;
        let output = match name {
            "mode" => {
                let gate = &session.harness.gate;
                if arg.is_empty() {
                    format!("approval mode: {}", gate.mode().as_str())
                } else {
                    match ApprovalMode::parse(arg) {
                        Some(mode) => {
                            gate.set_mode(mode);
                            format!("approval mode: {}", gate.mode().as_str())
                        }
                        None => format!("unknown mode '{arg}' (ask|auto|deny)"),
                    }
                }
            }
            "model" => self.switch_model(&mut session, arg),
            "cost" => self.cost(&session),
            _ => {
                let mut lines = vec![format!(
                    "tools: {}",
                    session.harness.registry.names().join(", ")
                )];
                if let Some(mcp) = &session.harness.mcp {
                    lines.extend(
                        mcp.server_statuses()
                            .into_iter()
                            .map(|(name, status)| format!("mcp {name}: {status}")),
                    );
                }
                lines.join("\n")
            }
        };
        self.sessions.insert(session_id.to_owned(), session);
        Some(output)
    }

    fn client_capabilities(&mut self, capabilities: ClientCapabilities) {
        self.client = capabilities;
    }

    fn new_session(
        &mut self,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
    ) -> Result<String, BackendError> {
        self.open(cwd, mcp_servers, None)
    }

    fn load_session(
        &mut self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
    ) -> Result<Vec<Update>, BackendError> {
        // Already open in this process: its harness stays, and its history replays.
        if !self.sessions.contains_key(session_id) {
            self.open(cwd, mcp_servers, Some(session_id))?;
        }
        let session = &self.sessions[session_id];
        Ok(replay(&session.state, &session.harness.context.secrets))
    }

    fn cancel_handle(&self, session_id: &str) -> Option<CancelHandle> {
        let token = self.sessions.get(session_id)?.harness.cancel.clone();
        Some(Arc::new(move || token.cancel()))
    }

    fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        sink: Arc<dyn TurnSink>,
    ) -> Result<Stop, BackendError> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| {
            BackendError::invalid_params(format!("Unknown session: {session_id}"))
        })?;
        let Session {
            harness,
            store,
            state,
            sink: slot,
            ..
        } = session;
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&sink));
        let secrets = harness.context.secrets.clone();

        let result = self
            .runtime
            .block_on(
                harness.run_turn_events(state, text, text, |event| match event {
                    TurnEvent::Text(chunk) | TurnEvent::Notice(chunk) if !chunk.is_empty() => {
                        sink.update(Update::Message(chunk.to_owned()))
                    }
                    TurnEvent::Thinking(chunk) if !chunk.is_empty() => {
                        sink.update(Update::Thought(chunk.to_owned()))
                    }
                    TurnEvent::ToolCall { call, .. } => sink.update(Update::ToolCall {
                        id: call.id.clone(),
                        title: title(call, &secrets),
                        name: call.name.clone(),
                        kind: tool_kind(&call.name),
                        raw_input: redacted_input(call, &secrets),
                    }),
                    TurnEvent::ToolResult {
                        call,
                        result,
                        observation,
                        ..
                    } => sink.update(Update::ToolResult {
                        id: call.id.clone(),
                        ok: result.ok,
                        text: result_text(result, observation),
                    }),
                    _ => {}
                }),
            );
        // The turn is over: nothing may be asked of the editor until the next one.
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        // Saved whether the turn finished or failed, so the session stays resumable.
        persist(store, state);
        match result {
            Ok(_) if harness.cancel.is_cancelled() => Ok(Stop::Cancelled),
            Ok(_) => Ok(Stop::EndTurn),
            Err(HarnessError::CircuitTripped(reason)) if reason.starts_with("step budget") => {
                Ok(Stop::MaxTurnRequests)
            }
            Err(error) => Err(BackendError::internal(error.to_string())),
        }
    }
}

/// Start the servers the client named and register their tools. A session with no
/// configured servers has no manager yet, so one is made. A server the client names
/// replaces a configured one of the same name for this session only: each session
/// has its own manager.
fn start_client_servers(config: &Config, harness: &mut Harness, servers: &[McpSpec]) {
    if servers.is_empty() {
        return;
    }
    let manager = match &harness.mcp {
        Some(manager) => Arc::clone(manager),
        None => {
            let manager = Arc::new(McpManager::empty(config));
            manager.attach(&harness.registry);
            harness.mcp = Some(Arc::clone(&manager));
            manager
        }
    };
    for spec in servers {
        let (server, literal) = server_config(spec);
        let name = spec.name();
        match manager.start_server(
            name,
            &server,
            &literal,
            &NoSecretStore,
            server.connect_timeout_s,
        ) {
            Ok(_) => {
                let (_, collisions) = manager.mount(name);
                if !collisions.is_empty() {
                    eprintln!(
                        "acp: MCP server '{name}': tools already taken by another server \
                         were skipped: {}",
                        collisions.join(", ")
                    );
                }
            }
            Err(reason) => {
                eprintln!("acp: MCP server '{name}' from the client failed to start: {reason}");
            }
        }
    }
}

/// The config for a server the client named, and the overrides that keep every
/// `${VAR}` in its values literal. The config layer expands `${VAR}` from the
/// environment and the keyring, which is right for a file the user wrote and wrong
/// for a value that arrives over the protocol: it must not be able to ask for the
/// agent's own keys. An override wins over both, so each reference maps to itself.
fn server_config(spec: &McpSpec) -> (McpServerConfig, HashMap<String, String>) {
    let mut texts: Vec<&str> = Vec::new();
    let value = match spec {
        McpSpec::Stdio {
            command, args, env, ..
        } => {
            texts.extend(args.iter().map(String::as_str));
            texts.extend(env.iter().map(|(_, value)| value.as_str()));
            serde_json::json!({
                "command": command,
                "args": args,
                "env": pairs_object(env),
                "transport": "stdio",
            })
        }
        McpSpec::Remote {
            url, sse, headers, ..
        } => {
            texts.push(url);
            texts.extend(headers.iter().map(|(_, value)| value.as_str()));
            serde_json::json!({
                "url": url,
                "headers": pairs_object(headers),
                "transport": if *sse { "sse" } else { "http" },
            })
        }
    };
    let literal = texts
        .iter()
        .flat_map(|text| var_ref_names(text))
        .map(|name| (name.clone(), format!("${{{name}}}")))
        .collect();
    let config = serde_json::from_value(value).expect("a server config always deserializes");
    (config, literal)
}

/// `[(name, value)]` as a JSON object, in order.
fn pairs_object(pairs: &[(String, String)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect(),
    )
}

/// A session's saved messages as the updates that tell them again: the user's
/// words, the agent's replies, each call it made, and what came back. The system
/// message is rebuilt per request and not part of the history.
fn replay(state: &AgentState, secrets: &crate::vault::SecretVault) -> Vec<Update> {
    let mut updates = Vec::new();
    for message in &state.messages {
        match message.role {
            Role::User if !message.content.is_empty() => {
                updates.push(Update::User(message.content.clone()))
            }
            Role::Assistant => {
                if !message.content.is_empty() {
                    updates.push(Update::Message(message.content.clone()));
                }
                for call in &message.tool_calls {
                    updates.push(Update::ToolCall {
                        id: call.id.clone(),
                        title: title(call, secrets),
                        name: call.name.clone(),
                        kind: tool_kind(&call.name),
                        raw_input: redacted_input(call, secrets),
                    });
                }
            }
            Role::Tool => {
                if let Some(id) = &message.tool_call_id {
                    updates.push(Update::ToolResult {
                        id: id.clone(),
                        ok: true,
                        text: message.content.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    updates
}

fn persist(store: &Option<SessionStore>, state: &AgentState) {
    if let Some(Err(error)) = store.as_ref().map(|store| store.persist(state)) {
        eprintln!("memory: could not save the session: {error}");
    }
}

/// The category of a built-in tool; an MCP server's tools and the rest are `other`.
pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" | "list_dir" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "run_command" | "python_exec" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// `name(arguments)`, the arguments as the model sent them, capped, with vault
/// secrets and control characters dealt with: the name of a tool comes from the
/// model (an MCP server names its own), and the editor shows the title as text.
fn title(call: &ToolCall, secrets: &crate::vault::SecretVault) -> String {
    let arguments = python_json_dumps(&Value::Object(call.arguments.clone()), false);
    let arguments = secrets.redact(&arguments);
    let arguments = if arguments.chars().count() > TITLE_ARGS {
        format!(
            "{} ...",
            arguments.chars().take(TITLE_ARGS).collect::<String>()
        )
    } else {
        arguments
    };
    show_controls(&format!("{}({arguments})", call.name))
}

/// The call's arguments for the editor, with every vault value replaced by its
/// `${secret:NAME}` placeholder.
fn redacted_input(call: &ToolCall, secrets: &crate::vault::SecretVault) -> Value {
    let mut input = Value::Object(call.arguments.clone());
    secrets.redact_value(&mut input);
    input
}

/// What the user decides on: the warning if there is one, then the tool's own
/// detail (a diff, a command) or the argument summary, controls shown not sent.
/// Already redacted by the preview.
fn approval_text(preview: &ApprovalPreview) -> String {
    let detail = preview.detail.as_deref().unwrap_or(&preview.summary);
    let text = match &preview.warning {
        Some(warning) => format!("Warning: {warning}\n\n{detail}"),
        None => detail.to_owned(),
    };
    show_controls(&text)
}

/// A result for the editor: what the model is shown, or the error when that is
/// empty.
fn result_text(result: &ToolResult, observation: &str) -> String {
    if !observation.trim().is_empty() {
        return observation.to_owned();
    }
    match result.error.as_deref().filter(|error| !error.is_empty()) {
        Some(error) => error.to_owned(),
        None => "(no output)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    #[test]
    fn built_in_tools_have_kinds_and_the_rest_are_other() {
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("list_dir"), ToolKind::Read);
        assert_eq!(tool_kind("write_file"), ToolKind::Edit);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("run_command"), ToolKind::Execute);
        assert_eq!(tool_kind("python_exec"), ToolKind::Execute);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind("firecrawl_scrape"), ToolKind::Other);
    }

    #[test]
    fn a_title_names_the_tool_and_caps_and_cleans_its_arguments() {
        let secrets = crate::vault::SecretVault::default();
        let short = title(&call("read_file", json!({"path": "a.rs"})), &secrets);
        assert_eq!(short, r#"read_file({"path": "a.rs"})"#);

        let long = title(
            &call("write_file", json!({"content": "x".repeat(500)})),
            &secrets,
        );
        assert!(long.ends_with(" ...)"), "{long}");
        assert!(long.chars().count() < 140, "{}", long.chars().count());

        // A tool name from an MCP server can carry control characters.
        let hostile = title(&call("evil\u{1b}[2J", json!({})), &secrets);
        assert!(!hostile.contains('\u{1b}'), "{hostile:?}");
    }

    #[test]
    fn what_the_user_approves_is_the_detail_with_the_warning_first() {
        let preview = ApprovalPreview {
            summary: "{\"path\": \"a\"}".into(),
            detail: Some("--- a/a\n+++ b/a\r\n@@".into()),
            lexer: Some("diff"),
            warning: None,
        };
        let text = approval_text(&preview);
        assert!(text.starts_with("--- a/a"));
        assert!(
            !text.contains('\r'),
            "controls are shown, not sent: {text:?}"
        );

        let warned = ApprovalPreview {
            warning: Some("changes what agent86 runs".into()),
            detail: None,
            ..preview
        };
        let text = approval_text(&warned);
        assert!(text.starts_with("Warning: changes what agent86 runs\n\n"));
        assert!(
            text.ends_with("{\"path\": \"a\"}"),
            "falls back to the summary"
        );
    }

    #[test]
    fn a_result_shows_the_observation_or_else_the_error() {
        let failed = ToolResult {
            call_id: "c1".into(),
            name: "x".into(),
            ok: false,
            content: String::new(),
            error: Some("denied".into()),
            metadata: Default::default(),
        };
        assert_eq!(result_text(&failed, "saw this"), "saw this");
        assert_eq!(result_text(&failed, "  "), "denied");
        let empty = ToolResult {
            error: None,
            ..failed
        };
        assert_eq!(result_text(&empty, ""), "(no output)");
    }
}
