//! Embedding agent86 in another program (beyond Python; see
//! `docs/adr/0003-embeddable-agent-api.md`).
//!
//! [`Agent::builder`] assembles the same [`Harness`] the CLI, the interface, and
//! the ACP server run, from parts the caller hands it, with no config file,
//! environment variable, home directory, keyring, or terminal involved:
//!
//! ```no_run
//! use agent86_core::sdk::{Agent, ApprovalMode, Builtins, FnTool};
//! use serde_json::json;
//! # fn provider() -> std::sync::Arc<dyn agent86_core::sdk::Provider> { unimplemented!() }
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
//! [`TurnEvent`] stream. On request it adds:
//!
//! - MCP servers ([`McpServer`]), values taken literally, never expanded from the
//!   environment or the keyring;
//! - long-term memory ([`Memory`]): sessions saved after every step, past turns
//!   recalled into similar later ones, `remember` and `recall`;
//! - skills from the directories named ([`AgentBuilder::skills_dir`]), offered to
//!   the model through `use_skill` and run directly with [`Turn::skill`];
//! - sub-agents ([`SubAgents`]): `delegate`, agent86's built-in specialists and
//!   the caller's own, in parallel and in git worktrees, each narrowing its
//!   caller's authority, and run directly with [`Turn::specialist`];
//! - named models ([`Model`], [`AgentBuilder::model`]) for specialists' `model:`
//!   lines and for triage routing ([`AgentBuilder::triage`]);
//! - OpenTelemetry spans ([`Otel`]), with agent86's span and attribute names;
//! - questions a turn asks mid-way: whether to raise a limit it reached
//!   ([`AgentBuilder::on_limit`]) and a secret the model needs
//!   ([`AgentBuilder::on_secret_request`]), plus secrets the program already holds
//!   ([`AgentBuilder::secret`]), which tools get as environment variables and the
//!   model only ever sees as `${secret:NAME}`.
//!
//! A conversation is a [`Conversation`], which saves to and loads from the JSON
//! the session database keeps.
//!
//! Nothing is discovered: no `.agent86/` or `~/.agent86/` directory is read, and
//! no `OTEL_*` variable either. Each part is what the caller passes in.
//!
//! This module is the crate's public surface, the one semantic versioning
//! covers; the rest of the crate is public for the `agent86` binary and makes no
//! stability promise.

use crate::agent::Harness;
use crate::agents_def::Specialties;
use crate::approval::ApprovalGate;
use crate::config::{builtin_config, LimitsConfig, McpServerConfig, ObservabilityConfig};
use crate::guardrails::Guardrails;
use crate::mcp::{var_ref_names, McpManager};
use crate::observability::Recorder;
use crate::router::{RoutedModel, Router};
use crate::secrets::NoSecretStore;
use crate::session::{AgentState, SessionStore};
use crate::specialty_turn::TurnKind;
use crate::tools::sandbox::Executor;
use crate::tools::{exec, files, search, web, ToolRegistry};
use crate::turn::final_answer;
use crate::types::Message;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Every type this module's signatures use, re-exported here so a user names no
// other module of the crate: those are public for the `agent86` binary only, are
// hidden from the docs, and carry no stability promise.
pub use crate::agent::{CancelToken, HarnessError, TurnEvent};
#[cfg(feature = "agent86ui")]
pub use crate::agent86ui::UiWait;
pub use crate::approval::ApprovalPreview;
pub use crate::config::{EgressMode, GuardrailsConfig, IngressMode};
pub use crate::limits::{LimitHit, LimitKind};
pub use crate::memory::embeddings::Embedder;
pub use crate::tools::{Tool, ToolContext, ToolError};
pub use crate::turn::TurnSummary;
pub use crate::types::{ApprovalMode, Role, ToolCall, ToolResult, ToolSpec, Usage};
pub use crate::vault::SecretRequest;
/// agent86UI's protocol layer (ADR 0005): A2UI 1.0 messages, catalogs, the
/// validator, and the headless surface runtime a renderer can build on.
#[cfg(feature = "agent86ui")]
pub use agent86_ui;
#[cfg(feature = "agent86ui")]
pub use agent86_ui::{Action, AgentMessage, AgentMessageKind, Catalog};
pub use providers::Provider;

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
///
/// `#[non_exhaustive]` (since 1.2.0): later releases may add reasons, so a
/// `match` outside this crate needs a `_` arm; the `Display` text describes any.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
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
    #[error("no model named '{0}'; add it with AgentBuilder::model")]
    UnknownModel(String),
    /// A secret given to [`AgentBuilder::secret`] was refused. The message names
    /// the secret and why, never its value.
    #[error("the secret {name} was refused: {reason}")]
    Secret { name: String, reason: String },
    /// [`AgentBuilder::ui`] was given no catalogs, or one that does not compile.
    #[cfg(feature = "agent86ui")]
    #[error("agent86UI could not be set up: {0}")]
    Ui(String),
}

