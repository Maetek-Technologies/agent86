//! Embedding agent86 in another program (beyond Python; a prototype, see
//! `docs/adr/0003-embeddable-agent-api.md`).
//!
//! [`Agent::builder`] assembles the same [`Harness`] the CLI, the interface, and
//! the ACP server run, from parts the caller hands it, with no config file,
//! environment variable, home directory, keyring, or terminal involved:
//!
//! ```no_run
//! use agent86::sdk::{Agent, Builtins, FnTool};
//! use agent86::types::ApprovalMode;
//! use serde_json::json;
//! # fn provider() -> std::sync::Arc<dyn agent86::provider::Provider> { unimplemented!() }
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let weather = FnTool::new(
//!     "get_weather",
//!     "The current weather in a city.",
//!     json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
//!     |args, _ctx| Ok(format!("Sunny in {}", args["city"].as_str().unwrap_or("?"))),
//! );
//! let mut agent = Agent::builder()
//!     .provider(provider())
//!     .workspace("./project")
//!     .builtins(Builtins::ReadOnly)
//!     .tool(weather)
//!     .approval(ApprovalMode::Ask)
//!     .approver(|tool, _preview| tool != "write_file")
//!     .build()?;
//! let reply = agent.run("What's the weather in Oslo?").await?;
//! println!("{}", reply.answer);
//! # Ok(()) }
//! ```
//!
//! What it keeps from the harness, unchanged: the ReAct loop and its circuit
//! breaker, the workspace jail, the approval gate (side-effecting calls with
//! nobody to ask are declined), the guardrails, the secret vault's redaction,
//! parallel read-only tool calls, context fitting and compaction, and the
//! [`TurnEvent`] stream. On request it adds MCP servers ([`McpServer`], values
//! taken literally, never expanded from the environment or the keyring) and
//! long-term memory ([`Memory`]: sessions saved after every step, past turns
//! recalled into similar later ones, `remember` and `recall`). What it leaves out
//! for now: skills, sub-agents, triage routing, and OpenTelemetry, each of which
//! the CLI builds from config in [`crate::setup`].
//!
//! This module is the intended public surface; the rest of the crate is public
//! for its own tests and binaries and makes no stability promise.

use crate::agent::{CancelToken, Harness, HarnessError, TurnEvent};
use crate::approval::{ApprovalGate, ApprovalPreview};
use crate::config::{
    builtin_config, GuardrailsConfig, LimitsConfig, McpServerConfig, ObservabilityConfig,
};
use crate::guardrails::Guardrails;
use crate::mcp::{var_ref_names, McpManager};
use crate::memory::embeddings::Embedder;
use crate::observability::Recorder;
use crate::provider::Provider;
use crate::secrets::NoSecretStore;
use crate::session::{AgentState, SessionStore};
use crate::tools::sandbox::Executor;
use crate::tools::{exec, files, search, web, Tool, ToolContext, ToolError, ToolRegistry};
use crate::turn::{final_answer, TurnSummary};
use crate::types::{ApprovalMode, Message, Role, ToolResult, Usage};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Which of agent86's own tools an [`Agent`] offers, each set including the one
/// before it. Every one of them is jailed to the workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Builtins {
    /// None: only the tools given with [`AgentBuilder::tool`].
    #[default]
    None,
    /// `read_file`, `list_dir`, `search_files`, and `find_files`.
    ReadOnly,
    /// Also `write_file` and `edit_file`, which go through the approval gate.
    Files,
    /// Also `run_command` and `python_exec` (under the sandbox executor) and
    /// `web_fetch` (guarded against SSRF), all through the approval gate.
    All,
}

/// Why [`AgentBuilder::build`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("no provider was given; call AgentBuilder::provider")]
    MissingProvider,
    #[error("no workspace was given; call AgentBuilder::workspace")]
    MissingWorkspace,
    #[error("could not open the workspace {path}: {detail}")]
    Workspace { path: String, detail: String },
    #[error("could not register a tool: {0}")]
    Tool(String),
    #[error("could not open the trace directory: {0}")]
    Trace(String),
    #[error("MCP server '{name}' failed to start: {reason}")]
    Mcp { name: String, reason: String },
    #[error("a session to resume needs memory; call AgentBuilder::memory")]
    SessionWithoutMemory,
    #[error("no session '{0}' in memory")]
    SessionNotFound(String),
    #[error("memory could not be read: {0}")]
    Memory(String),
}

