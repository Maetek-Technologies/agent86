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
//! [`TurnEvent`] stream. What it leaves out for now: MCP servers, long-term
//! memory, skills, sub-agents, triage routing, and OpenTelemetry, each of which
//! the CLI builds from config in [`crate::setup`].
//!
//! This module is the intended public surface; the rest of the crate is public
//! for its own tests and binaries and makes no stability promise.

use crate::agent::{CancelToken, Harness, HarnessError, TurnEvent};
use crate::approval::{ApprovalGate, ApprovalPreview};
use crate::config::{builtin_config, GuardrailsConfig, LimitsConfig, ObservabilityConfig};
use crate::guardrails::Guardrails;
use crate::observability::Recorder;
use crate::provider::Provider;
use crate::session::AgentState;
use crate::tools::sandbox::Executor;
use crate::tools::{exec, files, search, web, Tool, ToolContext, ToolError, ToolRegistry};
use crate::turn::{final_answer, TurnSummary};
use crate::types::{ApprovalMode, Message, Role, ToolResult, Usage};
use serde_json::{Map, Value};
use std::path::PathBuf;
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

    /// Assemble the agent. Reads nothing but the workspace path.
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
            mcp: None,
            memory: None,
            cancel: Default::default(),
            subagents: Default::default(),
        };
        harness.gate.queue.watch(harness.cancel.clone());
        harness
            .context
            .secrets
            .set_queue(harness.gate.queue.clone());
        Ok(Agent {
            harness,
            state: AgentState::new(),
        })
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
            .await?;
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