/// Why [`Agent::run`] failed.
///
/// `#[non_exhaustive]` (since 1.2.0): later releases may add reasons, so a
/// `match` outside this crate needs a `_` arm; the `Display` text describes any.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// The turn failed: the model could not be reached, or a limit tripped.
    #[error(transparent)]
    Harness(#[from] HarnessError),
    /// [`Turn::skill`] named a skill the agent does not have.
    #[error("no skill named '{0}'")]
    UnknownSkill(String),
    /// [`Turn::specialist`] named a specialist the agent does not have, or the
    /// agent has no [`SubAgents`].
    #[error("no specialist named '{0}'")]
    UnknownSpecialist(String),
    /// [`Agent::ui_action`] was given an action no surface up could have sent
    /// (no renderer, no such surface or component, or no name). The message
    /// says which, never the action's values.
    #[cfg(feature = "agent86ui")]
    #[error("the UI action was refused: {0}")]
    UiAction(String),
}

/// Decides a side-effecting call: given the tool's name and what the call would
/// do, `true` lets it run.
type Approver = Box<dyn Fn(&str, &ApprovalPreview) -> bool + Send + Sync>;

/// Answers [`AgentBuilder::on_secret_request`]: the value, or `None` to decline.
type SecretAsker = Box<dyn Fn(&SecretRequest) -> Option<String> + Send + Sync>;

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
    skill_dirs: Vec<PathBuf>,
    subagents: Option<SubAgents>,
    models: IndexMap<String, Model>,
    triage: Option<(String, String)>,
    otel: Option<Otel>,
    on_limit: Option<crate::limits::LimitPrompt>,
    on_secret: Option<SecretAsker>,
    #[cfg(feature = "agent86ui")]
    ui: Option<Ui>,
    /// Never printed: the builder has no `Debug`, and errors name only the secret.
    secrets: Vec<(String, String)>,
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
            skill_dirs: Vec::new(),
            subagents: None,
            models: IndexMap::new(),
            triage: None,
            otel: None,
            on_limit: None,
            on_secret: None,
            #[cfg(feature = "agent86ui")]
            ui: None,
            secrets: Vec::new(),
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

    /// Offer the skills in `dir`, a directory of `<name>/SKILL.md` folders, in
    /// agent86's (and Claude Code's) skill format. The model sees their names and
    /// descriptions and loads one with `use_skill`, whose `allowed-tools` then
    /// limit it; [`Turn::skill`] runs one directly. The folders join the jail for
    /// reading, since skills bundle files. Called again, an earlier directory wins a
    /// name both hold.
    pub fn skills_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skill_dirs.push(dir.into());
        self
    }

    /// Let the model delegate to sub-agents (see [`SubAgents`]).
    pub fn subagents(mut self, subagents: SubAgents) -> Self {
        self.subagents = Some(subagents);
        self
    }

    /// Name a model, for a specialist whose definition says `model: <name>` and for
    /// [`AgentBuilder::triage`]. A specialist naming a model that is not here runs
    /// on the turn's model, with a notice.
    pub fn model(mut self, name: impl Into<String>, model: Model) -> Self {
        self.models.insert(name.into(), model);
        self
    }

    /// Route each turn by its task, as agent86's `[model] router = "triage"` does:
    /// a short, plain ask goes to the model named `cheap`, anything else to
    /// `frontier` (both named with [`AgentBuilder::model`]); compaction summaries
    /// use `cheap`. The [`AgentBuilder::provider`] is still required, and serves
    /// nothing while routing is on.
    pub fn triage(mut self, cheap: impl Into<String>, frontier: impl Into<String>) -> Self {
        self.triage = Some((cheap.into(), frontier.into()));
        self
    }

    /// Export OpenTelemetry spans: a `turn` span per turn, parent of a
    /// `model_call` per model call and a `tool_call` per tool call, with the GenAI
    /// attribute names agent86 uses. Exported on a worker thread, flushed at the end
    /// of each turn and when the agent is dropped; a collector that cannot be
    /// reached becomes a note ([`Agent::notes`]), never an error.
    pub fn otel(mut self, otel: Otel) -> Self {
        self.otel = Some(otel);
        self
    }

    /// Asked when a turn reaches a limit (its step budget, a sub-agent's, or the
    /// output cap before any text): given what was reached and the values offered,
    /// return the new limit to go on with, or `None` to stop there as without this
    /// callback. A raise lasts for the agent's life, or for one specialist's run
    /// when [`LimitHit::run_only`] says so. Called on the turn's thread, one
    /// question at a time with approvals.
    pub fn on_limit(
        mut self,
        decide: impl Fn(&LimitHit) -> Option<u32> + Send + Sync + 'static,
    ) -> Self {
        self.on_limit = Some(Box::new(decide));
        self
    }

    /// Offer the model `request_secret`, and ask `answer` when it is called: given
    /// the variable's name and the model's reason, return the value (held for the
    /// agent's life, at least 6 characters) or `None` to decline. Tools that run
    /// commands get it as that environment variable, and every path back to the
    /// model, the conversation, and the trace shows only `${secret:NAME}`. Names
    /// that would change how programs run (`PATH`, `LD_PRELOAD`, ...) are refused
    /// before `answer` is asked. Nothing is kept in a keyring.
    pub fn on_secret_request(
        mut self,
        answer: impl Fn(&SecretRequest) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.on_secret = Some(Box::new(answer));
        self
    }

    /// Declare a renderer (ADR 0005): offer the model `render_ui`, through which
    /// it draws interactive UI as A2UI 1.0 messages in the renderer's catalogs.
    /// Each validated message reaches [`Agent::run`]'s callback as
    /// [`TurnEvent::Ui`], for the program to draw with its own widgets (Google's
    /// React and Lit renderers take them as they are). The system prompt gains a
    /// section with the catalogs' instructions and components on every request;
    /// sub-agents never draw.
    #[cfg(feature = "agent86ui")]
    pub fn ui(mut self, ui: Ui) -> Self {
        self.ui = Some(ui);
        self
    }

    /// Hold a secret the program already has (an API token for a command the
    /// agent will run), with the same rules as [`AgentBuilder::on_secret_request`]:
    /// tools get it as the environment variable `name`, the model sees only
    /// `${secret:NAME}`, and `write_file` and `edit_file` may put it in a file the
    /// approver sees. `name` must be a valid variable name and the value at least 6
    /// characters, or [`AgentBuilder::build`] fails.
    pub fn secret(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.push((name.into(), value.into()));
        self
    }

    /// Assemble the agent. Reads nothing but the workspace path, the memory
    /// database, skill and specialist directories when given, and starts the MCP
    /// servers given.
    pub fn build(self) -> Result<Agent, BuildError> {
        let router = self.router()?;
        let provider = self.provider.ok_or(BuildError::MissingProvider)?;
        let workspace = self.workspace.ok_or(BuildError::MissingWorkspace)?;
        let mut context = ToolContext::new(&workspace).map_err(|error| BuildError::Workspace {
            path: workspace.display().to_string(),
            detail: error.to_string(),
        })?;
        context.policy.timeout = self.tool_timeout;
        context.executor = Executor::Subprocess;
        for (name, value) in &self.secrets {
            let refused = |reason: &str| BuildError::Secret {
                name: name.clone(),
                reason: reason.to_owned(),
            };
            if !crate::vault::valid_name(name) {
                return Err(refused("not a valid environment variable name"));
            }
            let answer = crate::vault::SecretAnswer {
                value: value.clone(),
                remember: false,
            };
            context.secrets.accept(name, answer).map_err(|_| {
                refused(&format!(
                    "shorter than {} characters",
                    crate::vault::MIN_SECRET_CHARS
                ))
            })?;
        }

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

        // Skills: only the directories named, highest precedence first, their
        // folders readable through the jail.
        let skills = Arc::new(crate::skills::discover_in(&self.skill_dirs));
        if !skills.is_empty() {
            context.policy.allow(
                self.skill_dirs
                    .iter()
                    .filter(|dir| dir.is_dir())
                    .filter_map(|dir| std::fs::canonicalize(dir).ok())
                    .collect::<Vec<_>>(),
            );
            registry
                .register(Box::new(crate::tools::skills::UseSkillTool))
                .map_err(BuildError::Tool)?;
        }
        context.skills = Arc::clone(&skills);

        // Sub-agents: `delegate`, and the specialists.
        let config = builtin_config();
        let mut agents = crate::config::AgentsConfig {
            enabled: false,
            ..config.agents.clone()
        };
        let mut notices = Vec::new();
        if let Some(subagents) = &self.subagents {
            agents = crate::config::AgentsConfig {
                enabled: true,
                max_depth: subagents.max_depth,
                max_steps: subagents.max_steps.min(self.limits.max_steps.max(1)),
                max_parallel: subagents
                    .max_parallel
                    .clamp(1, crate::config::MAX_PARALLEL_CEILING),
            };
            let mut specialties = if subagents.built_ins {
                Specialties::built_in()
            } else {
                Specialties::default()
            };
            for dir in &subagents.dirs {
                specialties.add_user_dir(dir);
            }
            notices.extend(specialties.notices.iter().cloned());
            context.specialties = Arc::new(specialties);
            registry
                .register(Box::new(crate::tools::delegate::DelegateTool))
                .map_err(BuildError::Tool)?;
        }

        let tracer = self
            .otel
            .as_ref()
            .map(|otel| crate::otel::Tracer::to(otel.destination.clone()))
            .unwrap_or_default();

        if let Some(answer) = self.on_secret {
            context
                .secrets
                .set_prompt(Some(Box::new(move |request: &SecretRequest| {
                    answer(request).map(|value| crate::vault::SecretAnswer {
                        value,
                        remember: false,
                    })
                })));
            registry
                .register(Box::new(crate::tools::secret::RequestSecretTool))
                .map_err(BuildError::Tool)?;
        }
        #[cfg(feature = "agent86ui")]
        let mut ui_session = None;
        #[cfg(feature = "agent86ui")]
        if let Some(ui) = self.ui {
            if ui.catalogs.is_empty() {
                return Err(BuildError::Ui(
                    "a renderer needs at least one catalog".into(),
                ));
            }
            ui_session = Some(
                crate::agent86ui::UiSession::new(ui.catalogs)
                    .map_err(|error| BuildError::Ui(error.to_string()))?
                    .with_egress(self.guardrails.egress)
                    .with_remote_media(ui.remote_media)
                    .with_waiter(ui.on_wait),
            );
        }
        let mut session_limits = crate::limits::SessionLimits::default();
        session_limits.prompt = self.on_limit;

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
        let mut system = self
            .system
            .unwrap_or_else(|| crate::prompt::BASE_IDENTITY.trim().to_owned());
        system.push_str(&crate::prompt::skills_section(
            skills.values().map(AsRef::as_ref),
        ));
        let mut harness = Harness {
            provider,
            router,
            tracer,
            registry,
            context,
            limits,
            agents,
            system: Message {
                role: Role::System,
                content: system,
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            },
            max_tokens: self.max_output_tokens,
            session_limits,
            context_window: self.context_window,
            gate: ApprovalGate::new(self.approval, self.approver),
            guardrails: Guardrails::new(&self.guardrails),
            recorder,
            mcp,
            memory: store,
            cancel: Default::default(),
            subagents: Default::default(),
        };
        #[cfg(feature = "agent86ui")]
        if let Some(session) = ui_session {
            harness.attach_ui(session).map_err(BuildError::Tool)?;
        }
        harness.context.can_delegate = harness.delegation_offered(&harness.context, 0);
        harness.gate.queue.watch(harness.cancel.clone());
        harness
            .context
            .secrets
            .set_queue(harness.gate.queue.clone());
        let agent = Agent {
            harness,
            conversation: Conversation(state),
            notices,
        };
        // A resumed session's surfaces come back from its history.
        #[cfg(feature = "agent86ui")]
        agent.replay_surfaces();
        Ok(agent)
    }

    /// The router for the named models: it builds nothing, it looks names up, so a
    /// specialist's `model:` resolves with or without triage.
    fn router(&self) -> Result<Router, BuildError> {
        if self.models.is_empty() {
            return match &self.triage {
                Some((cheap, _)) => Err(BuildError::UnknownModel(cheap.clone())),
                None => Ok(Router::default()),
            };
        }
        let (triage, cheap, frontier) = match &self.triage {
            Some((cheap, frontier)) => {
                for name in [cheap, frontier] {
                    if !self.models.contains_key(name) {
                        return Err(BuildError::UnknownModel(name.clone()));
                    }
                }
                (true, cheap.as_str(), frontier.as_str())
            }
            None => (false, "", ""),
        };
        let window = self.context_window;
        let output = self.max_output_tokens;
        let models: HashMap<String, RoutedModel> = self
            .models
            .iter()
            .map(|(name, model)| {
                (
                    name.clone(),
                    RoutedModel {
                        reference: name.clone(),
                        provider: Arc::clone(&model.provider),
                        max_tokens: model.max_output_tokens.unwrap_or(output),
                        context_window: model.context_window.unwrap_or(window),
                        output_setting: "max_output_tokens".to_owned(),
                    },
                )
            })
            .collect();
        Ok(Router::new(
            triage,
            cheap,
            frontier,
            Box::new(move |name| {
                models
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("no model named '{name}'"))
            }),
        ))
    }
}