/// Decides a side-effecting call: given the tool's name and what the call would
/// do, `true` lets it run.
type Approver = Box<dyn Fn(&str, &ApprovalPreview) -> bool + Send + Sync>;

/// Builds an [`Agent`]; start from [`Agent::builder`].
pub struct AgentBuilder {
    provider: Option<Arc<dyn Provider>>,
    workspace: Option<PathBuf>,
    system: Option<String>,
    builtins: Builtins,
    tools: Vec<Box<dyn Tool>>,
    approval: ApprovalMode,
    approver: Option<Approver>,
    limits: LimitsConfig,
    guardrails: GuardrailsConfig,
    context_window: u64,
    max_output_tokens: u32,
    trace_dir: Option<PathBuf>,
    tool_timeout: Duration,
    mcp_servers: Vec<(String, McpServer)>,
    memory: Option<Memory>,
    session: Option<String>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        let config = builtin_config();
        Self {
            provider: None,
            workspace: None,
            system: None,
            builtins: Builtins::None,
            tools: Vec::new(),
            approval: ApprovalMode::Ask,
            approver: None,
            max_output_tokens: config.limits.max_output_tokens,
            tool_timeout: Duration::from_secs(config.limits.tool_timeout_s),
            limits: config.limits,
            guardrails: config.guardrails,
            context_window: DEFAULT_CONTEXT_WINDOW,
            trace_dir: None,
            mcp_servers: Vec::new(),
            memory: None,
            session: None,
        }
    }
}

/// The window assumed when the caller names none: small enough that a turn is
/// compacted before most models would refuse it.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

impl AgentBuilder {
    /// The model. Build one with [`crate::provider`]'s adapters or implement
    /// [`Provider`] yourself (only `stream` is required).
    pub fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// The directory the tools are jailed to. Required, so an embedded agent
    /// never silently works on the process's current directory.
    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    /// The system prompt; agent86's own identity ([`crate::prompt::BASE_IDENTITY`])
    /// when not set.
    pub fn system_prompt(mut self, text: impl Into<String>) -> Self {
        self.system = Some(text.into());
        self
    }

    /// Which of agent86's tools to offer ([`Builtins::None`] when not set).
    pub fn builtins(mut self, builtins: Builtins) -> Self {
        self.builtins = builtins;
        self
    }

