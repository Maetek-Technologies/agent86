//! Sub-agents, ported from Python's `agents/subagent.py` and the harness's
//! `spawn_subagent`, plus specialist sub-agents (beyond Python; see
//! `docs/subagents.md` and [`crate::agents_def`]).
//!
//! A sub-agent is a role-scoped Reason, Act, Observe loop that reuses the
//! harness's tool registry, sandbox, approval gate, and recorder, so a delegated
//! task gets the same guardrails and observability as the main agent. It runs on
//! the turn's model (routed or not, as Python's `h.provider`), under its own step
//! budget (`[agents] max_steps`, never more than the turn's) and the main loop's
//! output cap, reports every model call to the recorder tagged with its role and
//! depth, and hands its token usage back, so the parent turn's cost includes what
//! it spent. Depth is tracked so delegation cannot recurse without bound.
//!
//! A *specialty* (`delegate`'s `agent`) replaces the generic persona with its own
//! instructions and narrows what the sub-agent may do: its tools, a stricter
//! approval mode, always-asked tools, its own model and step budget. The narrowing
//! is a [`ToolScope`] on the sub-agent's forked context, set once at spawn and
//! intersected with the caller's, so authority only narrows through nesting. Its
//! system prompt is a filtered copy of the parent's that lists only what it may
//! use. A project's own specialty files are repo text, run only once the user
//! approved that exact file ([`Harness::approve_specialty`]).
//!
//! Beyond Python, a sub-agent stops at the main loop's safe points when the turn
//! is cancelled (before each model call, during it, and between tool calls).

use crate::agent::{
    invalid_arguments_result, observation, CircuitBreaker, Harness, HarnessError, ANSWER_PROMPT,
    CONTINUE_PROMPT, MAX_CONTINUATIONS,
};
use crate::agents_def::Specialty;
use crate::approval::ApprovalPreview;
use crate::limits::LimitKind;
use crate::memory::working::fit;
use crate::prompt::time_note;
use crate::provider::{Provider, ProviderError};
use crate::tools::delegate::{self, DelegateRequest, NAME as DELEGATE};
use crate::tools::{success, ToolContext, ToolScope};
use crate::types::{ApprovalMode, CompletionRequest, Message, Role, ToolCall, ToolResult, Usage};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// The sub-agent's persona, ahead of the parent's system content.
pub const ROLE_PREAMBLE: &str = "You are a '{role}' sub-agent operating inside a larger agent system. You were delegated a focused task by a supervising agent. Use tools as needed, complete the task precisely, and return ONLY the final result \u{2014} no preamble.";

/// Ahead of a specialty's own instructions, so it knows where its task comes from
/// and what to hand back.
pub const SPECIALTY_PREAMBLE: &str = "You are the '{name}' specialist sub-agent operating inside a larger agent system. A supervising agent delegated a focused task to you. Follow your instructions below, use only the tools you were given, and return ONLY the final result \u{2014} no preamble.";

/// A sub-agent's answer and what it spent, or the model failure that stopped it
/// with what it spent first.
pub type SubagentResult = Result<(String, Usage), Box<(ProviderError, Usage)>>;

/// Specialist sub-agents running at once from one batch (beyond Python;
/// `docs/subagents.md` section 6): one spend counter against the turn's cost cap,
/// seeded with what the turn had spent, fed by every sibling (and anything it
/// delegates to) after each model call, and one stop. A trip cancels every
/// sibling at its next safe point through `cancel`, never the turn's own token,
/// so an editor is not told the user cancelled; the turn then ends through its
/// breaker, which sees the same spend after the batch. The overshoot is at most
/// the calls already in flight.
#[derive(Debug)]
pub struct FanOut {
    spent: Mutex<f64>,
    /// `[limits] max_cost_usd`; 0 means no cap.
    cap: f64,
    /// Stops every sibling.
    pub cancel: crate::agent::CancelToken,
    tripped: Mutex<Option<String>>,
}

impl FanOut {
    pub fn new(spent: f64, cap: f64) -> Self {
        Self {
            spent: Mutex::new(spent),
            cap,
            cancel: crate::agent::CancelToken::default(),
            tripped: Mutex::new(None),
        }
    }

    /// Add a model call's cost; at the cap, stop every sibling.
    pub fn add(&self, cost_usd: f64) {
        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        *spent += cost_usd;
        if self.cap > 0.0 && *spent >= self.cap {
            let mut tripped = self.tripped.lock().unwrap_or_else(PoisonError::into_inner);
            if tripped.is_none() {
                *tripped = Some(format!(
                    "cost cap reached (${:.4} >= ${})",
                    *spent,
                    crate::pyjson::python_float_repr(self.cap)
                ));
            }
            self.cancel.cancel();
        }
    }