/// Sub-agents for [`AgentBuilder::subagents`]: the model gets `delegate`, which
/// runs a sub-agent on a task with its own step budget and a fork of the caller's
/// tools, approvals, and jail. A *specialist* is a named sub-agent with its own
/// instructions, tool allowlist, approval mode, model, and skills, defined in a
/// Markdown file with frontmatter (agent86's `.agent86/agents/<name>.md` format;
/// see `docs/subagents.md`). A specialist's authority only narrows its caller's.
/// Adjacent specialist delegations run in parallel, each on its own thread, so a
/// custom [`Provider`] must implement `fresh`; one with `isolation: worktree` runs
/// in a git worktree and hands back its diff.
#[derive(Debug, Clone)]
pub struct SubAgents {
    max_depth: u32,
    max_steps: u32,
    max_parallel: u32,
    built_ins: bool,
    dirs: Vec<PathBuf>,
}

impl Default for SubAgents {
    fn default() -> Self {
        let agents = builtin_config().agents;
        Self {
            max_depth: agents.max_depth,
            max_steps: agents.max_steps,
            max_parallel: agents.max_parallel,
            built_ins: true,
            dirs: Vec::new(),
        }
    }
}

impl SubAgents {
    /// Delegation two levels deep, 8 steps per sub-agent, 3 in parallel, with
    /// agent86's built-in specialists (`researcher`, `reviewer`, `planner`).
    pub fn new() -> Self {
        Self::default()
    }