    /// Offer `tool` too. A name already taken fails [`AgentBuilder::build`].
    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }

    /// The approval mode for side-effecting calls: `auto` runs them, `deny`
    /// refuses them, and `ask` (the default) asks the [`AgentBuilder::approver`],
    /// declining every one when there is none.
    pub fn approval(mut self, mode: ApprovalMode) -> Self {
        self.approval = mode;
        self
    }

    /// Who is asked in `ask` mode: given the tool's name and what the call would do
    /// (its arguments, and a diff for a file write), `true` lets it run. Called on
    /// the turn's thread, one question at a time.
    pub fn approver(
        mut self,
        approver: impl Fn(&str, &ApprovalPreview) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.approver = Some(Box::new(approver));
        self
    }

    /// Model calls one turn may make before it stops (40 by default).
    pub fn max_steps(mut self, steps: u32) -> Self {
        self.limits.max_steps = steps;
        self
    }

    /// The spend, in US dollars, at which a turn stops; `0` for no cap (5 by
    /// default). Only priced models count toward it.
    pub fn max_cost_usd(mut self, usd: f64) -> Self {
        self.limits.max_cost_usd = usd;
        self
    }

    /// Seconds one turn may run; `0` for no limit (900 by default).
    pub fn max_wall_clock_s(mut self, seconds: u64) -> Self {
        self.limits.max_wall_clock_s = seconds;
        self
    }

    /// Seconds a command or fetch may run (60 by default).
    pub fn tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = timeout;
        self
    }

    /// The tokens of context the model holds, which history is fitted to.
    pub fn context_window(mut self, tokens: u64) -> Self {
        self.context_window = tokens;
        self
    }

    /// The most tokens one model call may write (8192 by default).
    pub fn max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    /// Ingress, egress, and observation scanning (agent86's defaults when not set:
    /// warn, warn, on). Its `approval` is ignored; use [`AgentBuilder::approval`].
    pub fn guardrails(mut self, guardrails: GuardrailsConfig) -> Self {
        self.guardrails = guardrails;
        self
    }

    /// Write the flight recorder's `trace.jsonl` in `dir`, redacted as the CLI's
    /// is. Off when not set.
    pub fn trace_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.trace_dir = Some(dir.into());
        self
    }

    /// Start the MCP server `server` with the agent and offer its tools, named
    /// `mcp__<name>__<tool>` and asked about unless the server marks them
    /// read-only. A server that fails to start fails [`AgentBuilder::build`]; the
    /// servers stop when the agent is dropped.
    pub fn mcp_server(mut self, name: impl Into<String>, server: McpServer) -> Self {
        self.mcp_servers.push((name.into(), server));
        self
    }

    /// Give the agent long-term memory: the session is saved after every step and
    /// at the end of each turn, finished turns are recalled into similar later
    /// ones, and the model gets `remember` and `recall`. One [`Memory`] can be
    /// shared by several agents.
    pub fn memory(mut self, memory: Memory) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Continue the session `id` from [`AgentBuilder::memory`] (any session saved
    /// there, by this API, `agent86`, or Python's agent86).
    pub fn session(mut self, id: impl Into<String>) -> Self {
        self.session = Some(id.into());
        self
    }

    /// Assemble the agent. Reads nothing but the workspace path, the memory
    /// database when one is given, and starts the MCP servers given.
    pub fn build(self) -> Result<Agent, BuildError> {
        let provider = self.provider.ok_or(BuildError::MissingProvider)?;
        let workspace = self.workspace.ok_or(BuildError::MissingWorkspace)?;
        let mut context = ToolContext::new(&workspace).map_err(|error| BuildError::Workspace {
            path: workspace.display().to_string(),
            detail: error.to_string(),
        })?;
        context.policy.timeout = self.tool_timeout;
        context.executor = Executor::Subprocess;

        let registry = registry_for(self.builtins);
        for tool in self.tools {
            registry.register(tool).map_err(BuildError::Tool)?;
        }

        let store = self.memory.as_ref().map(|memory| memory.store.clone());
        let state = match (&self.session, &store) {
            (None, _) => AgentState::new(),
            (Some(_), None) => return Err(BuildError::SessionWithoutMemory),
            (Some(id), Some(store)) => store
                .load(id)
                .map_err(|error| BuildError::Memory(error.0))?
                .ok_or_else(|| BuildError::SessionNotFound(id.clone()))?,
        };
        if let Some(memory) = &self.memory {
            let (episodes, sessions, days) = memory.retention;
            store
                .as_ref()
                .expect("memory gives a store")
                .enforce_retention(episodes, sessions, days)
                .map_err(|error| BuildError::Memory(error.0))?;
            for tool in [
                Box::new(crate::tools::memory::RememberTool) as Box<dyn Tool>,
                Box::new(crate::tools::memory::RecallTool),
            ] {
                registry.register(tool).map_err(BuildError::Tool)?;
            }
        }
        context.memory = store.clone();

        let mcp = start_mcp(&self.mcp_servers, &registry)?;

        let recorder = match &self.trace_dir {
            None => Recorder::disabled(),
            Some(dir) => {
                std::fs::create_dir_all(dir)
                    .map_err(|error| BuildError::Trace(error.to_string()))?;
                let defaults = builtin_config().observability;
                Recorder::from_config(&ObservabilityConfig {
                    trace: true,
                    path: dir.display().to_string(),
                    otel: false,
                    ..defaults
                })
            }
        }
        .with_secrets(context.secrets.clone());

        let mut limits = self.limits;
        limits.max_output_tokens = self.max_output_tokens;
        let config = builtin_config();
        let harness = Harness {
            provider,
            router: Default::default(),
            tracer: Default::default(),
            registry,
            context,
            limits,
            agents: crate::config::AgentsConfig {
                enabled: false,
                ..config.agents
            },
            system: Message {
                role: Role::System,
                content: self
                    .system
                    .unwrap_or_else(|| crate::prompt::BASE_IDENTITY.trim().to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            },
            max_tokens: self.max_output_tokens,
            session_limits: Default::default(),
            context_window: self.context_window,
            gate: ApprovalGate::new(self.approval, self.approver),
            guardrails: Guardrails::new(&self.guardrails),
            recorder,
            mcp,
            memory: store,
            cancel: Default::default(),
            subagents: Default::default(),
        };
        harness.gate.queue.watch(harness.cancel.clone());
        harness
            .context
            .secrets
            .set_queue(harness.gate.queue.clone());
        Ok(Agent { harness, state })
    }
}

/// Start `servers` on a manager attached to `registry`, failing on the first that
/// does not start or whose tools collide; dropping the manager stops the rest.
fn start_mcp(
    servers: &[(String, McpServer)],
    registry: &ToolRegistry,
) -> Result<Option<Arc<McpManager>>, BuildError> {
    if servers.is_empty() {
        return Ok(None);
    }
    let manager = Arc::new(McpManager::empty(&builtin_config()));
    manager.attach(registry);
    let mut seen: Vec<&str> = Vec::new();
    for (name, server) in servers {
        let failed = |reason: String| BuildError::Mcp {
            name: name.clone(),
            reason,
        };
        if seen.contains(&name.as_str()) {
            return Err(failed("another server was given the same name".to_owned()));
        }
        seen.push(name);
        let mut config = server.config.clone();
        config.resolve_transport().map_err(failed)?;
        manager
            .start_server(
                name,
                &config,
                &server.literal_refs(),
                &NoSecretStore,
                config.connect_timeout_s,
            )
            .map_err(failed)?;
        let (_, collisions) = manager.mount(name);
        if !collisions.is_empty() {
            return Err(BuildError::Tool(format!(
                "MCP server '{name}' offers tools already taken: {}",
                collisions.join(", ")
            )));
        }
    }
    Ok(Some(manager))
}

/// An MCP server for [`AgentBuilder::mcp_server`]. Every value (arguments,
/// environment, URL, headers) is used exactly as given: a `${VAR}` in one is
/// never expanded from the process's environment or the keyring, as it would be
/// in a config file, so pass resolved values.
#[derive(Debug, Clone)]
pub struct McpServer {
    config: McpServerConfig,
}

impl McpServer {
    fn from(value: Value) -> Self {
        Self {
            config: serde_json::from_value(value).expect("a server config always deserializes"),
        }
    }

    /// A server agent86 starts as `command` with `args`, speaking MCP on its
    /// standard streams. It gets agent86's scrubbed environment plus
    /// [`McpServer::env`], never the whole of this process's.
    pub fn stdio<S: Into<String>>(
        command: impl Into<String>,
        args: impl IntoIterator<Item = S>,
    ) -> Self {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        Self::from(json!({"command": command.into(), "args": args, "transport": "stdio"}))
    }

    /// A server at `url` over streamable HTTP.
    pub fn http(url: impl Into<String>) -> Self {
        Self::from(json!({"url": url.into(), "transport": "http"}))
    }

    /// A server at `url` over the older SSE transport.
    pub fn sse(url: impl Into<String>) -> Self {
        Self::from(json!({"url": url.into(), "transport": "sse"}))
    }

    /// Set an environment variable for a stdio server.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.insert(name.into(), value.into());
        self
    }

    /// Send a header with every request to a remote server.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.insert(name.into(), value.into());
        self
    }

    /// How long the server has to start and list its tools (30 seconds by default).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.connect_timeout_s = timeout.as_secs_f64();
        self
    }

    /// How long one tool call may take (120 seconds by default).
    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.config.call_timeout_s = timeout.as_secs_f64();
        self
    }

    /// The most characters of a result the model is shown (8000 by default).
    pub fn max_result_chars(mut self, chars: usize) -> Self {
        self.config.max_result_chars = chars;
        self
    }

    /// Every `${VAR}` in the values mapped to itself, so the manager's expansion
    /// leaves it as written (overrides win over the environment and the keyring).
    fn literal_refs(&self) -> HashMap<String, String> {
        let config = &self.config;
        config
            .args
            .iter()
            .chain(config.env.values())
            .chain(config.url.iter())
            .chain(config.headers.values())
            .flat_map(|text| var_ref_names(text))
            .map(|name| (name.clone(), format!("${{{name}}}")))
            .collect()
    }
}