    /// Why the siblings were stopped, once they were.
    pub fn tripped(&self) -> Option<String> {
        self.tripped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Where a sub-agent's progress is reported (beyond Python;
/// `docs/subagents.md` section 10): one card per top-level delegation, named by
/// that `delegate` call's id, or a synthetic one for `/agent`. Nested runs report on
/// their top-level card, and every call a sub-agent makes gets an id namespaced
/// under it (`<card>/<n>`), since provider ids repeat (Ollama's restart with every
/// response) and two siblings would otherwise both present the same one.
#[derive(Debug)]
pub struct Card {
    pub id: String,
    /// Whether the surface was already shown the call this card belongs to (a
    /// `delegate` call is; `/agent` has none).
    pub announced: bool,
    next: std::sync::atomic::AtomicU64,
}

impl Card {
    pub fn new(id: impl Into<String>, announced: bool) -> Self {
        Self {
            id: id.into(),
            announced,
            next: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The next id for a call under this card.
    pub fn call_id(&self) -> String {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        format!("{}/{n}", self.id)
    }
}

/// One sub-agent run as its questions describe it (beyond Python): who it is, and
/// its model calls and spend so far, kept current as it runs, so an editor's
/// approval inbox can show them beside each question
/// ([`crate::approval::Asker`]). On its [`ToolContext`]; a nested run has its own.
#[derive(Debug)]
pub struct SubagentRun {
    pub run: u64,
    /// The specialty or role it runs as.
    pub name: String,
    pub depth: u32,
    progress: Mutex<(u32, f64)>,
}

impl SubagentRun {
    pub fn new(run: u64, name: impl Into<String>, depth: u32) -> Self {
        Self {
            run,
            name: name.into(),
            depth,
            progress: Mutex::new((0, 0.0)),
        }
    }

    /// Its model calls so far, and what it and anything it delegated have spent.
    pub fn progress(&self) -> (u32, f64) {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, steps: u32, cost_usd: f64) {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner) = (steps, cost_usd);
    }
}

/// What the observer is told about a sub-agent (beyond Python): an editor turns it
/// into the delegation's card, the terminal interface into notes. Everything here
/// is already redacted by the time it is reported, except a call's arguments,
/// which the surface redacts as it does the main loop's.
#[derive(Debug)]
pub enum SubagentEvent<'a> {
    Started {
        card: &'a Card,
        run: u64,
        name: &'a str,
        depth: u32,
        task: &'a str,
    },
    ToolCall {
        card: &'a Card,
        call: &'a ToolCall,
    },
    ToolResult {
        card: &'a Card,
        call: &'a ToolCall,
        result: &'a ToolResult,
    },
    Notice {
        card: &'a Card,
        text: &'a str,
    },
    Finished {
        card: &'a Card,
        run: u64,
        name: &'a str,
        depth: u32,
        ok: bool,
        /// What it and anything it delegated used (beyond Python: for an editor's
        /// per-delegation cost).
        usage: &'a Usage,
    },
}

/// Receives [`SubagentEvent`]s from whichever thread a sub-agent runs on.
pub type SubagentObserver = Arc<dyn Fn(&SubagentEvent<'_>) + Send + Sync>;

/// Where approvals of project specialties are remembered, beside the user config.
pub const APPROVALS_FILE: &str = "approved-agents.json";

fn message(role: Role, content: String) -> Message {
    Message {
        role,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}

/// The harness's sub-agent state (beyond Python).
#[derive(Default)]
pub struct SubagentState {
    /// What sub-agents had to tell the user during the turn, which the loop
    /// emits as notices after the batch.
    notices: Mutex<Vec<String>>,
    /// Approvals of project specialties.
    pub approvals: ApprovalStore,
    /// `run --agent`: the specialty's persona and preloaded skills, which make the
    /// main loop's system message a filtered one.
    main: Mutex<Option<MainSpecialty>>,
    /// The last per-spawn id handed out: sub-agent events carry it as `run`, so
    /// concurrent runs of one specialty, which share the `sub:<role>` session, can
    /// be told apart.
    runs: std::sync::atomic::AtomicU64,
    /// The surface's observer of sub-agents, if it installed one.
    observer: Mutex<Option<SubagentObserver>>,
}

/// A specialty running as the main agent: its persona and preloaded skills.
type MainSpecialty = (String, Vec<Arc<crate::skills::Skill>>);

impl SubagentState {
    pub(crate) fn set_main(&self, main: Option<MainSpecialty>) {
        *self.main.lock().unwrap_or_else(PoisonError::into_inner) = main;
    }

    /// Report sub-agents to `observer` from now on.
    pub fn set_observer(&self, observer: Option<SubagentObserver>) {
        *self.observer.lock().unwrap_or_else(PoisonError::into_inner) = observer;
    }

    pub(crate) fn observe(&self, event: &SubagentEvent<'_>) {
        let observer = self
            .observer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(observer) = observer {
            observer(event);
        }
    }

    fn observed(&self) -> bool {
        self.observer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    pub(crate) fn next_run(&self) -> u64 {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    pub(crate) fn main(&self) -> Option<MainSpecialty> {
        self.main
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Approved project specialties, remembered by resolved path and SHA-256 in
/// `~/.agent86/approved-agents.json`, outside `state_json` and the session
/// database, so an edited file is asked about again.
#[derive(Debug, Clone, Default)]
pub struct ApprovalStore {
    /// The store file; `None` remembers nothing.
    pub path: Option<PathBuf>,
    /// `--trust-project` or `AGENT86_TRUST_PROJECT`: every project specialty
    /// counts as approved for this run, and nothing is remembered. Never set for
    /// an editor session, where the editor still asks.
    pub forced: bool,
}

impl ApprovalStore {
    /// The store beside the user config in `home`.
    pub fn under_home(home: &std::path::Path) -> Self {
        Self {
            path: Some(home.join(".agent86").join(APPROVALS_FILE)),
            forced: false,
        }
    }

    fn read(&self) -> serde_json::Map<String, serde_json::Value> {
        self.path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|document| document.get("agents")?.as_object().cloned())
            .unwrap_or_default()
    }

    /// Whether this exact file was approved (or needs no approval).
    pub fn approved(&self, specialty: &Specialty) -> bool {
        if !specialty.is_project() {
            return true;
        }
        let Some(path) = specialty.source.path() else {
            return true;
        };
        self.forced
            || self
                .read()
                .get(&path.display().to_string())
                .and_then(|hash| hash.as_str())
                == Some(specialty.hash().as_str())
    }

    /// Remember this exact file as approved. A failure only means the question
    /// comes back next time.
    pub fn record(&self, specialty: &Specialty) {
        let (Some(store), Some(path)) = (&self.path, specialty.source.path()) else {
            return;
        };
        let mut agents = self.read();
        agents.insert(path.display().to_string(), json!(specialty.hash()));
        let Ok(text) = serde_json::to_string_pretty(&json!({ "version": 1, "agents": agents }))
        else {
            return;
        };
        if let Some(parent) = store.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let partial = store.with_extension("json.incomplete");
        if std::fs::write(&partial, text + "\n").is_ok() {
            let _ = std::fs::rename(&partial, store);
        }
    }
}

/// The model a sub-agent runs on, with what the harness derives from it.
struct RunModel {
    provider: Arc<dyn Provider>,
    context_window: u64,
    /// The configured output cap (a session override still replaces it).
    max_tokens: u32,
    /// What to raise for a larger cap, for messages.
    output_setting: String,
    /// The specialty's own `model:`, so a raise of its cap is for this run only.
    own: bool,
}

/// What a sub-agent is asked to do, resolved.
pub(crate) struct Spawn {
    /// The generic role, or the specialty's name.
    pub role: String,
    pub task: String,
    pub specialty: Option<Arc<Specialty>>,
    /// Skills preloaded into its system message.
    pub preloaded: Vec<Arc<crate::skills::Skill>>,
    /// The worktree made for it (isolation), which it runs in and whose diff its
    /// answer carries; `None` when it shares its caller's workspace or worktree.
    pub worktree: Option<Arc<crate::worktree::Worktree>>,
    /// The card it reports on, for a top-level run; a nested run uses its caller's.
    pub card: Option<Arc<Card>>,
}

impl Harness {
    /// Whether `delegate` is offered to `ctx` at `depth`.
    pub fn delegation_offered(&self, ctx: &ToolContext, depth: u32) -> bool {
        self.agents.enabled
            && depth < self.agents.max_depth
            && self.registry.get(DELEGATE).is_some()
            && ctx.allowed_tool(DELEGATE)
    }

    /// The system prompt's list of specialist sub-agents for `ctx`, or `None` with
    /// none. An unapproved project specialty is listed by name only: its
    /// description is repo text too.
    pub(crate) fn specialty_listing(&self, ctx: &ToolContext) -> Option<String> {
        if ctx.specialties.is_empty() {
            return None;
        }
        let mut text = String::from(
            "Specialist sub-agents (call delegate with `agent` set to one of these names; each \
             runs with its own instructions and tools):",
        );
        for specialty in ctx.specialties.found.values() {
            if !self.subagents.approvals.approved(specialty) {
                text.push_str(&format!(
                    "\n- {}: (a project file the user has not approved yet; they are asked \
                     before it first runs)",
                    specialty.name
                ));
                continue;
            }
            text.push_str(&format!(
                "\n- {}: {}",
                specialty.name, specialty.description
            ));
            if let Some(tools) = &specialty.tools {
                let tools = if tools.is_empty() {
                    "none".to_owned()
                } else {
                    tools.join(", ")
                };
                text.push_str(&format!(" [tools: {tools}]"));
            }
        }
        text.push_str(
            "\nSeveral delegate calls with `agent` in one step run at the same time. Give any \
             that writes files `isolate: true`, so each works in a git worktree of its own and \
             hands back its diff.",
        );
        Some(text)
    }

    /// Notices sub-agents left for the user this turn, oldest first.
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(
            &mut self
                .subagents
                .notices
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// A notice about the sub-agent on `card`: a line on its card when a surface
    /// observes sub-agents (an editor would otherwise show it in the middle of the
    /// reply), else one of the turn's notices.
    pub(crate) fn notice_on(&self, card: Option<&Arc<Card>>, text: String) {
        match card {
            Some(card) if self.subagents.observed() => {
                self.subagents
                    .observe(&SubagentEvent::Notice { card, text: &text });
            }
            _ => self.notice(text),
        }
    }

    pub(crate) fn notice(&self, text: String) {
        self.subagents
            .notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text);
    }

    /// The one check every start of a project specialty goes through (`delegate`
    /// at any depth, `/agent`, `run --agent`): a built-in or user specialty passes;
    /// a project one passes once the user approved this exact file. The whole file
    /// is shown, and a yes is remembered by its hash. It is asked in every mode but
    /// `deny`, and refused when nobody can be asked. `call` is the call the
    /// question belongs to, for a surface that names it.
    pub fn approve_specialty(&self, specialty: &Specialty, call: &ToolCall) -> Result<(), String> {
        if self.subagents.approvals.approved(specialty) {
            return Ok(());
        }
        let path = specialty
            .source
            .path()
            .map(|path| crate::escape::show_controls(&crate::tools::display_path(path)))
            .unwrap_or_default();
        let name = &specialty.name;
        if self.gate.mode() == ApprovalMode::Deny {
            return Err(format!(
                "The project specialty '{name}' ({path}) has not been approved, and approval \
                 mode deny refuses to ask."
            ));
        }
        let preview = ApprovalPreview {
            summary: format!("run the project specialty '{name}' ({path})"),
            detail: Some(specialty.text.clone()),
            lexer: None,
            warning: Some(format!(
                "This project defines the specialty '{name}'. Its file becomes a sub-agent's \
                 instructions. Approving remembers this version; an edited file is asked about \
                 again."
            )),
            full: true,
            agent: Some(name.clone()),
            change: None,
            asker: None,
        };
        match self.gate.ask_specialty(call, &preview) {
            Some(true) => {
                self.subagents.approvals.record(specialty);
                Ok(())
            }
            Some(false) => Err(format!(
                "The user declined to run the project specialty '{name}' ({path})."
            )),
            None => Err(format!(
                "The project specialty '{name}' ({path}) needs the user's approval, and nobody \
                 can be asked. Run agent86 at a terminal to review it, or pass --trust-project \
                 (or set AGENT86_TRUST_PROJECT=1)."
            )),
        }
    }

    /// Whether `call` delegates to a specialist sub-agent the main loop may run:
    /// what [`Harness::fan_out`] can run beside others. Plain role-only delegations
    /// stay sequential, as in Python.
    pub(crate) fn is_specialty_delegation(&self, call: &ToolCall) -> bool {
        call.name == DELEGATE
            && self.agents.enabled
            && call.invalid_arguments().is_none()
            && call
                .arguments
                .get("agent")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|agent| !agent.trim().is_empty())
            && self.context.allowed_tool(DELEGATE)
            && self.registry.get(DELEGATE).is_some()
    }

    /// Run several approved specialty delegations from the main loop at once
    /// (beyond Python; `docs/subagents.md` section 6), and answer each in call
    /// order whatever order they finish in.
    ///
    /// Everything that may ask the user before a sub-agent starts (a project
    /// specialty's approval, an attached file's read) and making a worktree happen
    /// here, on the loop's thread, in call order. Then each sub-agent gets a scoped
    /// thread with its own current-thread runtime, at most `[agents] max_parallel`
    /// at a time (the rest queue): futures on the turn's runtime would not do,
    /// since tools run synchronously and one long command would freeze every
    /// sibling. Each sub-agent there calls a provider copy with an HTTP client of
    /// its own, its questions wait their turn in the gate's prompt queue, and all of
    /// them share one spend counter against the cost cap ([`FanOut`]). A run not
    /// started before a cancel is answered `Not executed: cancelled`; a thread that
    /// panics is answered as the parallel read path answers one.
    pub(crate) fn fan_out(
        &self,
        calls: &[&ToolCall],
        sid: &str,
        spent: f64,
    ) -> Vec<(ToolResult, Usage)> {
        let mut results: Vec<Option<(ToolResult, Usage)>> = vec![None; calls.len()];
        let mut queue: std::collections::VecDeque<(usize, Spawn)> = Default::default();
        for (position, call) in calls.iter().enumerate() {
            let prepared = match self.delegation(call, &self.context) {
                Some(Ok(request)) => self
                    .resolve_delegation(call, &request, &self.context)
                    .map_err(|error| {
                        let mut result = crate::tools::failure(DELEGATE, error);
                        result.call_id = call.id.clone();
                        result
                    }),
                Some(Err(invalid)) => Err(invalid),
                None => Err(self.registry.dispatch(call, &self.context)),
            };
            match prepared {
                Ok(spawn) => queue.push_back((position, spawn)),
                Err(result) => {
                    self.record_tool_call(call, &result, sid);
                    results[position] = Some((result, Usage::default()));
                }
            }
        }
        let shared = Arc::new(FanOut::new(spent, self.limits.max_cost_usd));
        let mut caller = self.context.clone();
        caller.fanout = Some(Arc::clone(&shared));
        caller.fresh_clients = true;
        let workers = self.agents.parallel_limit().min(queue.len());
        let queue = Mutex::new(queue);
        let done: Mutex<Vec<(usize, (ToolResult, Usage))>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let next = queue
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .pop_front();
                    let Some((position, spawn)) = next else {
                        break;
                    };
                    let call = calls[position];
                    let outcome = if self.cancel.is_cancelled() {
                        if let Some(worktree) = &spawn.worktree {
                            worktree.remove();
                        }
                        (
                            crate::agent::not_executed(call, "cancelled"),
                            Usage::default(),
                        )
                    } else {
                        let mut span = self.tool_span(call);
                        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let runtime = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .map_err(|error| error.to_string())?;
                            Ok::<_, String>(
                                runtime.block_on(self.spawn_resolved(spawn, 1, &caller)),
                            )
                        }));
                        let outcome = match ran {
                            Ok(Ok(result)) => delegate_outcome(call, result),
                            Ok(Err(error)) => (
                                crate::agent::not_executed(
                                    call,
                                    &format!("its runtime could not start ({error})"),
                                ),
                                Usage::default(),
                            ),
                            Err(_) => (
                                crate::agent::not_executed(call, "the tool panicked"),
                                Usage::default(),
                            ),
                        };
                        span.set(vec![("tool.ok", json!(outcome.0.ok))]);
                        self.tracer.end(span);
                        self.record_tool_call(call, &outcome.0, sid);
                        outcome
                    };
                    done.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((position, outcome));
                });
            }
        });
        for (position, outcome) in done.into_inner().unwrap_or_else(PoisonError::into_inner) {
            results[position] = Some(outcome);
        }
        results
            .into_iter()
            .zip(calls)
            .map(|(result, call)| {
                result.unwrap_or_else(|| {
                    (
                        crate::agent::not_executed(call, "cancelled"),
                        Usage::default(),
                    )
                })
            })
            .collect()
    }

    /// The request of a `delegate` call the harness should run itself, or `None`
    /// when the registry should answer it (delegation off, the tool not
    /// registered, or refused by the active skill or specialty). Invalid arguments
    /// come back as the tool's own validation failure.
    pub(crate) fn delegation(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Option<Result<DelegateRequest, ToolResult>> {
        if call.name != DELEGATE || !self.agents.enabled || !ctx.allowed_tool(&call.name) {
            return None;
        }
        let tool = self.registry.get(DELEGATE)?;
        Some(delegate::request(&call.arguments).map_err(|_| tool.run(call, ctx)))
    }

    /// Run an approved delegation from `caller` at `depth` and answer the call with
    /// the sub-agent's result, or with the failure that stopped it.
    pub(crate) async fn delegate(
        &self,
        call: &ToolCall,
        request: DelegateRequest,
        depth: u32,
        caller: &ToolContext,
    ) -> (ToolResult, Usage) {
        match self.resolve_delegation(call, &request, caller) {
            Err(error) => {
                let mut result = crate::tools::failure(DELEGATE, error);
                result.call_id = call.id.clone();
                (result, Usage::default())
            }
            Ok(spawn) => delegate_outcome(call, self.spawn_resolved(spawn, depth, caller).await),
        }
    }

    /// Resolve a delegation's specialty, approval, preloaded skills, and first
    /// message, or say why it cannot run.
    pub(crate) fn resolve_delegation(
        &self,
        call: &ToolCall,
        request: &DelegateRequest,
        caller: &ToolContext,
    ) -> Result<Spawn, String> {
        let specialty = match &request.agent {
            None => {
                if request.skill.is_some() {
                    return Err("The `skill` argument needs `agent`: name the specialist \
                                sub-agent that should load it."
                        .into());
                }
                None
            }
            Some(name) => Some(self.specialty_for(name, caller, call)?),
        };
        // A top-level delegation is a card of its own, named by its call.
        let card = caller
            .card
            .is_none()
            .then(|| Arc::new(Card::new(call.id.clone(), true)));
        let notice_card = card.as_ref().or(caller.card.as_ref());
        let mut preloaded: Vec<Arc<crate::skills::Skill>> = Vec::new();
        if let Some(specialty) = &specialty {
            for name in specialty.skills.iter().chain(request.skill.as_ref()) {
                match caller.skills.get(name) {
                    Some(skill) => {
                        if !preloaded.iter().any(|known| Arc::ptr_eq(known, skill)) {
                            preloaded.push(Arc::clone(skill));
                        }
                    }
                    None if request.skill.as_deref() == Some(name.as_str()) => {
                        return Err(format!(
                            "Unknown skill '{}'. Available: {}.",
                            crate::escape::show_controls(name),
                            list_or_none(caller.skills.keys().map(String::as_str))
                        ));
                    }
                    None => self.notice_on(
                        notice_card,
                        format!(
                            "[agent] specialty '{}' names the skill '{name}', which is not \
                         installed; it runs without it.",
                            specialty.name
                        ),
                    ),
                }
            }
        }
        // Isolation needs a specialty; inside a worktree it is the same worktree.
        let isolate = request.isolate || specialty.as_ref().is_some_and(|s| s.worktree);
        if isolate && specialty.is_none() {
            return Err(
                "The `isolate` argument needs `agent`: name the specialist sub-agent \
                        that should run in a worktree."
                    .into(),
            );
        }
        let task = self.first_message(request, caller)?;
        let worktree = if isolate && caller.worktree.is_none() {
            Some(Arc::new(crate::worktree::Worktree::create(
                caller.policy.workspace(),
            )?))
        } else {
            None
        };
        let role = match &specialty {
            Some(specialty) => specialty.name.clone(),
            None if request.role.is_empty() => "assistant".to_owned(),
            None => request.role.clone(),
        };
        Ok(Spawn {
            role,
            task,
            specialty,
            preloaded,
            worktree,
            card,
        })
    }

    /// The specialty `name`, approved to run from `caller`, or why not.
    pub(crate) fn specialty_for(
        &self,
        name: &str,
        caller: &ToolContext,
        call: &ToolCall,
    ) -> Result<Arc<Specialty>, String> {
        let Some(specialty) = caller.specialties.get(name) else {
            return Err(format!(
                "Unknown specialist sub-agent '{}'. Available: {}.",
                crate::escape::show_controls(name),
                list_or_none(caller.specialties.names().into_iter())
            ));
        };
        self.approve_specialty(specialty, call)?;
        Ok(Arc::clone(specialty))
    }

    /// The sub-agent's first message: the task, then the parent's summary, then the
    /// attached files. None of it passes through `ToolRegistry::dispatch` as part of
    /// the message, so the vault's redaction and the observation guardrail are
    /// applied here; each file is read through `read_file` on the caller's own
    /// authority (its allowlist, active skill, and always-asked tools), and in an
    /// editor session through the editor, so an unsaved buffer is what is attached.
    fn first_message(
        &self,
        request: &DelegateRequest,
        caller: &ToolContext,
    ) -> Result<String, String> {
        let mut text = caller.secrets.redact(&request.task);
        if let Some(summary) = &request.summary {
            let (summary, _) = self.guardrails.observe(caller.secrets.redact(summary));
            text.push_str(&format!(
                "\n\n[Context from the supervising agent]\n{summary}"
            ));
        }
        for path in &request.files {
            let Some(tool) = self.registry.get("read_file") else {
                return Err("Files cannot be attached: read_file is not available.".into());
            };
            if !caller.allowed_tool("read_file") {
                return Err(format!(
                    "The file {path} was not attached: read_file is not permitted here."
                ));
            }
            let call = ToolCall {
                id: String::new(),
                name: "read_file".into(),
                arguments: serde_json::Map::from_iter([("path".to_owned(), json!(path))]),
            };
            let decision = self.gate.decide(tool.as_ref(), &call, caller);
            if !decision.approved {
                return Err(format!(
                    "The file {path} was not attached: {}.",
                    decision.reason
                ));
            }
            let result = self.registry.dispatch(&call, caller);
            if !result.ok {
                return Err(format!(
                    "The file {path} could not be attached: {}",
                    result.error.unwrap_or_default()
                ));
            }
            let (content, _) = self.guardrails.observe(result.content);
            text.push_str(&format!("\n\n[Attached file: {path}]\n{content}"));
        }
        Ok(text)
    }

    /// Run a delegated task on a role-scoped sub-agent, as Python's
    /// `spawn_subagent`: its answer and everything it (and anything it delegated
    /// to) spent, or the model failure that stopped it with what it spent first.
    pub fn spawn_subagent<'a>(
        &'a self,
        role: &'a str,
        task: &'a str,
        depth: u32,
    ) -> BoxFuture<'a, SubagentResult> {
        let spawn = Spawn {
            role: role.to_owned(),
            task: task.to_owned(),
            specialty: None,
            preloaded: Vec::new(),
            worktree: None,
            card: None,
        };
        self.spawn_resolved(spawn, depth, &self.context)
    }

    /// Run `spawn` at `depth`, delegated from `caller`, whose context the
    /// sub-agent forks and narrows.
    pub(crate) fn spawn_resolved<'a>(
        &'a self,
        spawn: Spawn,
        depth: u32,
        caller: &'a ToolContext,
    ) -> BoxFuture<'a, SubagentResult> {
        Box::pin(async move {
            let max_depth = self.agents.max_depth;
            if depth > max_depth {
                if let Some(worktree) = &spawn.worktree {
                    worktree.remove();
                }
                return Ok((
                    format!("[delegation refused: max agent depth {max_depth} reached]"),
                    Usage::default(),
                ));
            }
            let run = self.subagents.next_run();
            let preview: String = spawn.task.chars().take(200).collect();
            self.recorder.event(
                "sub",
                "spawn",
                vec![
                    ("role", json!(spawn.role)),
                    ("depth", json!(depth)),
                    ("run", json!(run)),
                    ("task", json!(preview)),
                ],
            );
            let mut spawn = spawn;
            if spawn.role.is_empty() {
                spawn.role = "assistant".into();
            }
            let worktree = spawn.worktree.clone();
            let card = spawn.card.clone().or_else(|| caller.card.clone());
            let name = spawn.role.clone();
            let result = self.run_subagent(spawn, depth, caller, run).await;
            if let Some(card) = &card {
                let usage = match &result {
                    Ok((_, usage)) => *usage,
                    Err(failure) => failure.1,
                };
                self.subagents.observe(&SubagentEvent::Finished {
                    card,
                    run,
                    name: &name,
                    depth,
                    ok: result.is_ok(),
                    usage: &usage,
                });
            }
            // The worktree's diff goes back with the answer, through the vault's
            // redaction: `write_file` expands secrets, so a diff can hold a value.
            let Some(worktree) = worktree else {
                return result;
            };
            let note = worktree.finish(|diff| caller.secrets.redact(diff));
            result.map(|(answer, usage)| (format!("{answer}{note}"), usage))
        })
    }

    /// The model a sub-agent runs on: the specialty's own, else the turn's. One
    /// that cannot be built falls back to the turn's, with a notice for the user
    /// and a `route` event naming the model it wanted.
    fn subagent_model(
        &self,
        specialty: Option<&Specialty>,
        sid: &str,
        ctx: &ToolContext,
    ) -> Result<RunModel, ProviderError> {
        let mut model = self.resolve_subagent_model(specialty, sid, ctx.card.as_ref());
        // On a fan-out thread, a client of its own (and no fallback to sharing).
        if ctx.fresh_clients {
            model.provider = model.provider.fresh()?;
        }
        Ok(model)
    }

    fn resolve_subagent_model(
        &self,
        specialty: Option<&Specialty>,
        sid: &str,
        card: Option<&Arc<Card>>,
    ) -> RunModel {
        let turn = || RunModel {
            provider: self.active_provider(),
            context_window: self.active_context_window(),
            max_tokens: self.configured_output(),
            output_setting: self.output_setting(),
            own: false,
        };
        let Some((name, wanted)) =
            specialty.and_then(|s| s.model.as_deref().map(|model| (s.name.as_str(), model)))
        else {
            return turn();
        };
        match self.router.model_for(wanted) {
            Ok(model) => RunModel {
                provider: model.provider,
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                output_setting: model.output_setting,
                own: true,
            },
            Err(error) => {
                let fallback = turn();
                self.notice_on(
                    card,
                    format!(
                    "[agent] specialty '{name}' could not use its model {} ({}); it runs on {} \
                     instead.",
                    crate::escape::show_controls(wanted),
                    crate::escape::show_controls(&error),
                    fallback.provider.model()
                ),
                );
                self.recorder.event(
                    sid,
                    "route",
                    vec![
                        ("model", json!(fallback.provider.model())),
                        ("agent", json!(name)),
                        ("wanted", json!(wanted)),
                    ],
                );
                fallback
            }
        }
    }

    /// The scope a specialty runs under from `caller`: its tools (with no `tools`
    /// line, all but `delegate`), stricter approval, and always-asked tools,
    /// narrowed by the caller's own scope; preloaded skills narrow the tools again.
    pub(crate) fn specialty_scope(
        specialty: &Specialty,
        preloaded: &[Arc<crate::skills::Skill>],
        caller: &ToolContext,
    ) -> ToolScope {
        let mut scope = ToolScope {
            label: specialty.name.clone(),
            only: specialty.tools.clone(),
            except: if specialty.tools.is_none() {
                vec![DELEGATE.to_owned()]
            } else {
                Vec::new()
            },
            approval: specialty.approval,
            confirm: specialty.confirm.clone(),
            preloaded: preloaded.iter().map(|skill| skill.name.clone()).collect(),
            isolated: false,
        };
        // A tool must also be in at least one preloaded skill's `allowed-tools`;
        // a preloaded skill with none lifts that condition.
        if !preloaded.is_empty() && preloaded.iter().all(|s| !s.allowed_tools.is_empty()) {
            let from_skills: Vec<String> = preloaded
                .iter()
                .flat_map(|s| s.allowed_tools.iter().cloned())
                .collect();
            scope.only = Some(match scope.only.take() {
                Some(only) => only
                    .into_iter()
                    .filter(|t| from_skills.contains(t))
                    .collect(),
                None => from_skills,
            });
        }
        scope.within(caller.scope.as_deref())
    }

    /// A specialty's system message: who it is and its instructions, then the base
    /// identity and environment, then only the skills and MCP tools it may use,
    /// its preloaded skills, the specialty list when it may delegate, the names of
    /// the user's secrets, and `extra` (the time, or a turn's notes).
    pub(crate) fn filtered_system(
        &self,
        persona: &str,
        ctx: &ToolContext,
        preloaded: &[Arc<crate::skills::Skill>],
        delegation_offered: bool,
        extra: Option<&str>,
    ) -> Message {
        let all_skills = crate::prompt::skills_section(ctx.skills.values().map(AsRef::as_ref));
        let base = self
            .system
            .content
            .strip_suffix(all_skills.as_str())
            .unwrap_or(&self.system.content);
        let mut content = format!("{persona}\n\n{base}");
        if ctx.allowed_tool("use_skill") {
            content.push_str(&crate::prompt::skills_section(
                ctx.skills.values().map(AsRef::as_ref),
            ));
        }
        for skill in preloaded {
            content.push_str(&format!(
                "\n\nPreloaded skill '{}' (follow it for this task):\n{}",
                skill.name,
                skill.load_text()
            ));
        }
        let permitted = |name: &str| ctx.scope_allows(name);
        if let Some(section) = self
            .mcp
            .as_ref()
            .map(|mcp| mcp.prompt_section_for(Some(&permitted)))
            .filter(|section| !section.is_empty())
        {
            content.push_str(&format!("\n\n{section}"));
        }
        if delegation_offered {
            if let Some(section) = self.specialty_listing(ctx) {
                content.push_str(&format!("\n\n{section}"));
            }
        }
        if let Some(section) = ctx.secrets.prompt_section() {
            content.push_str(&format!("\n\n{section}"));
        }
        if let Some(extra) = extra.filter(|extra| !extra.is_empty()) {
            content.push_str(&format!("\n\n{extra}"));
        }
        message(Role::System, content)
    }

    /// The persona ahead of a specialty's instructions.
    pub(crate) fn specialty_persona(specialty: &Specialty) -> String {
        format!(
            "{}\n\n{}",
            SPECIALTY_PREAMBLE.replace("{name}", &specialty.name),
            specialty.instructions
        )
    }

    async fn run_subagent(
        &self,
        spawn: Spawn,
        depth: u32,
        caller: &ToolContext,
        run: u64,
    ) -> SubagentResult {
        let Spawn {
            role,
            task,
            specialty,
            preloaded,
            worktree,
            card,
        } = spawn;
        let role = role.as_str();
        let sid = format!("sub:{role}");
        // Its own context, forked from whoever delegated: a skill it activates
        // restricts only its own calls, and a specialty narrows what it may do.
        let mut ctx = caller.fork();
        if let Some(specialty) = &specialty {
            ctx.scope = Some(Arc::new(Self::specialty_scope(
                specialty, &preloaded, caller,
            )));
        }
        // Isolated: jailed to the worktree, with the editor's buffers (which are the
        // real workspace's) left out, and MCP tools only when the specialty names
        // them, since they act outside any jail.
        if let Some(worktree) = &worktree {
            match ctx
                .policy
                .rerooted(&worktree.root, project_skill_roots(caller))
            {
                Ok(policy) => ctx.policy = policy,
                Err(error) => {
                    return Ok((
                        format!("[sub-agent '{role}' could not start in its worktree: {error}]"),
                        Usage::default(),
                    ))
                }
            }
            ctx.editor = None;
            ctx.worktree = Some(Arc::clone(worktree));
        }
        if ctx.worktree.is_some() {
            let mut scope = ctx.scope.as_deref().cloned().unwrap_or_default();
            scope.isolated = true;
            ctx.scope = Some(Arc::new(scope));
        }
        if card.is_some() {
            ctx.card = card;
        }
        let progress = Arc::new(SubagentRun::new(run, role, depth));
        ctx.subagent = Some(Arc::clone(&progress));
        if let Some(card) = &ctx.card {
            self.subagents.observe(&SubagentEvent::Started {
                card,
                run,
                name: role,
                depth,
                task: &task,
            });
        }
        let offered = self.delegation_offered(&ctx, depth);
        ctx.can_delegate = offered;
        // The turn's model, routed or not, as Python's sub-agent uses `h.provider`
        // (the request goes to it, and the trace names it), or the specialty's own.
        let model = match self.subagent_model(specialty.as_deref(), &sid, &ctx) {
            Ok(model) => model,
            Err(error) => return Err(Box::new((error, Usage::default()))),
        };
        let provider = Arc::clone(&model.provider);
        // Beyond Python: the time the sub-agent started ends its system message.
        let time = time_note(&jiff::ZonedDateTime::now());
        let system = match (&specialty, &ctx.scope) {
            (Some(specialty), _) => self.filtered_system(
                &Self::specialty_persona(specialty),
                &ctx,
                &preloaded,
                offered,
                Some(&time),
            ),
            // A plain role from inside a specialty does not advertise what it may
            // not use.
            (None, Some(_)) => self.filtered_system(
                &ROLE_PREAMBLE.replace("{role}", role),
                &ctx,
                &[],
                offered,
                Some(&time),
            ),
            // The persona, then the parent's system content, as Python sends it.
            (None, None) => message(
                Role::System,
                format!(
                    "{}\n\n{}",
                    ROLE_PREAMBLE.replace("{role}", role),
                    self.system_for(&ctx, offered, Some(&time)).content
                ),
            ),
        };
        let mut messages = vec![message(Role::User, task)];
        // `/limits` (or `--max-agent-steps`), then the specialty's `max_steps`, then
        // `[agents] max_steps`, always under the turn's budget.
        let own_steps = specialty.as_ref().and_then(|s| s.max_steps);
        let max_steps = match own_steps {
            Some(steps) => self
                .session_limits
                .get(LimitKind::AgentSteps)
                .unwrap_or(steps)
                .min(self.max_steps()),
            None => self.agent_max_steps(),
        };
        // A raise offered to a specialty with a budget of its own lasts for this run.
        let steps_run_only = own_steps.is_some();
        let mut limits = self.limits.clone();
        limits.max_steps = max_steps;
        let mut breaker = CircuitBreaker::new(&limits);
        let mut cap_raised: Option<u32> = None;
        let output_cap = |raised: Option<u32>| {
            raised.unwrap_or_else(|| {
                self.session_limits
                    .get(LimitKind::Output)
                    .unwrap_or(model.max_tokens)
            })
        };
        let mut spent = Usage::default();
        let mut delegated = Usage::default();
        // The pieces of an answer cut off at the output cap, and how often it was.
        let mut parts: Vec<String> = Vec::new();
        let mut continuations = 0;
        // Beyond Python, as in the main loop: an empty final reply is asked about once
        // (on the next request only), and its reasoning kept in case the next is too.
        let mut nudged = false;
        let mut ask_again = false;
        let mut unanswered_thinking = String::new();
        let halted = |error: HarnessError| match error {
            HarnessError::CircuitTripped(detail) => {
                format!("[sub-agent '{role}' halted: {detail}]")
            }
            other => format!("[sub-agent '{role}' halted: {other}]"),
        };

        loop {
            // Beyond Python, whose sub-agent has no cancel checks: the main loop's
            // safe points. Never start another model call once cancelled.
            if self.stopped(&ctx) {
                return Ok((
                    self.subagent_cancelled(role, &sid, depth, run, &ctx),
                    spent + delegated,
                ));
            }
            // Beyond Python: the interface may offer to raise a spent budget.
            if breaker.out_of_steps() {
                if let Some(raised) = self.raise_limit(
                    LimitKind::AgentSteps,
                    Some(role),
                    &sid,
                    breaker.max_steps(),
                    steps_run_only,
                ) {
                    breaker.set_max_steps(raised);
                }
            }
            if let Err(error) = breaker.before_step() {
                return Ok((halted(error), spent + delegated));
            }
            // Only what it may call is offered; at the deepest level it cannot
            // spawn further.
            let mut specs = self.registry.specs();
            specs.retain(|spec| ctx.scope_allows(&spec.name));
            if !offered {
                specs.retain(|spec| spec.name != DELEGATE);
            }
            // The same window discipline as the main loop, with the system message
            // held out of the trim so it is never the part dropped, on the window
            // and cap of the model it runs on.
            let cap = output_cap(cap_raised);
            let budget = self.context_budget_with(&system, &specs, model.context_window, cap);
            let mut request_messages = vec![system.clone()];
            request_messages.extend(fit(&messages, budget).iter().cloned());
            if std::mem::take(&mut ask_again) {
                request_messages.push(message(Role::User, ANSWER_PROMPT.to_owned()));
            }
            let request = provider.complete(CompletionRequest {
                model: provider.model().to_owned(),
                messages: request_messages,
                tools: specs,
                temperature: 0.0,
                // A deliberate fix: Python sends none, so a sub-agent gets the
                // provider's default (8192 on Anthropic, the server's own on an
                // OpenAI-compatible endpoint), which no setting but the provider's
                // `max_tokens` changes. It gets the main loop's cap instead (or that
                // of its own model).
                max_tokens: Some(cap),
                stream: false,
            });
            // A long or stalled response stops mid-flight: dropping the future drops
            // the HTTP response.
            let completion = tokio::select! {
                biased;
                () = self.wait_stopped(&ctx) => None,
                completion = request => Some(completion),
            };
            let completion = match completion {
                None => {
                    return Ok((
                        self.subagent_cancelled(role, &sid, depth, run, &ctx),
                        spent + delegated,
                    ))
                }
                Some(Err(error)) => return Err(Box::new((error, spent + delegated))),
                Some(Ok(completion)) => completion,
            };
            breaker.record_step(&completion.usage);
            spent = spent + completion.usage;
            progress.record(breaker.steps(), spent.cost_usd + delegated.cost_usd);
            if let Some(fanout) = &ctx.fanout {
                fanout.add(completion.usage.cost_usd);
            }
            let names: Vec<&str> = completion
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            self.recorder.event(
                &sid,
                "model_call",
                vec![
                    ("agent", json!(role)),
                    ("depth", json!(depth)),
                    ("run", json!(run)),
                    ("step", json!(breaker.steps())),
                    ("model", json!(provider.model())),
                    ("input_tokens", json!(completion.usage.input_tokens)),
                    ("output_tokens", json!(completion.usage.output_tokens)),
                    ("cost_usd", json!(completion.usage.cost_usd)),
                    ("stop_reason", json!(completion.stop_reason)),
                    ("tool_calls", json!(names)),
                ],
            );
            let cut_off = completion.stop_reason.as_deref() == Some("max_tokens");
            let has_text = !completion.text.trim().is_empty();
            // Beyond Python: a cap reached before any text may be raised and the
            // request sent again, the empty reply left out of the history. A
            // specialty on its own model keeps the raise for this run.
            if cut_off && !has_text && completion.tool_calls.is_empty() && parts.is_empty() {
                if let Some(raised) =
                    self.raise_limit(LimitKind::Output, Some(role), &sid, cap, model.own)
                {
                    if model.own {
                        cap_raised = Some(raised);
                    }
                    continue;
                }
            }
            // Beyond Python: a reply that ends cleanly with nothing to say is asked
            // about once, the empty reply left out of the history; a model out of
            // steps is not asked again.
            if completion.tool_calls.is_empty() && !cut_off && !has_text && parts.is_empty() {
                if !completion.thinking.trim().is_empty() {
                    unanswered_thinking = completion.thinking.clone();
                }
                if !nudged && !breaker.out_of_steps() {
                    nudged = true;
                    ask_again = true;
                    self.recorder.event(
                        &sid,
                        "empty_answer",
                        vec![
                            ("agent", json!(role)),
                            ("depth", json!(depth)),
                            ("run", json!(run)),
                            ("step", json!(breaker.steps())),
                            ("model", json!(provider.model())),
                            ("thinking_chars", json!(unanswered_thinking.chars().count())),
                        ],
                    );
                    continue;
                }
            }
            messages.push(Message {
                tool_calls: completion.tool_calls.clone(),
                ..message(Role::Assistant, completion.text.clone())
            });

            if completion.tool_calls.is_empty() {
                // A deliberate fix: Python returns whatever the last call wrote, so an
                // answer cut off at the output cap reaches the parent truncated, or
                // empty when a thinking model spent the whole cap reasoning. A partial
                // answer continues as the main loop's does; a cap reached before any
                // text is reported, so the parent can tell it from an empty answer.
                if cut_off && has_text && continuations < MAX_CONTINUATIONS {
                    continuations += 1;
                    parts.push(completion.text);
                    messages.push(message(Role::User, CONTINUE_PROMPT.to_owned()));
                    self.recorder.event(
                        &sid,
                        "continuation",
                        vec![
                            ("agent", json!(role)),
                            ("depth", json!(depth)),
                            ("run", json!(run)),
                            ("index", json!(continuations)),
                            ("step", json!(breaker.steps())),
                            ("model", json!(provider.model())),
                        ],
                    );
                    continue;
                }
                parts.push(completion.text);
                let mut answer = parts.concat();
                if answer.trim().is_empty() {
                    answer = if cut_off {
                        // The setting to raise: the session's override, else the
                        // one the model it ran on takes its cap from.
                        let setting = if self.session_limits.get(LimitKind::Output).is_some() {
                            self.session_limits.output_setting()
                        } else {
                            model.output_setting.clone()
                        };
                        format!(
                            "[sub-agent '{role}' wrote no answer: its last reply reached the output limit ({} tokens) before any text. Raise {setting} or delegate a smaller task.]",
                            completion.usage.output_tokens,
                        )
                    } else if unanswered_thinking.trim().is_empty() {
                        format!("[sub-agent '{role}' returned no text]")
                    } else {
                        // Said as such, so the parent can weigh it as reasoning.
                        format!(
                            "[sub-agent '{role}' gave no answer; its reasoning follows]\n{}",
                            self.context.secrets.redact(&unanswered_thinking)
                        )
                    };
                }
                self.recorder.event(
                    &sid,
                    "subagent_done",
                    vec![
                        ("role", json!(role)),
                        ("depth", json!(depth)),
                        ("run", json!(run)),
                        ("steps", json!(breaker.steps())),
                        ("cost_usd", json!(spent.cost_usd)),
                    ],
                );
                return Ok((answer, spent + delegated));
            }

            for call in &completion.tool_calls {
                // Between calls, never mid-execution: a tool that started finishes.
                // The sub-agent's history is discarded, so the calls left unrun need
                // no answers; the parent's call gets one.
                if self.stopped(&ctx) {
                    return Ok((
                        self.subagent_cancelled(role, &sid, depth, run, &ctx),
                        spent + delegated,
                    ));
                }
                let (result, usage) = self.run_subagent_tool(&ctx, call, &sid, depth, run).await;
                delegated = delegated + usage;
                progress.record(breaker.steps(), spent.cost_usd + delegated.cost_usd);
                let (content, findings) = self.guardrails.observe(observation(&result));
                if let Some(findings) = findings {
                    self.recorder.event(
                        &sid,
                        "guardrail",
                        vec![
                            ("stage", json!("observation")),
                            ("tool", json!(call.name)),
                            ("findings", json!(findings)),
                        ],
                    );
                }
                messages.push(Message {
                    tool_call_id: Some(call.id.clone()),
                    name: Some(call.name.clone()),
                    ..message(Role::Tool, content)
                });
                if let Err(error) = breaker.record_tool_result(result.ok) {
                    return Ok((halted(error), spent + delegated));
                }
            }
        }
    }

    /// Whether this sub-agent must stop: the turn was cancelled, or its fan-out
    /// tripped.
    fn stopped(&self, ctx: &ToolContext) -> bool {
        self.cancel.is_cancelled()
            || ctx
                .fanout
                .as_ref()
                .is_some_and(|fanout| fanout.cancel.is_cancelled())
    }

    /// Resolves once [`Harness::stopped`] would say so.
    async fn wait_stopped(&self, ctx: &ToolContext) {
        let fanout = async {
            match &ctx.fanout {
                Some(fanout) => fanout.cancel.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = self.cancel.cancelled() => {}
            () = fanout => {}
        }
    }

    /// Close out a sub-agent that was stopped: by the user (it says it was
    /// cancelled, and `subagent_done` carries `status`, as a cancelled turn's
    /// `turn_end` does), or by its fan-out's cost cap (halted, saying why). Neither
    /// counts as a tool error.
    fn subagent_cancelled(
        &self,
        role: &str,
        sid: &str,
        depth: u32,
        run: u64,
        ctx: &ToolContext,
    ) -> String {
        let tripped = (!self.cancel.is_cancelled())
            .then(|| ctx.fanout.as_ref().and_then(|fanout| fanout.tripped()))
            .flatten();
        self.recorder.event(
            sid,
            "subagent_done",
            vec![
                ("role", json!(role)),
                ("depth", json!(depth)),
                ("run", json!(run)),
                (
                    "status",
                    json!(if tripped.is_some() {
                        "halted"
                    } else {
                        "cancelled"
                    }),
                ),
            ],
        );
        match tripped {
            Some(reason) => format!("[sub-agent '{role}' halted: {reason}]"),
            None => format!("[sub-agent '{role}' cancelled]"),
        }
    }

    /// One of a sub-agent's calls: the same interception, approval, and dispatch
    /// as the main loop, one at a time, on the sub-agent's own context. A
    /// delegation from here runs one level deeper and forks this context.
    async fn run_subagent_tool(
        &self,
        ctx: &ToolContext,
        call: &ToolCall,
        sid: &str,
        depth: u32,
        run: u64,
    ) -> (ToolResult, Usage) {
        // Under a card, the call goes by an id of its own, for the surface's sake:
        // the gate, a nested delegation, and the observer all see that id. The
        // sub-agent's history keeps the model's.
        let named;
        let call = match &ctx.card {
            Some(card) => {
                named = ToolCall {
                    id: card.call_id(),
                    ..call.clone()
                };
                self.subagents
                    .observe(&SubagentEvent::ToolCall { card, call: &named });
                &named
            }
            None => call,
        };
        let (result, usage) = self.subagent_tool_result(ctx, call, sid, depth, run).await;
        if let Some(card) = &ctx.card {
            self.subagents.observe(&SubagentEvent::ToolResult {
                card,
                call,
                result: &result,
            });
        }
        (result, usage)
    }

    async fn subagent_tool_result(
        &self,
        ctx: &ToolContext,
        call: &ToolCall,
        sid: &str,
        depth: u32,
        run: u64,
    ) -> (ToolResult, Usage) {
        if let Some(raw) = call.invalid_arguments() {
            return (invalid_arguments_result(call, raw), Usage::default());
        }
        let Some(tool) = self.registry.get(&call.name) else {
            return (self.registry.dispatch(call, ctx), Usage::default());
        };
        // A call outside the specialty's tools gets the registry's refusal, which
        // names the sub-agent, rather than the gate's bare "not executed".
        let (result, usage) = if !ctx.scope_allows(&call.name) {
            (self.registry.dispatch(call, ctx), Usage::default())
        } else {
            let decision = self.gate.decide(tool.as_ref(), call, ctx);
            if !decision.approved {
                let mut result = crate::tools::failure(
                    &call.name,
                    format!("Not executed: {}.", decision.reason),
                );
                result.call_id = call.id.clone();
                return (result, Usage::default());
            }
            match self.delegation(call, ctx) {
                Some(Ok(request)) => self.delegate(call, request, depth + 1, ctx).await,
                Some(Err(invalid)) => (invalid, Usage::default()),
                None => (self.registry.dispatch(call, ctx), Usage::default()),
            }
        };
        self.recorder.event(
            sid,
            "tool_call",
            vec![
                ("tool", json!(call.name)),
                ("ok", json!(result.ok)),
                ("depth", json!(depth)),
                ("run", json!(run)),
            ],
        );
        self.record_repair(call, &result, sid);
        (result, usage)
    }
}