    /// How deep delegation may nest; the main agent is depth 0.
    pub fn max_depth(mut self, depth: u32) -> Self {
        self.max_depth = depth;
        self
    }

    /// Each sub-agent's step budget, kept at or below the turn's.
    pub fn max_steps(mut self, steps: u32) -> Self {
        self.max_steps = steps;
        self
    }

    /// How many adjacent specialist delegations run at once (1 to 8).
    pub fn max_parallel(mut self, parallel: u32) -> Self {
        self.max_parallel = parallel;
        self
    }

    /// Leave out agent86's built-in specialists.
    pub fn without_built_ins(mut self) -> Self {
        self.built_ins = false;
        self
    }

    /// Add the specialists defined in `dir` (`<name>.md` files), replacing a
    /// built-in or an earlier directory's of the same name. They are trusted as
    /// the caller's own, so never pass a directory from a repository the user did
    /// not write: its files become prompt text and tool scopes. A file that does
    /// not parse is skipped, and said so in [`Agent::notes`].
    pub fn specialists_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dirs.push(dir.into());
        self
    }
}

/// A model for [`AgentBuilder::model`].
#[derive(Clone)]
pub struct Model {
    provider: Arc<dyn Provider>,
    context_window: Option<u64>,
    max_output_tokens: Option<u32>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("provider", &self.provider.name())
            .field("model", &self.provider.model())
            .field("context_window", &self.context_window)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

impl Model {
    /// `provider`, with the agent's context window and output cap unless set.
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            context_window: None,
            max_output_tokens: None,
        }
    }

    /// The tokens of context this model holds.
    pub fn context_window(mut self, tokens: u64) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// The most tokens one call to this model may write.
    pub fn max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }
}