/// Long-term memory for [`AgentBuilder::memory`]: agent86's SQLite memory
/// database (the same file and schema the CLI uses, so its sessions and facts are
/// shared), opened once and cheap to clone.
#[derive(Clone)]
pub struct Memory {
    store: SessionStore,
    retention: (i64, i64, f64),
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl Memory {
    /// Open (creating if needed) the database at `path`, embedding with the
    /// offline hashing embedder (`hash:256`), which needs no model download.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BuildError> {
        Self::wrap(SessionStore::open(path.as_ref()))
    }

    /// [`Memory::open`] with another embedder, such as the sentence-transformers
    /// model `crate::memory::embeddings::build_embedder` loads. Episodes and facts
    /// written with one embedder are only found again with the same one.
    pub fn open_with(
        path: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self, BuildError> {
        Self::wrap(SessionStore::open_with(path.as_ref(), embedder))
    }

    /// A database that lives only as long as this value and its clones.
    pub fn in_memory() -> Result<Self, BuildError> {
        Self::open(":memory:")
    }

    fn wrap(store: Result<SessionStore, crate::session::StoreError>) -> Result<Self, BuildError> {
        let memory = builtin_config().memory;
        Ok(Self {
            store: store.map_err(|error| BuildError::Memory(error.0))?,
            retention: (
                memory.retention_max_episodes,
                memory.retention_max_sessions,
                memory.retention_max_age_days,
            ),
        })
    }

    /// Keep at most `episodes` episodes and `sessions` sessions, and none older
    /// than `max_age_days` (`0` for no age limit), pruned when an agent is built
    /// (1000, 500, and no age limit by default). Pinned entries are kept.
    pub fn retention(mut self, episodes: i64, sessions: i64, max_age_days: f64) -> Self {
        self.retention = (episodes, sessions, max_age_days);
        self
    }

    /// The sessions saved here, newest first: (id, title).
    pub fn sessions(&self, limit: usize) -> Result<Vec<(String, Option<String>)>, BuildError> {
        Ok(self
            .store
            .recent_sessions(limit)
            .map_err(|error| BuildError::Memory(error.0))?
            .into_iter()
            .map(|session| (session.session_id, session.title))
            .collect())
    }

    /// The store underneath, for what this type does not cover yet (facts,
    /// episodes, pins). Its API makes no stability promise.
    pub fn store(&self) -> &SessionStore {
        &self.store
    }
}

fn registry_for(builtins: Builtins) -> ToolRegistry {
    let registry = ToolRegistry::new();
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    if builtins != Builtins::None {
        tools.push(Box::new(files::ReadFileTool));
        tools.push(Box::new(files::ListDirTool));
        tools.push(Box::new(search::SearchFilesTool));
        tools.push(Box::new(search::FindFilesTool));
    }
    if matches!(builtins, Builtins::Files | Builtins::All) {
        tools.push(Box::new(files::WriteFileTool));
        tools.push(Box::new(files::EditFileTool));
    }
    if builtins == Builtins::All {
        tools.push(Box::new(exec::RunCommandTool));
        tools.push(Box::new(exec::PythonExecTool));
        tools.push(Box::new(web::WebFetchTool));
    }
    for tool in tools {
        registry
            .register(tool)
            .expect("built-in tool names are unique");
    }
    registry
}

/// What one [`Agent::run`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    /// The turn's answer: the model's last message when it made no tool call,
    /// otherwise empty (a turn stopped by a limit or a cancel).
    pub answer: String,
    /// Everything the turn streamed, notices and `[tool] ...` lines included, as
    /// `agent86 run --output full` prints it.
    pub output: String,
    /// The turn's tokens, cost, steps, and tool calls.
    pub turn: Option<TurnSummary>,
    /// The conversation's usage so far.
    pub usage: Usage,
}