fn list_or_none<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// A project's skill directories inside the real workspace, which an isolated
/// sub-agent must still reach at their real path (the jail drops roots inside the
/// workspace, and the worktree is a different one).
fn project_skill_roots(caller: &ToolContext) -> Vec<PathBuf> {
    let workspace = caller.policy.workspace();
    [
        workspace.join(".agent86").join("skills"),
        workspace.join(".claude").join("skills"),
    ]
    .into_iter()
    .filter(|path| path.is_dir())
    .filter_map(|path| std::fs::canonicalize(path).ok())
    .collect()
}

/// The answer to a `delegate` call from its sub-agent's outcome. A model call that
/// failed inside the sub-agent is the tool's error, as Python's `Tool.run` reports
/// an exception; what it spent before the failure is still the turn's (beyond
/// Python, which loses it).
fn delegate_outcome(call: &ToolCall, outcome: SubagentResult) -> (ToolResult, Usage) {
    let (mut result, usage) = match outcome {
        Ok((text, usage)) => (success(DELEGATE, text), usage),
        Err(failed) => {
            let (error, usage) = *failed;
            (
                crate::tools::failure(DELEGATE, format!("ProviderError: {error}")),
                usage,
            )
        }
    };
    result.call_id = call.id.clone();
    (result, usage)
}