/// A renderer for agent86UI ([`AgentBuilder::ui`]): the A2UI catalogs it
/// draws, in order of preference.
#[cfg(feature = "agent86ui")]
#[derive(Clone)]
pub struct Ui {
    catalogs: Vec<Catalog>,
    remote_media: bool,
    on_wait: Option<crate::agent86ui::UiWaiter>,
}

#[cfg(feature = "agent86ui")]
impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui")
            .field(
                "catalogs",
                &self.catalogs.iter().map(Catalog::id).collect::<Vec<_>>(),
            )
            .field("remote_media", &self.remote_media)
            .field("on_wait", &self.on_wait.is_some())
            .finish()
    }
}

#[cfg(feature = "agent86ui")]
impl Ui {
    /// A renderer for the specification's basic catalog, which Google's
    /// renderers implement.
    pub fn basic() -> Self {
        Self::new([Catalog::basic().clone()])
    }

    /// A renderer for `catalogs` (load one with [`Catalog::from_value`]), in
    /// order of preference.
    pub fn new(catalogs: impl IntoIterator<Item = Catalog>) -> Self {
        Self {
            catalogs: catalogs.into_iter().collect(),
            remote_media: false,
            on_wait: None,
        }
    }

    /// Let the model wait for the user's answer (`render_ui` with `wait`), for
    /// a form in the middle of a task: once the call's [`TurnEvent::Ui`]
    /// messages are out, `answer` is asked for the first [`Action`] the user
    /// takes on one of [`UiWait::surface_ids`], or `None` when they dismiss it.
    /// Called on the turn's thread, one question at a time with approvals, and
    /// never once the turn is cancelled. The action's context becomes the call's
    /// result, through the vault and the ingress guardrail as typed input does.
    /// Without it, a waiting call is refused and the model asks without waiting.
    pub fn on_wait(
        mut self,
        answer: impl Fn(&UiWait) -> Option<Action> + Send + Sync + 'static,
    ) -> Self {
        self.on_wait = Some(std::sync::Arc::new(answer));
        self
    }