/// An agent: a harness and its conversation. Each [`Agent::run`] is one turn
/// that continues the conversation.
pub struct Agent {
    harness: Harness,
    state: AgentState,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Run one turn toward `goal`.
    pub async fn run(&mut self, goal: &str) -> Result<RunResult, HarnessError> {
        self.run_with_events(goal, |_| {}).await
    }

    /// [`Agent::run`], reporting each [`TurnEvent`] (text, reasoning, tool calls
    /// and their results, usage) as it happens.
    pub async fn run_with_events(
        &mut self,
        goal: &str,
        mut on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<RunResult, HarnessError> {
        let output = self
            .harness
            .run_turn_events(&mut self.state, goal, goal, &mut on_event)
            .await;
        // Saved after a failed turn too, as the CLI does; a failed write loses this
        // save, never the turn (the harness saved after every step already).
        if let Some(store) = &self.harness.memory {
            let _ = store.persist(&self.state);
        }
        let output = output?;
        Ok(RunResult {
            answer: final_answer(&self.state.messages).to_owned(),
            output,
            turn: self.state.last_turn.clone(),
            usage: self.state.usage,
        })
    }

    /// A handle that stops the running turn at its next safe point (before a model
    /// call, while streaming, or between tool calls), from any thread.
    pub fn cancel_handle(&self) -> CancelToken {
        self.harness.cancel.clone()
    }

    /// The conversation: messages, steps, usage. It serializes to the session
    /// database's `state_json`, so it can be stored and given back with
    /// [`Agent::with_state`].
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Continue an earlier conversation instead of starting a new one.
    pub fn with_state(mut self, state: AgentState) -> Self {
        self.state = state;
        self
    }

    /// The session's id, under which [`AgentBuilder::memory`] saves it.
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    /// What the MCP servers reported (a server that dropped, reconnected, or
    /// changed its tools), oldest first.
    pub fn notes(&self) -> Vec<String> {
        self.harness
            .mcp
            .as_ref()
            .map(|mcp| mcp.notes())
            .unwrap_or_default()
    }

    /// The names of the tools the model is offered.
    pub fn tool_names(&self) -> Vec<String> {
        self.harness.registry.names()
    }

    /// The harness underneath, for what this module does not cover yet. Its API
    /// makes no stability promise.
    pub fn harness(&self) -> &Harness {
        &self.harness
    }
}

/// The function behind an [`FnTool`]: given the validated-as-JSON arguments and
/// the tool context (its workspace jail, `ctx.policy.resolve_within`), the text
/// the model sees, or an error message.
type ToolFn = dyn Fn(&Map<String, Value>, &ToolContext) -> Result<String, String> + Send + Sync;

/// A tool from a closure, for the common case where implementing [`Tool`] is more
/// than needed. Read-only unless [`FnTool::side_effecting`] says otherwise.
pub struct FnTool {
    name: String,
    description: String,
    parameters: Value,
    side_effecting: bool,
    run: Box<ToolFn>,
}

impl FnTool {
    /// A tool named `name` (letters, digits, `_`, `-`), described to the model by
    /// `description`, taking arguments matching the JSON Schema `parameters`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        run: impl Fn(&Map<String, Value>, &ToolContext) -> Result<String, String>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            side_effecting: false,
            run: Box::new(run),
        }
    }

    /// Mark the tool as having effects outside the conversation, so every call goes
    /// through the approval gate.
    pub fn side_effecting(mut self) -> Self {
        self.side_effecting = true;
        self
    }
}

impl Tool for FnTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn side_effecting(&self) -> bool {
        self.side_effecting
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        match (self.run)(args, ctx) {
            Ok(content) => Ok(ToolResult {
                call_id: String::new(),
                name: self.name.clone(),
                ok: true,
                content,
                error: None,
                metadata: Map::new(),
            }),
            Err(message) => Err(ToolError::Raised {
                exception: "ToolError".to_owned(),
                message,
            }),
        }
    }
}