    /// Let `Image`, `Video`, and `AudioPlayer` load `https` URLs. Off by
    /// default: a surface's media is then a `data:` URL or a file in the
    /// workspace (a path relative to it, which the program serves), since a
    /// fetch can track the user or carry data out in its URL.
    pub fn allow_remote_media(mut self) -> Self {
        self.remote_media = true;
        self
    }

    /// The catalogs, in order of preference.
    pub fn catalogs(&self) -> &[Catalog] {
        &self.catalogs
    }
}

/// Where [`AgentBuilder::otel`] sends spans. OTLP is HTTP with JSON bodies.
#[derive(Debug, Clone)]
pub struct Otel {
    destination: crate::otel::Destination,
}

impl Otel {
    /// An OTLP/HTTP collector's traces endpoint, such as
    /// `http://localhost:4318/v1/traces`, with a 10-second timeout.
    pub fn otlp(traces_url: impl Into<String>) -> Self {
        Self {
            destination: crate::otel::Destination::Otlp {
                url: traces_url.into(),
                headers: Vec::new(),
                timeout: Duration::from_secs(10),
            },
        }
    }

    /// Each span as one line of JSON on stderr.
    pub fn console() -> Self {
        Self {
            destination: crate::otel::Destination::Console,
        }
    }

    /// Send `name: value` with every export (an API key, for a hosted collector).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let crate::otel::Destination::Otlp { headers, .. } = &mut self.destination {
            headers.push((name.into(), value.into()));
        }
        self
    }

    /// How long one export may take.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        if let crate::otel::Destination::Otlp { timeout: limit, .. } = &mut self.destination {
            *limit = timeout;
        }
        self
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
///
/// `#[non_exhaustive]` (since 1.2.0): later releases may add fields, so code
/// outside this crate reads the fields it needs and cannot build one or
/// destructure it without `..`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
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

/// One turn for [`Agent::run`]: a goal (a `&str` or `String` converts to one),
/// a skill run directly, or a specialist run directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    /// The model works toward the goal with every tool it is offered.
    Goal(String),
    /// The skill's instructions lead the user's message and the skill is active
    /// from the first step, so its `allowed-tools` apply at once.
    Skill { name: String, task: String },
    /// The specialist runs the task as a sub-agent at depth 1, and the exchange is
    /// kept as an ordinary user and assistant message. With `with_context`, the
    /// conversation so far is summarized and handed over.
    Specialist {
        name: String,
        task: String,
        with_context: bool,
    },
}

impl Turn {
    pub fn skill(name: impl Into<String>, task: impl Into<String>) -> Self {
        Self::Skill {
            name: name.into(),
            task: task.into(),
        }
    }

    pub fn specialist(name: impl Into<String>, task: impl Into<String>) -> Self {
        Self::Specialist {
            name: name.into(),
            task: task.into(),
            with_context: false,
        }
    }

    /// For a [`Turn::Specialist`], hand over a summary of the conversation so far.
    pub fn with_context(mut self) -> Self {
        if let Self::Specialist { with_context, .. } = &mut self {
            *with_context = true;
        }
        self
    }
}

impl From<&str> for Turn {
    fn from(goal: &str) -> Self {
        Self::Goal(goal.to_owned())
    }
}

impl From<String> for Turn {
    fn from(goal: String) -> Self {
        Self::Goal(goal)
    }
}

impl From<&String> for Turn {
    fn from(goal: &String) -> Self {
        Self::Goal(goal.clone())
    }
}

/// An agent: a harness and its conversation. Each [`Agent::run`] is one turn
/// that continues the conversation.
pub struct Agent {
    harness: Harness,
    conversation: Conversation,
    /// What building found worth saying (a specialist file skipped).
    notices: Vec<String>,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Run one turn: a goal, or a [`Turn`].
    pub async fn run(&mut self, turn: impl Into<Turn>) -> Result<RunResult, RunError> {
        self.run_with_events(turn, |_| {}).await
    }

    /// [`Agent::run`], reporting each [`TurnEvent`] (text, reasoning, tool calls
    /// and their results, usage) as it happens.
    pub async fn run_with_events(
        &mut self,
        turn: impl Into<Turn>,
        mut on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<RunResult, RunError> {
        let (kind, prompt, display) = match turn.into() {
            Turn::Goal(goal) => (TurnKind::Plain, goal.clone(), goal),
            Turn::Skill { name, task } => {
                let skill = self
                    .harness
                    .context
                    .skills
                    .get(&name)
                    .cloned()
                    .ok_or(RunError::UnknownSkill(name.clone()))?;
                let display = format!("/{name} {task}").trim_end().to_owned();
                (TurnKind::Skill(skill), task, display)
            }
            Turn::Specialist {
                name,
                task,
                with_context,
            } => {
                if !self.harness.agents.enabled
                    || self.harness.context.specialties.get(&name).is_none()
                {
                    return Err(RunError::UnknownSpecialist(name));
                }
                let display = format!("/agent {name} {task}");
                (TurnKind::Agent { name, with_context }, task, display)
            }
        };
        let output = self
            .harness
            .run_kind(
                &mut self.conversation.0,
                &kind,
                &prompt,
                &display,
                &mut on_event,
            )
            .await;
        // Saved after a failed turn too, as the CLI does; a failed write loses this
        // save, never the turn (the harness saved after every step already).
        if let Some(store) = &self.harness.memory {
            let _ = store.persist(&self.conversation.0);
        }
        let output = output?;
        Ok(RunResult {
            answer: final_answer(&self.conversation.0.messages).to_owned(),
            output,
            turn: self.conversation.0.last_turn.clone(),
            usage: self.conversation.0.usage,
        })
    }

    /// Hand the model what the user did on a surface (a button pressed), when no
    /// `render_ui` call was waiting for it: it becomes the next user message,
    /// `[ui action] <name> on <surfaceId>: <context as JSON>` (plus the event's
    /// `userMessage` on its own line), and runs a turn as [`Agent::run`] does,
    /// through the vault and the ingress guardrail like typed input. Recorded as
    /// `ui_action`, without its values. Call it after the turn that drew the
    /// surface has ended; actions taken during a turn wait for it, as typed
    /// input does.
    #[cfg(feature = "agent86ui")]
    pub async fn ui_action(&mut self, action: Action) -> Result<RunResult, RunError> {
        self.ui_action_with_events(action, |_| {}).await
    }

    /// [`Agent::ui_action`], reporting each [`TurnEvent`] as it happens.
    #[cfg(feature = "agent86ui")]
    pub async fn ui_action_with_events(
        &mut self,
        action: Action,
        on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<RunResult, RunError> {
        let text = self
            .harness
            .accept_ui_action(&self.conversation.0.session_id, &action)
            .map_err(RunError::UiAction)?;
        self.run_with_events(Turn::Goal(text), on_event).await
    }

    /// A handle that stops the running turn at its next safe point (before a model
    /// call, while streaming, or between tool calls), from any thread.
    pub fn cancel_handle(&self) -> CancelToken {
        self.harness.cancel.clone()
    }

    /// The conversation so far, to read or to save with [`Conversation::to_json`].
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// Continue an earlier conversation instead of starting a new one.
    /// With agent86UI, the surfaces are rebuilt from its `render_ui` calls, as
    /// on resuming a session; [`Agent::surfaces`] gives them to draw.
    pub fn with_conversation(mut self, conversation: Conversation) -> Self {
        self.conversation = conversation;
        #[cfg(feature = "agent86ui")]
        self.replay_surfaces();
        self
    }

    /// Rebuild the renderer's surfaces from the conversation's history.
    #[cfg(feature = "agent86ui")]
    fn replay_surfaces(&self) {
        if let Some(ui) = &self.harness.context.ui {
            ui.replay(&self.conversation.0, &self.harness.context);
        }
    }

    /// Every surface up now, each as the one `createSurface` message that draws
    /// it as it is (its components and data model), in surface id order. After
    /// resuming a session ([`AgentBuilder::session`] or
    /// [`Agent::with_conversation`]) the surfaces are rebuilt from the
    /// history's `render_ui` calls, so a program that reopens draws these to
    /// show the user where they were. What the user typed but never submitted is
    /// the renderer's alone, so it is not here. Empty without a renderer.
    #[cfg(feature = "agent86ui")]
    pub fn surfaces(&self) -> Vec<AgentMessage> {
        self.harness
            .context
            .ui
            .as_ref()
            .map(|ui| ui.snapshot())
            .unwrap_or_default()
    }

    /// The session's id, under which [`AgentBuilder::memory`] saves it.
    pub fn session_id(&self) -> &str {
        self.conversation.id()
    }

    /// The names of the secrets held, never their values.
    pub fn secret_names(&self) -> Vec<String> {
        self.harness.context.secrets.names()
    }

    /// What is worth telling the user, oldest first: specialist files that were
    /// skipped, what the MCP servers reported (a server that dropped, reconnected,
    /// or changed its tools), and why spans could not be exported.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = self.notices.clone();
        if let Some(mcp) = &self.harness.mcp {
            notes.extend(mcp.notes());
        }
        notes.extend(self.harness.tracer.note());
        notes
    }

    /// The skills [`Turn::skill`] can run, in precedence order.
    pub fn skills(&self) -> Vec<String> {
        self.harness.context.skills.keys().cloned().collect()
    }

    /// The specialists [`Turn::specialist`] and `delegate` can run.
    pub fn specialists(&self) -> Vec<String> {
        if !self.harness.agents.enabled {
            return Vec::new();
        }
        self.harness
            .context
            .specialties
            .names()
            .into_iter()
            .map(str::to_owned)
            .collect()
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

/// A conversation: its messages, steps, and usage. It is saved as the JSON the
/// session database keeps (`state_json`, the format agent86 and Python's agent86
/// share), so a saved conversation opens in either.
/// [`Conversation::default`] is a new conversation with a fresh id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Conversation(AgentState);

/// One message of a [`Conversation`].
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationMessage {
    pub role: Role,
    /// The text: the user's words, the model's reply, or a tool's result.
    pub text: String,
    /// The tools an assistant message called.
    pub tool_calls: Vec<ToolCall>,
    /// For a tool's result, the id of the call it answers.
    pub answers: Option<String>,
}

impl Conversation {
    /// A new, empty conversation with a fresh id.
    pub fn new() -> Self {
        Self::default()
    }

    /// Its id, which the session database files it under.
    pub fn id(&self) -> &str {
        &self.0.session_id
    }

    /// Its messages, oldest first (the system prompt is not one of them).
    pub fn messages(&self) -> Vec<ConversationMessage> {
        self.0
            .messages
            .iter()
            .map(|message| ConversationMessage {
                role: message.role,
                text: message.content.clone(),
                tool_calls: message.tool_calls.clone(),
                answers: message.tool_call_id.clone(),
            })
            .collect()
    }

    /// The model calls made so far.
    pub fn steps(&self) -> usize {
        self.0.step_count()
    }

    /// The tokens and cost so far.
    pub fn usage(&self) -> Usage {
        self.0.usage
    }

    /// The last turn's summary, or `None` before the first.
    pub fn last_turn(&self) -> Option<&TurnSummary> {
        self.0.last_turn.as_ref()
    }

    /// The conversation as the session database's JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).expect("a conversation always serializes")
    }

    /// A conversation from [`Conversation::to_json`]'s JSON, or a session
    /// database's `state_json`.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json).map(Self)
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

/// The model layer: the [`Provider`] trait, which only needs `stream`, the types
/// a provider speaks, and agent86's own adapters, which run any OpenAI-compatible
/// endpoint (OpenAI, OpenRouter, Groq, vLLM, LM Studio, llama.cpp), Anthropic,
/// Ollama, and Google Gemini, with agent86's retries, redaction, and pricing.
pub mod providers {
    pub use crate::provider::anthropic::{AnthropicConfig, AnthropicProvider};
    pub use crate::provider::google::{GoogleConfig, GoogleProvider};
    pub use crate::provider::ollama::{OllamaConfig, OllamaProvider};
    pub use crate::provider::openai::{OpenAiConfig, OpenAiProvider};
    pub use crate::provider::{collect_completion, DeltaStream, Provider, ProviderError};
    pub use crate::types::{
        Completion, CompletionDelta, CompletionRequest, Message, StopReason, ToolSpec,
    };
}
