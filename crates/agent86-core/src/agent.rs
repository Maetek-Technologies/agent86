//! The ReAct loop, ported from Python's `orchestration/loop.py` and
//! `orchestration/circuit.py`.
//!
//! One user turn: call the model, run the tools it asks for, feed the results
//! back, and repeat until it answers without tools. A circuit breaker bounds the
//! turn by steps, cost, wall-clock time, and consecutive tool errors. Tool
//! activity is reported inline as `[tool] ...` lines, exactly as Python streams it.
//!
//! Side-effecting calls pass through the approval gate before they run.
//!
//! Guardrails scan the goal, each answer, and each tool observation.
//!
//! A turn can be cancelled from another thread or task ([`CancelToken`]). It stops at
//! the next safe point: before a model call, while a response streams, and between
//! tool calls, never in the middle of one. The partial exchange is kept.
//!
//! Every request is fitted to the model's context window (`memory::working`).
//! Before a model call, a conversation over budget has its oldest span replaced
//! by a model-written summary (`[limits] compaction = "summarize"`), or dropped.
//!
//! `delegate` runs a sub-agent (`crate::subagent`) inside the batch's sequential
//! part, and what it spends is added to the turn.

use crate::approval::ApprovalGate;
use crate::config::EgressMode;
use crate::config::{AgentsConfig, CompactionMode, LimitsConfig};
use crate::guardrails::Guardrails;
use crate::limits::LimitKind;
use crate::memory::sizing::{
    aged_view, calibrated_budget, result_cap, shorten_middle, sized_reserve,
};
use crate::memory::working::{
    apply_progress, apply_summary, compacted_notice, compaction_failed_notice, compaction_plan,
    count_spec_tokens, count_tokens, fit_pinned, fits, python_strip, summary_request_text,
    trimmed_notice, Cut, Fitted, KEEP_RECENT, SUMMARY_MAX_TOKENS, SUMMARY_RETRY_NOTE,
    SUMMARY_SYSTEM_PROMPT,
};
use crate::observability::Recorder;
use crate::prompt::time_note;
use crate::provider::reasoning::REASONING_LOOP;
use crate::provider::{Provider, ProviderError};
use crate::pyjson::{python_float_repr, python_json_dumps};
use crate::router::Router;
use crate::session::AgentState;
use crate::tools::{ToolContext, ToolRegistry};
use crate::turn::TurnSummary;
use crate::types::{
    python_repr, AgentPhase, CompletionRequest, Message, Role, Step, ToolCall, ToolResult,
    ToolSpec, Usage,
};
use crate::vault::StreamRedactor;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

/// What the harness says to a model that ran out of output tokens mid-answer.
pub const CONTINUE_PROMPT: &str = "Continue exactly where you left off; do not repeat.";

/// How many times one turn may ask for more after hitting the output cap.
pub const MAX_CONTINUATIONS: u64 = 3;

/// Tokens held back in a summary request for the chat template and the
/// estimate's error, beyond Python.
const SUMMARY_MARGIN_TOKENS: u64 = 256;

/// The least transcript a summary request is given, however small the window.
const MIN_TRANSCRIPT_TOKENS: u64 = 1_024;

/// Why [`Harness::write_summary`] wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SummaryError {
    Cancelled,
    /// The provider failed; the text is what the trace records.
    Failed(String),
    /// Both attempts came back empty.
    Empty,
}

/// What [`Harness::write_summary`] produced, and what it spent doing so.
#[derive(Debug, Clone)]
pub(crate) struct WrittenSummary {
    pub outcome: Result<String, SummaryError>,
    pub usage: Usage,
    pub model: String,
}

/// A compaction to announce: the notice, and the summary when one was written.
struct Compaction {
    notice: String,
    summary: Option<String>,
}

/// What a turn keeps about its place in the history while it fits and
/// compacts it, beyond Python.
struct TurnWindow<'g> {
    /// The turn's message as the user sent it, kept verbatim through compactions.
    goal_text: &'g str,
    /// Where the turn's message is in the history.
    goal: usize,
    /// Where a continuation in progress began; compaction never reaches it.
    continuation: Option<usize>,
    /// After a failed summary, the history size at which one is tried again.
    retry_at: Option<u64>,
    /// Whether this turn has said that fitting leaves messages out.
    trim_announced: bool,
    /// How many messages the last request left out.
    trimmed: usize,
    /// How many tool results the last request sent aged (`crate::memory::sizing`).
    aged: usize,
}

/// The user-visible line announcing continuation `index`.
pub fn continuation_notice(index: u64) -> String {
    format!("\n[continuation {index}/{MAX_CONTINUATIONS}]\n")
}

/// What the harness asks, once, of a model whose final reply was empty. Beyond Python.
pub const ANSWER_PROMPT: &str =
    "Your last reply was empty. Give the user your answer now, as plain text.";

/// Said when an empty final reply is sent back for an answer.
pub const EMPTY_ANSWER_NOTICE: &str = "\n[no answer; asking the model again]\n";

/// Said when the model still gave no answer and its reasoning is shown instead.
pub const THINKING_ANSWER_NOTICE: &str = "\n[no answer; showing the model's reasoning]\n";

/// Said when the model still gave no answer and had no reasoning to show.
pub const NO_ANSWER_NOTICE: &str = "\n[the model gave no answer]\n";

/// Said when the provider stopped a reply whose reasoning kept repeating itself.
/// Beyond Python.
pub const REASONING_LOOP_NOTICE: &str =
    "\n[the model's reasoning was repeating itself; stopped it]\n";

/// What a cancelled turn says, as Python's `_cancelled`.
pub const CANCELLED_NOTICE: &str = "\n[cancelled]\n";

/// Asks the in-flight turn to stop at its next safe point, as Python's `Harness.cancel`.
///
/// Clones share one flag, so a signal handler or another task can hold one. Cancelling
/// when no turn is running is harmless: every turn clears the flag on entry.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    pub fn clear(&self) {
        self.inner.flag.store(false, Ordering::SeqCst);
    }

    /// Resolves once [`CancelToken::cancel`] has been called.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            // Registered before the flag is read, so a cancel in between is not missed.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// One thing a running turn reports. Each event carries the text that `run` and the
/// plain REPL stream for it, so every surface sees the same output.
///
/// `#[non_exhaustive]` (since 1.2.0, ADR 0004): later releases add kinds of event,
/// so a `match` outside this crate needs a `_` arm. [`TurnEvent::text`] is what any
/// event adds to the turn's output, whatever its kind.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum TurnEvent<'a> {
    /// The model's text (or the ingress guardrail's refusal, which stands in for it).
    Text(&'a str),
    /// A notice from the harness (`[cancelled]`, `[guardrail] ...`, a continuation or
    /// compaction note): part of the output, but never part of the answer, so `run`
    /// can keep it off stdout.
    Notice(&'a str),
    /// A piece of the model's reasoning, already redacted. Not part of the turn's
    /// output: only the interface shows it.
    Thinking(&'a str),
    /// A tool call, announced before its batch runs; `text` is its `[tool] name(...)` line.
    ToolCall { call: &'a ToolCall, text: &'a str },
    /// A call's result, in call order. `observation` is what the model is shown, after
    /// the observation guardrail; `text` is the `[tool] name -> summary` line.
    ToolResult {
        call: &'a ToolCall,
        result: &'a ToolResult,
        observation: &'a str,
        text: &'a str,
    },
    /// Beyond Python: a model call finished. `step` is what it used; `session` is
    /// the session's usage with it added (sub-agents' spend joins when their batch
    /// ends). Adds nothing to the turn's output.
    Usage {
        step: &'a crate::types::Usage,
        session: &'a crate::types::Usage,
    },
    /// The history was compacted: `notice` is what the other surfaces stream (a
    /// notice, as before), and `summary` the summary written, `None` when it could
    /// not be and the history was trimmed instead (beyond Python: an editor shows it
    /// as ACP's `compaction_update`).
    Compacted {
        notice: &'a str,
        summary: Option<&'a str>,
    },
    /// Beyond Python (ADR 0005): one A2UI message a `render_ui` call drew,
    /// validated, for the surface to draw. Sent after the call's `ToolCall` and
    /// before its `ToolResult`, in order. Adds nothing to the turn's output.
    #[cfg(feature = "agent86ui")]
    Ui(&'a agent86_ui::AgentMessage),
}

impl<'a> TurnEvent<'a> {
    /// The text this event adds to the turn's output.
    pub fn text(&self) -> &'a str {
        match *self {
            Self::Text(text)
            | Self::Notice(text)
            | Self::ToolCall { text, .. }
            | Self::ToolResult { text, .. }
            | Self::Compacted { notice: text, .. } => text,
            Self::Thinking(_) | Self::Usage { .. } => "",
            #[cfg(feature = "agent86ui")]
            Self::Ui(_) => "",
        }
    }
}

/// A turn could not be completed.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum HarnessError {
    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error("Circuit tripped: {0}")]
    CircuitTripped(String),

    #[error(
        "The '{provider}' provider ended its stream for model {} without a final completion. \
         The response was truncated; retry, or check the endpoint's streaming implementation.",
        python_repr(model)
    )]
    NoFinalCompletion { provider: String, model: String },
}

impl HarnessError {
    /// The name of the exception Python raises for this failure, which its
    /// full-screen interface shows (`error: ProviderError: ...`).
    pub fn python_type(&self) -> &'static str {
        match self {
            Self::Provider(_) => "ProviderError",
            Self::CircuitTripped(_) | Self::NoFinalCompletion { .. } => "HarnessError",
        }
    }
}

/// Bounds one turn: steps, cumulative cost, wall-clock time, and the tool-error streak.
pub struct CircuitBreaker {
    limits: LimitsConfig,
    steps: u32,
    cost_usd: f64,
    consecutive_errors: u32,
    started: Instant,
}

impl CircuitBreaker {
    pub fn new(limits: &LimitsConfig) -> Self {
        Self {
            limits: limits.clone(),
            steps: 0,
            cost_usd: 0.0,
            consecutive_errors: 0,
            started: Instant::now(),
        }
    }

    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// The step budget.
    pub fn max_steps(&self) -> u32 {
        self.limits.max_steps
    }

    /// Raise (or lower) the step budget mid-run, when the user raises it.
    pub fn set_max_steps(&mut self, max_steps: u32) {
        self.limits.max_steps = max_steps;
    }

    /// The step budget is spent, so the next [`CircuitBreaker::before_step`] trips.
    pub fn out_of_steps(&self) -> bool {
        self.steps >= self.limits.max_steps
    }

    /// Fail if starting another model call would breach a bound.
    pub fn before_step(&self) -> Result<(), HarnessError> {
        let limits = &self.limits;
        if self.steps >= limits.max_steps {
            return Err(trip(format!(
                "step budget reached ({} steps)",
                limits.max_steps
            )));
        }
        if limits.max_cost_usd > 0.0 && self.cost_usd >= limits.max_cost_usd {
            return Err(trip(format!(
                "cost cap reached (${:.4} >= ${})",
                self.cost_usd,
                python_float_repr(limits.max_cost_usd)
            )));
        }
        let wall_clock = Duration::from_secs(limits.max_wall_clock_s);
        if limits.max_wall_clock_s > 0 && self.started.elapsed() >= wall_clock {
            return Err(trip(format!(
                "wall-clock limit reached ({}s)",
                limits.max_wall_clock_s
            )));
        }
        Ok(())
    }

    pub fn record_step(&mut self, usage: &Usage) {
        self.steps += 1;
        self.cost_usd += usage.cost_usd;
    }

    /// Count spending that is not a step, such as a compaction summary.
    /// What the turn has spent so far.
    pub fn cost(&self) -> f64 {
        self.cost_usd
    }

    pub fn record_cost(&mut self, cost_usd: f64) {
        self.cost_usd += cost_usd;
    }

    pub fn record_tool_result(&mut self, ok: bool) -> Result<(), HarnessError> {
        if ok {
            self.consecutive_errors = 0;
            return Ok(());
        }
        self.consecutive_errors += 1;
        if self.consecutive_errors > self.limits.max_consecutive_errors {
            return Err(trip(format!(
                "aborted after {} consecutive tool errors",
                self.consecutive_errors
            )));
        }
        Ok(())
    }
}

fn trip(reason: String) -> HarnessError {
    HarnessError::CircuitTripped(reason)
}

/// Everything one turn needs.
pub struct Harness {
    /// The session's model; the REPL's `/model` replaces it between turns. With
    /// triage routing on, each turn may use another (see [`Harness::active_provider`]).
    pub provider: Arc<dyn Provider>,
    /// Triage routing (`crate::router`); [`Router::default`] never routes.
    pub router: Router,
    /// OpenTelemetry spans (`crate::otel`); [`crate::otel::Tracer::default`] records none.
    pub tracer: crate::otel::Tracer,
    pub registry: ToolRegistry,
    pub context: ToolContext,
    pub limits: LimitsConfig,
    /// `[agents]`: whether `delegate` runs sub-agents, how deep, and how long.
    pub agents: AgentsConfig,
    pub system: Message,
    /// The configured output cap, as Python's `max_output_tokens_for`; a session
    /// override may replace it (see [`Harness::output_cap`]).
    pub max_tokens: u32,
    /// Limits overridden for the session, and the hook that offers to raise one
    /// when a turn reaches it (`crate::limits`).
    pub session_limits: crate::limits::SessionLimits,
    /// Tokens of context the model can hold (`context_window_for`); the REPL's
    /// `/model` updates it with the provider.
    pub context_window: u64,
    /// Decides whether side-effecting calls may run.
    pub gate: ApprovalGate,
    /// Ingress, egress, and observation scanning.
    pub guardrails: Guardrails,
    /// The flight recorder; [`Recorder::disabled`] writes nothing.
    pub recorder: Recorder,
    /// Live MCP servers; their section of the system message is rebuilt for every
    /// request, because servers drop, reconnect, and change their tools mid-turn.
    pub mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    /// Long-term memory: past episodes are recalled into each turn's system
    /// message and every finished turn is recorded; `None` runs statelessly.
    pub memory: Option<crate::session::SessionStore>,
    /// Stops the running turn at its next safe point.
    pub cancel: CancelToken,
    /// Specialist sub-agents' state (beyond Python): notices for the user and the
    /// approvals of project specialties (`crate::subagent`).
    pub subagents: crate::subagent::SubagentState,
    /// Reported against estimated input tokens, per model (beyond Python;
    /// `crate::memory::sizing::Calibration`).
    pub calibration: crate::memory::sizing::Calibration,
    /// The verification gate (beyond Python; `crate::verify`): off by default.
    pub verify: crate::verify::Verify,
}

impl Harness {
    /// The system message for the next request: the prompt plus, when MCP servers
    /// are live, their section, then the turn's recall note, as Python's
    /// `_system_content`. Beyond Python, the names of the secrets the user entered
    /// come before the recall note, when there are any.
    pub(crate) fn system_message(&self, extra: Option<&str>) -> Message {
        let offered = self.delegation_offered(&self.context, 0);
        // Beyond Python: under `run --agent`, the specialty's filtered prompt.
        if let Some((persona, preloaded, _)) = self.subagents.main() {
            return self.filtered_system(&persona, &self.context, &preloaded, offered, extra);
        }
        self.system_for(&self.context, offered, extra)
    }

    /// [`Harness::system_message`] for `ctx`, an unrestricted context (the main
    /// loop's, or a plain sub-agent's fork of it). Beyond Python, when `delegate`
    /// is offered, the specialist sub-agents are listed after the MCP section.
    pub(crate) fn system_for(
        &self,
        ctx: &ToolContext,
        delegation_offered: bool,
        extra: Option<&str>,
    ) -> Message {
        let mut system = self.system.clone();
        if let Some(section) = self
            .mcp
            .as_ref()
            .map(|mcp| mcp.prompt_section())
            .filter(|section| !section.is_empty())
        {
            system.content = format!("{}\n\n{section}", system.content);
        }
        if delegation_offered {
            if let Some(section) = self.specialty_listing(ctx) {
                system.content = format!("{}\n\n{section}", system.content);
            }
        }
        if let Some(section) = ctx.secrets.prompt_section() {
            system.content = format!("{}\n\n{section}", system.content);
        }
        // Beyond Python (ADR 0006): how to ask the user, wherever someone can
        // answer and the context may ask.
        if crate::ask::reachable(ctx) && ctx.allowed_tool(crate::ask::ASK_USER) {
            #[cfg(feature = "agent86ui")]
            let drawing = ctx.ui.is_some() && ctx.allowed_tool(crate::agent86ui::RENDER_UI);
            #[cfg(not(feature = "agent86ui"))]
            let drawing = false;
            system.content = format!(
                "{}\n\n{}",
                system.content,
                crate::ask::prompt_section(drawing)
            );
        }
        // Beyond Python: agent86UI's section, only where a renderer is and the
        // context may draw.
        #[cfg(feature = "agent86ui")]
        if let Some(ui) = ctx.ui.as_deref() {
            if ctx.allowed_tool(crate::agent86ui::RENDER_UI) {
                system.content = format!("{}\n\n{}", system.content, ui.prompt_section());
            }
        }
        if let Some(extra) = extra.filter(|extra| !extra.is_empty()) {
            system.content = format!("{}\n\n{extra}", system.content);
        }
        system
    }

    /// Conversation tokens available for this request, as Python's
    /// `_context_budget`: the window minus what the request spends before any
    /// history (the system message and the tool catalogue, both of which change at
    /// runtime), minus the room the response needs.
    ///
    /// Beyond Python, the budget shrinks for a model whose reported input tokens
    /// have run above the estimate (`crate::memory::sizing::Calibration`).
    pub(crate) fn context_budget(&self, system: &Message, specs: &[ToolSpec]) -> u64 {
        self.context_budget_with(
            system,
            specs,
            self.active_context_window(),
            self.output_cap(),
            self.active_provider().model(),
        )
    }

    /// A tool result cut to its share of `budget`, unless `[limits] context_sizing`
    /// is off (`crate::memory::sizing::result_cap`).
    pub(crate) fn cap_result(&self, content: String, budget: u64) -> String {
        if !self.limits.context_sizing {
            return content;
        }
        shorten_middle(
            &content,
            result_cap(budget, self.context.policy.max_output_chars),
        )
    }

    /// `messages` as a request carries them: aged once the window fills, unless
    /// `[limits] context_sizing` is off (`crate::memory::sizing::aged_view`).
    pub(crate) fn request_view(
        &self,
        messages: &[Message],
        budget: u64,
    ) -> crate::memory::sizing::Aged {
        if self.limits.context_sizing {
            aged_view(messages, budget)
        } else {
            crate::memory::sizing::Aged {
                messages: messages.to_vec(),
                aged: 0,
            }
        }
    }

    /// [`Harness::context_budget`] for `model` with this window and output cap (a
    /// sub-agent, perhaps on a model of its own). Beyond Python, with
    /// `[limits] context_sizing` on, it is calibrated for the model
    /// (`crate::memory::sizing::calibrated_budget`) and the reserve is held to a
    /// share of the window (`sized_reserve`; the output cap already is).
    pub(crate) fn context_budget_with(
        &self,
        system: &Message,
        specs: &[ToolSpec],
        window: u64,
        output_cap: u32,
        model: &str,
    ) -> u64 {
        let overhead = count_tokens(std::slice::from_ref(system)) + count_spec_tokens(specs);
        let (ratio, reserve) = if self.limits.context_sizing {
            (
                self.calibration.ratio(model),
                sized_reserve(self.limits.context_reserve_tokens, window),
            )
        } else {
            (1.0, self.limits.context_reserve_tokens)
        };
        calibrated_budget(
            window,
            overhead,
            reserve,
            u64::from(output_cap),
            self.limits.max_context_tokens,
            ratio,
        )
    }

    /// The tools a main-loop request offers: the registry's, narrowed under
    /// `run --agent` to what the specialty may call.
    fn request_tools(&self) -> Vec<ToolSpec> {
        let mut tools = self.registry.specs();
        tools.retain(|spec| self.context.scope_allows(&spec.name));
        tools
    }

    /// Write a summary of `messages` with the compaction summarizer (its prompt,
    /// the cheap route when routing is on), as Python's summary call, with two
    /// things beyond Python: the transcript is cut to fit the summarizer's own
    /// window ([`fit_transcript`](crate::memory::working::fit_transcript)), and an
    /// empty reply (a thinking model can spend the whole cap reasoning) is asked
    /// for once more, with a larger cap and [`SUMMARY_RETRY_NOTE`]. The usage of
    /// every attempt is returned, also on failure, since it was spent.
    pub(crate) async fn write_summary(&self, messages: &[Message]) -> WrittenSummary {
        let (summarizer, window) = self.summary_model();
        let model = summarizer.model().to_owned();
        let first_cap = SUMMARY_MAX_TOKENS * 2;
        // Four times the first cap, but no more than a quarter of the window, so the
        // retry still leaves the transcript room; never less than the first cap
        // unless that is over half the window.
        let share = |divisor: u64| u32::try_from(window / divisor).unwrap_or(u32::MAX);
        let retry_cap = (SUMMARY_MAX_TOKENS * 8)
            .min(share(4))
            .max(first_cap.min(share(2)));
        let instructions = message(Role::System, SUMMARY_SYSTEM_PROMPT);
        let overhead = count_tokens(std::slice::from_ref(&instructions))
            + count_tokens(&[message(Role::User, SUMMARY_RETRY_NOTE)])
            + SUMMARY_MARGIN_TOKENS;
        let room = window
            .saturating_sub(overhead + u64::from(retry_cap))
            .max(MIN_TRANSCRIPT_TOKENS);
        let transcript =
            summary_request_text(messages, usize::try_from(room * 4).unwrap_or(usize::MAX));
        let mut usage = Usage::default();
        let mut attempt = 0;
        loop {
            let (text, cap) = if attempt == 0 {
                (transcript.clone(), first_cap)
            } else {
                (format!("{transcript}\n\n{SUMMARY_RETRY_NOTE}"), retry_cap)
            };
            let request = CompletionRequest {
                model: model.clone(),
                messages: vec![instructions.clone(), message(Role::User, &text)],
                tools: Vec::new(),
                temperature: 0.0,
                // A summary is a bounded artefact; it should not cost more than the span.
                max_tokens: Some(cap),
                stream: false,
            };
            let completion = tokio::select! {
                biased;
                // The next safe point sees the cancel and closes the turn out.
                () = self.cancel.cancelled() => {
                    return WrittenSummary { outcome: Err(SummaryError::Cancelled), usage, model };
                }
                completion = summarizer.complete(request) => completion,
            };
            let completion = match completion {
                Ok(completion) => completion,
                Err(error) => {
                    return WrittenSummary {
                        outcome: Err(SummaryError::Failed(format!("ProviderError: {error}"))),
                        usage,
                        model,
                    }
                }
            };
            usage = usage + completion.usage;
            let text = python_strip(&completion.text).to_owned();
            if !text.is_empty() {
                return WrittenSummary {
                    outcome: Ok(text),
                    usage,
                    model,
                };
            }
            if attempt > 0 {
                return WrittenSummary {
                    outcome: Err(SummaryError::Empty),
                    usage,
                    model,
                };
            }
            attempt += 1;
        }
    }

    /// Replace the oldest over-budget span with a summary, in place, before a model
    /// call, as Python's `_compact_if_needed`. Dropping the oldest turns (still
    /// available as `compaction = "drop"`) makes a long session forget its own
    /// goal; a summary keeps the goal, the decisions, and the paths found.
    ///
    /// Beyond Python, the span may reach inside the current turn
    /// ([`compaction_plan`]): the turn's message is kept verbatim with the summary
    /// appended, so a long single turn compacts too instead of losing its goal to
    /// fitting. A failed or empty summary is not tried again until the history has
    /// grown by a quarter of the budget, so a summarizer that cannot answer is not
    /// asked every step.
    ///
    /// Never fails the turn: a summary that cannot be written falls back to the drop
    /// that fitting the request does anyway, and says so.
    async fn compact_if_needed(
        &self,
        state: &mut AgentState,
        extra_system: Option<&str>,
        summary: &mut TurnSummary,
        breaker: &mut CircuitBreaker,
        window: &mut TurnWindow<'_>,
    ) -> Option<Compaction> {
        if self.limits.compaction != CompactionMode::Summarize {
            return None;
        }
        let system = self.system_message(extra_system);
        let budget = self.context_budget(&system, &self.request_tools());
        // Beyond Python: judged on what a request would carry, so a history that
        // fits once its older results are aged costs no summary.
        if fits(&self.request_view(&state.messages, budget).messages, budget) {
            return None;
        }
        let tokens = count_tokens(&state.messages);
        if window.retry_at.is_some_and(|at| tokens < at) {
            return None;
        }
        // Everything left is recent or the current turn's own message and tool
        // blocks: nothing may be compacted, so fitting trims and the provider's
        // limits take over.
        let cut = compaction_plan(
            &state.messages,
            budget,
            KEEP_RECENT,
            window.goal,
            window.continuation,
        )?;
        let sid = state.session_id.clone();
        let end = cut.cut();
        let replaced = cut.replaced();
        let prefix = &state.messages[..end];
        let dropped_tokens = count_tokens(prefix);
        let written = self.write_summary(prefix).await;
        summary.add_usage(&written.usage);
        breaker.record_cost(written.usage.cost_usd);
        let failed = |window: &mut TurnWindow<'_>, status: &str, error: Option<String>| {
            window.retry_at = Some(tokens + (budget / 4).max(1));
            let mut fields = vec![
                ("status", json!(status)),
                ("fallback", json!("drop")),
                ("dropped", json!(replaced)),
            ];
            if let Some(error) = error {
                fields.push(("error", json!(error)));
            }
            self.recorder.event(&sid, "compaction", fields);
            Some(Compaction {
                notice: compaction_failed_notice(replaced),
                summary: None,
            })
        };
        let text = match written.outcome {
            Ok(text) => text,
            Err(SummaryError::Cancelled) => return None,
            Err(SummaryError::Failed(error)) => return failed(window, "failed", Some(error)),
            Err(SummaryError::Empty) => return failed(window, "empty", None),
        };
        window.retry_at = None;

        // The originals are archived before they leave the live conversation.
        if let Some(memory) = &self.memory {
            let _ = memory.record_compaction(&sid, &text, prefix);
        }
        let before = state.messages.len();
        let kept = state.messages.split_off(end);
        state.messages = match cut {
            Cut::Earlier(_) => apply_summary(&text, kept),
            Cut::WithinTurn { .. } => apply_progress(window.goal_text, &text, kept),
        };
        let removed = before - state.messages.len();
        // Indices into the history move with it. (Python keeps the old ones, which
        // go stale after a compaction mid-turn.)
        window.goal = match cut {
            Cut::Earlier(_) => window.goal - removed,
            Cut::WithinTurn { .. } => 0,
        };
        if let Some(start) = window.continuation.as_mut() {
            *start -= removed;
        }
        summary.compactions += 1;
        let mut fields = vec![
            ("status", json!("ok")),
            ("dropped", json!(replaced)),
            ("dropped_tokens", json!(dropped_tokens)),
            (
                "summary_tokens",
                json!(count_tokens(&[message(Role::User, &text)])),
            ),
            ("kept", json!(state.messages.len())),
            ("model", json!(written.model)),
        ];
        if matches!(cut, Cut::WithinTurn { .. }) {
            fields.push(("scope", json!("turn")));
        }
        self.recorder.event(&sid, "compaction", fields);
        // Saved at once: a resume must see the compacted history, not a stale copy
        // that would re-inflate the context on the next turn.
        if let Some(memory) = &self.memory {
            let _ = memory.persist(state);
        }
        Some(Compaction {
            notice: compacted_notice(replaced),
            summary: Some(text),
        })
    }

    /// Run one user turn on `state`, passing every piece of streamed text,
    /// including tool notices, to `on_text`, and return everything streamed.
    ///
    /// The turn's messages and steps are appended to `state`, its usage added,
    /// and its summary left on `state.last_turn`. A failed turn still leaves
    /// `state` in the `error` phase with its summary, so it can be persisted.
    pub async fn run_turn(
        &self,
        state: &mut AgentState,
        goal: &str,
        on_text: impl FnMut(&str),
    ) -> Result<String, HarnessError> {
        self.run_turn_as(state, goal, goal, on_text).await
    }

    /// [`Harness::run_turn`] where the model sees `user_text` (for a prompt with
    /// `@file` mentions, the expanded text) and the trace records `display_text`,
    /// what the user typed, as Python's `display_text` argument.
    pub async fn run_turn_as(
        &self,
        state: &mut AgentState,
        user_text: &str,
        display_text: &str,
        mut on_text: impl FnMut(&str),
    ) -> Result<String, HarnessError> {
        self.run_turn_events(state, user_text, display_text, |event| {
            on_text(event.text())
        })
        .await
    }

    /// [`Harness::run_turn_as`] reporting [`TurnEvent`]s, so an interface can show
    /// tool calls without parsing the `[tool] ...` lines. Every event carries the text
    /// the other entry points stream, in the same order.
    pub async fn run_turn_events(
        &self,
        state: &mut AgentState,
        user_text: &str,
        display_text: &str,
        on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<String, HarnessError> {
        self.run_turn_with(state, user_text, display_text, None, on_event)
            .await
    }

    /// A turn with `skill` chosen by the user (`/<name> [task]`, `/skill <name>`,
    /// `run --skill`), beyond Python: the model gets the skill's instructions ahead
    /// of `task` in the user's message ([`crate::skills::skill_request`]), and the
    /// skill is active from the first step, so its `allowed-tools` apply at once.
    /// Everything else is [`Harness::run_turn_events`]; `display_text` is what the
    /// user typed, for the trace.
    pub async fn run_skill_turn(
        &self,
        state: &mut AgentState,
        skill: &Arc<crate::skills::Skill>,
        task: &str,
        display_text: &str,
        on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<String, HarnessError> {
        let user_text = crate::skills::skill_request(skill, task);
        self.run_turn_with(state, &user_text, display_text, Some(skill), on_event)
            .await
    }

    async fn run_turn_with(
        &self,
        state: &mut AgentState,
        user_text: &str,
        display_text: &str,
        skill: Option<&Arc<crate::skills::Skill>>,
        on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<String, HarnessError> {
        // A cancel requested while no turn was running must not kill this one.
        self.cancel.clear();
        // A skill activated with `use_skill` restricts tools for its turn only: the
        // restriction is lifted on entry and on every exit, so it can never keep
        // refusing tools for the rest of the session. A skill the user ran is then
        // made active for this turn alone.
        self.context.clear_skill();
        if let Some(skill) = skill {
            self.context.activate_skill(skill);
        }
        let started = Instant::now();
        // One `turn` span per user turn, parent of its model and tool spans.
        let mut span = self.tracer.start(
            "turn",
            vec![
                ("session.id", json!(state.session_id)),
                (
                    "gen_ai.request.model",
                    json!(self.active_provider().model()),
                ),
            ],
        );
        let mut summary = TurnSummary::default();
        // A secret the user pasted anyway (or an `@file` that holds one) goes no
        // further than this: not to the model, the history, nor the trace.
        let secrets = &self.context.secrets;
        let (user_text, display_text) = (secrets.redact(user_text), secrets.redact(display_text));
        let result = self
            .turn(state, &user_text, &display_text, &mut summary, on_event)
            .await;
        self.context.clear_skill();
        summary.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        state.last_turn = Some(summary);
        if let Err(error) = &result {
            state.phase = AgentPhase::Error;
            let reason = match error {
                HarnessError::CircuitTripped(detail) => format!("circuit tripped: {detail}"),
                HarnessError::Provider(detail) => {
                    format!("provider stream failed: ProviderError: {detail}")
                }
                HarnessError::NoFinalCompletion { .. } => {
                    "provider stream ended without a final completion".to_owned()
                }
            };
            self.recorder.event(
                &state.session_id,
                "turn_end",
                vec![("status", json!("error")), ("reason", json!(reason))],
            );
        }
        // Closed with what the turn spent, on every exit, so a backend sees the cost
        // of an aborted turn too.
        span.set(turn_attributes(state, self.active_provider().model()));
        if let Err(error) = &result {
            span.fail(error.python_type(), &error.to_string());
        }
        self.tracer.end(span);
        result
    }

    async fn turn(
        &self,
        state: &mut AgentState,
        goal: &str,
        display_text: &str,
        summary: &mut TurnSummary,
        mut on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<String, HarnessError> {
        let mut output = String::new();
        let mut emit = |event: TurnEvent<'_>| {
            output.push_str(event.text());
            on_event(event);
        };

        let sid = state.session_id.clone();
        self.recorder
            .event(&sid, "turn_start", vec![("task", json!(display_text))]);
        self.verify.set_last(None);

        // Ingress guardrail on the user's input.
        let report = self.guardrails.inspect_ingress(goal);
        if report.flagged() {
            self.recorder.event(
                &sid,
                "guardrail",
                vec![
                    ("stage", json!("ingress_input")),
                    ("findings", json!(report.summary())),
                ],
            );
            if self.guardrails.should_block(&report) {
                let refusal = format!(
                    "Refused: input tripped the ingress guardrail ({}).",
                    report.summary()
                );
                state.messages.push(message(Role::User, goal));
                state.messages.push(message(Role::Assistant, &refusal));
                state.phase = AgentPhase::Error;
                self.recorder
                    .event(&sid, "turn_end", vec![("status", json!("blocked"))]);
                emit(TurnEvent::Text(&refusal));
                return Ok(output);
            }
            emit(TurnEvent::Notice(&format!(
                "[guardrail] input flagged: {}\n",
                report.summary()
            )));
        }

        // Redact mode cannot stream: text inspected only after it was shown has
        // already leaked. Each step's text is held back and replayed redacted.
        let buffering = self.guardrails.egress == EgressMode::Redact;

        // Pick this turn's model (triage routing), after the input was let through.
        self.route(goal, &sid)?;
        let provider = self.active_provider();

        // What happened last time a task like this came up.
        let recall_note = self
            .memory
            .as_ref()
            .and_then(|memory| memory.recall_note(goal));
        // Beyond Python: the time the turn started, then what was recalled.
        let time = time_note(&jiff::ZonedDateTime::now());
        let recall_note = Some(match recall_note {
            Some(recalled) => format!("{time}\n\n{recalled}"),
            None => time,
        });
        // Where this turn begins. Its message is never summarized away nor left out
        // of a request (beyond Python, whose fitting drops it first in a long turn);
        // compaction may reach past it, keeping it verbatim.
        let mut window = TurnWindow {
            goal_text: goal,
            goal: state.messages.len(),
            continuation: None,
            retry_at: None,
            trim_announced: false,
            trimmed: 0,
            aged: 0,
        };
        state.messages.push(message(Role::User, goal));
        let mut limits = self.limits.clone();
        limits.max_steps = self.max_steps();
        let mut breaker = CircuitBreaker::new(&limits);
        let mut broken_streak = BrokenStreak::default();
        // Where the first partial answer went, and the pieces collected so far.
        let mut continuation_start: Option<usize> = None;
        let mut continuation_parts: Vec<String> = Vec::new();
        // Beyond Python: an empty final reply is asked about once (`ask_again` puts
        // ANSWER_PROMPT on the next request only, never in the history), and the
        // reasoning it came with is kept in case the second try is empty too.
        let mut nudged = false;
        let mut ask_again = false;
        let mut unanswered_thinking = String::new();
        // Beyond Python: the verification gate's progress (`crate::verify`).
        let mut verify_run = crate::verify::VerifyRun::default();
        // Beyond Python: one reminder per way of going in circles (`crate::stall`).
        let mut stall = crate::stall::StallWatch::new(self.limits.stall_nudge);

        loop {
            // Never start another model call once cancelled: that is the expensive,
            // long-latency step the user is trying to escape.
            if self.cancel.is_cancelled() {
                self.cancelled(state, breaker.steps(), &mut emit);
                return Ok(output);
            }
            // Beyond Python: the interface may offer to raise a spent budget.
            if breaker.out_of_steps() {
                match self.raise_limit(LimitKind::Steps, None, &sid, breaker.max_steps(), false) {
                    Some(raised) => breaker.set_max_steps(raised),
                    // Ctrl+C at the offer stops it and cancels the turn.
                    None if self.cancel.is_cancelled() => {
                        self.cancelled(state, breaker.steps(), &mut emit);
                        return Ok(output);
                    }
                    None => {}
                }
            }
            breaker.before_step()?;

            // Compact before the request is built, so the model call sees the
            // compacted history. Once per step, never nested.
            window.continuation = continuation_start;
            if let Some(compaction) = self
                .compact_if_needed(
                    state,
                    recall_note.as_deref(),
                    summary,
                    &mut breaker,
                    &mut window,
                )
                .await
            {
                continuation_start = window.continuation;
                // A failed summary says what fitting drops; a written one starts
                // the count again.
                window.trim_announced = compaction.summary.is_none();
                // Said out loud: a conversation that quietly lost its oldest turns
                // looks, from the user's seat, like a model that forgot.
                emit(TurnEvent::Compacted {
                    notice: &compaction.notice,
                    summary: compaction.summary.as_deref(),
                });
            }

            // The request's estimated size, for calibration, and the budget the
            // step's results are capped against.
            let estimated;
            let step_budget;
            let mut completion = {
                let system = self.system_message(recall_note.as_deref());
                // Under `run --agent`, only what the specialty may call is offered.
                let tools = self.request_tools();
                let budget = self.context_budget(&system, &tools);
                step_budget = budget;
                // Beyond Python: older results go as stubs once the window fills.
                let aged = self.request_view(&state.messages, budget);
                self.announce_age(&sid, aged.aged, budget, &mut window);
                let fitted = fit_pinned(&aged.messages, budget, Some(window.goal));
                self.announce_trim(&sid, &fitted, budget, &mut window, &mut emit);
                let mut request_messages = Vec::with_capacity(fitted.messages.len() + 1);
                request_messages.push(system);
                request_messages.extend(fitted.messages);
                if std::mem::take(&mut ask_again) {
                    request_messages.push(message(Role::User, ANSWER_PROMPT));
                }
                estimated = count_tokens(&request_messages) + count_spec_tokens(&tools);
                let request = CompletionRequest {
                    model: provider.model().to_owned(),
                    messages: request_messages,
                    tools,
                    // Beyond Python: `run --agent`'s specialty, else the section's.
                    temperature: request_temperature(
                        provider.as_ref(),
                        self.subagents.main().and_then(|main| main.2),
                    ),
                    max_tokens: Some(self.output_cap()),
                    stream: true,
                };
                // GenAI semantic conventions where a name exists, `agent86.*` otherwise.
                let mut span = self.tracer.start(
                    "model_call",
                    vec![
                        ("gen_ai.system", json!(provider.name())),
                        ("gen_ai.request.model", json!(provider.model())),
                        ("agent86.step", json!(breaker.steps() + 1)),
                    ],
                );
                let mut stream = provider.stream(request);
                let mut completion = None;
                let mut buffered = String::new();
                // The model never saw a secret's value, but should it produce one
                // anyway, it is not shown.
                let mut redactor = StreamRedactor::default();
                let mut thinking_redactor = StreamRedactor::default();
                loop {
                    // A long or stalled response stops mid-flight, not after it ends.
                    // Dropping the stream drops the HTTP response.
                    let next = tokio::select! {
                        biased;
                        () = self.cancel.cancelled() => None,
                        delta = stream.next() => Some(delta),
                    };
                    let Some(Some(delta)) = next else { break };
                    let delta = match delta {
                        Ok(delta) => delta,
                        Err(error) => {
                            span.set(model_call_attributes(None));
                            span.fail("ProviderError", &error.to_string());
                            self.tracer.end(span);
                            return Err(error.into());
                        }
                    };
                    // Reasoning is shown only as it streams. Redact mode holds text
                    // back until the guardrail has seen it, so it shows none.
                    if !delta.thinking.is_empty() && !buffering {
                        let shown = thinking_redactor.push(&self.context.secrets, &delta.thinking);
                        if !shown.is_empty() {
                            emit(TurnEvent::Thinking(&shown));
                        }
                    }
                    if !delta.text.is_empty() {
                        if buffering {
                            buffered.push_str(&delta.text);
                        } else {
                            let shown = redactor.push(&self.context.secrets, &delta.text);
                            if !shown.is_empty() {
                                emit(TurnEvent::Text(&shown));
                            }
                        }
                    }
                    if delta.done && delta.completion.is_some() {
                        completion = delta.completion;
                    }
                }
                drop(stream);
                let held = thinking_redactor.finish(&self.context.secrets);
                if !held.is_empty() {
                    emit(TurnEvent::Thinking(&held));
                }
                let held = redactor.finish(&self.context.secrets);
                if !held.is_empty() {
                    emit(TurnEvent::Text(&held));
                }
                span.set(model_call_attributes(completion.as_ref()));
                self.tracer.end(span);
                if self.cancel.is_cancelled() {
                    // Whatever was held back still belongs to the user: redacted, then shown.
                    let partial = self
                        .guardrails
                        .inspect_egress(&self.context.secrets.redact(&buffered))
                        .1;
                    if !partial.is_empty() {
                        emit(TurnEvent::Text(&partial));
                    }
                    self.cancelled(state, breaker.steps(), &mut emit);
                    return Ok(output);
                }
                completion.ok_or_else(|| HarnessError::NoFinalCompletion {
                    provider: provider.name().to_owned(),
                    model: provider.model().to_owned(),
                })?
            };

            breaker.record_step(&completion.usage);
            if self.limits.context_sizing {
                self.calibration.observe(
                    provider.model(),
                    estimated,
                    completion.usage.input_tokens,
                );
            }
            summary.steps = u64::from(breaker.steps());
            summary.add_usage(&completion.usage);
            // Beyond Python: what this call used, and the session so far, for a
            // surface that shows it as it goes (an editor's `usage_update`).
            emit(TurnEvent::Usage {
                step: &completion.usage,
                session: &(state.usage + completion.usage),
            });
            let names: Vec<&str> = completion
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            self.recorder.event(
                &sid,
                "model_call",
                vec![
                    ("step", json!(breaker.steps())),
                    ("model", json!(provider.model())),
                    ("input_tokens", json!(completion.usage.input_tokens)),
                    ("output_tokens", json!(completion.usage.output_tokens)),
                    ("cost_usd", json!(completion.usage.cost_usd)),
                    ("stop_reason", json!(completion.stop_reason)),
                    ("tool_calls", json!(names)),
                ],
            );

            if completion.stop_reason.as_deref() == Some(REASONING_LOOP) {
                emit(TurnEvent::Notice(REASONING_LOOP_NOTICE));
            }
            // Beyond Python: the provider ended the reply itself (Gemini's SAFETY,
            // RECITATION, ...; an OpenAI-compatible content filter; a model's
            // refusal). Said once, and not asked about again.
            let stopped = completion
                .stop_reason
                .as_deref()
                .and_then(|reason| reason.strip_prefix(crate::provider::STOPPED_PREFIX))
                .map(str::to_owned);
            if let Some(reason) = &stopped {
                emit(TurnEvent::Notice(&format!(
                    "\n[{} stopped the reply: {reason}]\n",
                    provider.name()
                )));
            }

            // Beyond Python: a reply that ends cleanly with nothing to say (a thinking
            // model can put its whole answer in its reasoning) is asked about once.
            // The empty step stays out of the history. If the second reply is empty
            // too, the reasoning stands in for the answer. A reply cut off at the cap
            // is the continuation path's, and a model out of steps is not asked again.
            let mut from_thinking = false;
            if completion.tool_calls.is_empty()
                && completion.stop_reason.as_deref() != Some("max_tokens")
                && stopped.is_none()
                && completion.text.trim().is_empty()
                && continuation_start.is_none()
            {
                if !completion.thinking.trim().is_empty() {
                    unanswered_thinking = std::mem::take(&mut completion.thinking);
                }
                if !nudged && !breaker.out_of_steps() {
                    nudged = true;
                    ask_again = true;
                    self.recorder.event(
                        &sid,
                        "empty_answer",
                        vec![
                            ("step", json!(breaker.steps())),
                            ("model", json!(provider.model())),
                            ("thinking_chars", json!(unanswered_thinking.chars().count())),
                        ],
                    );
                    record_step(
                        state,
                        Step {
                            index: state.steps.len() as u64 + 1,
                            phase: AgentPhase::Execute,
                            thought: String::new(),
                            tool_calls: Vec::new(),
                            results: Vec::new(),
                            usage: completion.usage,
                        },
                    );
                    self.checkpoint(state);
                    emit(TurnEvent::Notice(EMPTY_ANSWER_NOTICE));
                    continue;
                }
                if unanswered_thinking.trim().is_empty() {
                    emit(TurnEvent::Notice(NO_ANSWER_NOTICE));
                    // Beyond Python: LM Studio has been seen to answer a request that
                    // overflows the loaded context with nothing at all.
                    if provider.name() == crate::provider::ServerKind::LmStudio.name() {
                        emit(TurnEvent::Notice(&format!(
                            "{}\n",
                            crate::provider::hints::LMSTUDIO_EMPTY_ANSWER
                        )));
                    }
                } else {
                    completion.text = std::mem::take(&mut unanswered_thinking);
                    from_thinking = true;
                    emit(TurnEvent::Notice(THINKING_ANSWER_NOTICE));
                }
            }

            // Egress guardrail on the model's text. In redact mode the redacted copy
            // is what everything downstream sees: the output, the history, and the step.
            let answer = self.context.secrets.redact(&completion.text);
            let (egress, redacted) = self.guardrails.inspect_egress(&answer);
            let text = if buffering {
                if !redacted.is_empty() {
                    emit(TurnEvent::Text(&redacted));
                }
                redacted
            } else {
                // Reasoning standing in for the answer was never streamed as text.
                if from_thinking {
                    emit(TurnEvent::Text(&answer));
                }
                answer
            };
            if egress.flagged() {
                self.recorder.event(
                    &sid,
                    "guardrail",
                    vec![
                        ("stage", json!("egress")),
                        ("findings", json!(egress.summary())),
                    ],
                );
                emit(TurnEvent::Notice(&format!(
                    "\n[guardrail] output flagged: {}\n",
                    egress.summary()
                )));
            }
            let mut tool_calls = completion.tool_calls;
            // Before anything keeps or sees the calls, so all see the repair.
            let repairs = self.repair_broken_calls(&mut tool_calls, &sid);
            self.scan_tool_arguments(&tool_calls, &sid);
            let cut_off = completion.stop_reason.as_deref() == Some("max_tokens");
            // Beyond Python: a reply that reached the cap before writing anything (a
            // thinking model can spend all of it reasoning) has nothing to continue
            // from. The interface may offer a larger cap and ask again; the empty
            // reply is kept out of the history.
            if cut_off
                && tool_calls.is_empty()
                && text.trim().is_empty()
                && continuation_start.is_none()
            {
                let cap = self.output_cap();
                if self
                    .raise_limit(LimitKind::Output, None, &sid, cap, false)
                    .is_some()
                {
                    record_step(
                        state,
                        Step {
                            index: state.steps.len() as u64 + 1,
                            phase: AgentPhase::Execute,
                            thought: String::new(),
                            tool_calls: Vec::new(),
                            results: Vec::new(),
                            usage: completion.usage,
                        },
                    );
                    self.checkpoint(state);
                    continue;
                }
            }
            let mut step = Step {
                index: state.steps.len() as u64 + 1,
                phase: AgentPhase::Execute,
                thought: text.clone(),
                tool_calls: tool_calls.clone(),
                results: Vec::new(),
                usage: completion.usage,
            };
            state.messages.push(Message {
                tool_calls: tool_calls.clone(),
                ..message(Role::Assistant, &text)
            });

            if tool_calls.is_empty() {
                if cut_off && summary.continuations < MAX_CONTINUATIONS {
                    summary.continuations += 1;
                    continuation_start.get_or_insert(state.messages.len() - 1);
                    continuation_parts.push(text);
                    state.messages.push(message(Role::User, CONTINUE_PROMPT));
                    record_step(state, step);
                    self.checkpoint(state);
                    self.recorder.event(
                        &sid,
                        "continuation",
                        vec![
                            ("index", json!(summary.continuations)),
                            ("step", json!(breaker.steps())),
                            ("model", json!(provider.model())),
                            (
                                "chars",
                                json!(continuation_parts
                                    .last()
                                    .map_or(0, |part| part.chars().count())),
                            ),
                        ],
                    );
                    emit(TurnEvent::Notice(&continuation_notice(
                        summary.continuations,
                    )));
                    continue;
                }
                if let Some(start) = continuation_start.take() {
                    // Collapse [partial, "continue", partial, ...] into one answer.
                    continuation_parts.push(text);
                    let joined = std::mem::take(&mut continuation_parts).concat();
                    state.messages.truncate(start);
                    state.messages.push(message(Role::Assistant, &joined));
                    step.thought = joined;
                }
                // Beyond Python: with `[verify] command` set, a turn that changed the
                // workspace ends only once the check passes or its attempts run out.
                let steps_left = !breaker.out_of_steps();
                if let crate::verify::Gate::SendBack(feedback) =
                    self.verify_answer(&mut verify_run, steps_left, &sid, &mut emit)
                {
                    record_step(state, step);
                    state.messages.push(message(Role::User, &feedback));
                    self.checkpoint(state);
                    continue;
                }
                let outcome = step.thought.clone();
                record_step(state, step);
                state.phase = AgentPhase::Done;
                self.recorder.event(
                    &sid,
                    "turn_end",
                    vec![
                        ("status", json!("done")),
                        ("steps", json!(breaker.steps())),
                        ("continuations", json!(summary.continuations)),
                    ],
                );
                if let Some(memory) = &self.memory {
                    // A failed write loses one episode, never the turn.
                    let _ = memory.add_episode(&sid, goal, &outcome, &Default::default());
                }
                return Ok(output);
            }

            // A step that wants tools ends any continuation in progress.
            continuation_start = None;
            continuation_parts.clear();

            // Before the batch, never mid-execution. Python leaves these calls
            // unanswered, which providers reject on the next request; each is
            // answered here as not executed, as a cancel mid-batch does.
            if self.cancel.is_cancelled() {
                for call in &tool_calls {
                    let result = not_executed(call, "cancelled");
                    state.messages.push(Message {
                        tool_call_id: Some(call.id.clone()),
                        name: Some(call.name.clone()),
                        ..message(Role::Tool, &observation(&result))
                    });
                    step.results.push(result);
                }
                record_step(state, step);
                self.cancelled(state, breaker.steps(), &mut emit);
                return Ok(output);
            }

            // The whole batch is announced up front, then observed strictly in call order.
            for call in &tool_calls {
                let text = format!(
                    "\n[tool] {}({})\n",
                    call.name,
                    preview_arguments(&call.arguments)
                );
                emit(TurnEvent::ToolCall { call, text: &text });
            }
            let results = self.execute_batch(&tool_calls, &sid, breaker.cost()).await;
            // Beyond Python: what a sub-agent had to tell the user (a specialty's
            // model that could not be built), as notices, so `run`'s stdout keeps
            // only the answer.
            for notice in self.take_notices() {
                emit(TurnEvent::Notice(&format!("{notice}\n")));
            }
            // Questions shown as text (ADR 0006's text fallback), which end the turn.
            let mut asked_in_text: Option<String> = None;
            for (call, (result, delegated)) in tool_calls.iter().zip(results) {
                summary.tool_calls += 1;
                // What render_ui drew goes to the surface, never to the session or
                // the trace (the call's arguments already hold it).
                #[cfg(feature = "agent86ui")]
                let result = {
                    let mut result = result;
                    let (drawn, trace) = crate::agent86ui::take_drawn(&mut result.metadata);
                    for message in &drawn {
                        emit(TurnEvent::Ui(message));
                    }
                    if let Some(Value::Object(fields)) = trace {
                        // agent86's own kind (Python has none): which surfaces, how
                        // many components, and the verdict, never the tree.
                        let fields: Vec<(&str, Value)> = fields
                            .iter()
                            .map(|(key, value)| (key.as_str(), value.clone()))
                            .collect();
                        self.recorder.event(&sid, "ui_render", fields);
                    }
                    // A waiting call asks now that its messages are out, so the
                    // program has them to draw before it is asked for an answer.
                    match crate::agent86ui::take_wait(&mut result.metadata) {
                        Some(surfaces) => self.ui_wait(call, result, surfaces, &sid),
                        None => result,
                    }
                };
                // An `ask_user` call asks now (ADR 0006), after anything drawn
                // before it in the batch is out.
                let mut result = self.ask_questions(call, result, &sid, &mut emit);
                if let Some(Value::String(text)) = result.metadata.remove(crate::ask::ASK_TEXT_KEY)
                {
                    asked_in_text = Some(match asked_in_text.take() {
                        Some(earlier) => format!("{earlier}\n\n{text}"),
                        None => text,
                    });
                }
                let result = self.annotate_broken_json(call, result, &repairs, &mut broken_streak);
                // What a sub-agent spent is what this turn spent: in the summary, the
                // step, and the cost cap, but not as one of this turn's steps.
                summary.add_usage(&delegated);
                step.usage = step.usage + delegated;
                breaker.record_cost(delegated.cost_usd);
                let (content, findings) = self.guardrails.observe(observation(&result));
                // Beyond Python: no result takes more than its share of the window.
                let content = self.cap_result(content, step_budget);
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
                emit(TurnEvent::ToolResult {
                    call,
                    result: &result,
                    observation: &content,
                    text: &format!("[tool] {} -> {}\n", call.name, summarize(&result)),
                });
                state.messages.push(Message {
                    tool_call_id: Some(call.id.clone()),
                    name: Some(call.name.clone()),
                    ..message(Role::Tool, &content)
                });
                let ok = result.ok;
                verify_run.saw(&call.name, ok);
                stall.saw(call, &result, &content);
                step.results.push(result);
                if let Err(tripped) = breaker.record_tool_result(ok) {
                    record_step(state, step);
                    return Err(tripped);
                }
            }
            // ADR 0006's text fallback: the questions are the agent's message, kept
            // as an ordinary assistant message after the results (so the history
            // alternates as every provider requires, and reads naturally on resume
            // and in Python), and the turn ends without another model call. No new
            // stop reason is stored; the user's reply is the next turn.
            if let Some(questions) = asked_in_text {
                emit(TurnEvent::Text(&questions));
                state.messages.push(message(Role::Assistant, &questions));
                record_step(state, step);
                state.phase = AgentPhase::Done;
                self.recorder.event(
                    &sid,
                    "turn_end",
                    vec![
                        ("status", json!("done")),
                        ("steps", json!(breaker.steps())),
                        ("continuations", json!(summary.continuations)),
                    ],
                );
                if let Some(memory) = &self.memory {
                    let _ = memory.add_episode(&sid, goal, &questions, &Default::default());
                }
                return Ok(output);
            }
            // Every call has an observation by now, including any skipped because
            // the cancel landed mid-batch, so a note can follow them.
            if let Some(found) = stall.take() {
                self.remind_of_stall(&found, &sid, &mut emit);
                state.messages.push(message(Role::User, &found.note()));
            }
            record_step(state, step);
            self.checkpoint(state);
            if self.cancel.is_cancelled() {
                self.cancelled(state, breaker.steps(), &mut emit);
                return Ok(output);
            }
        }
    }

    /// Save the session at a step boundary (beyond Python; checkpointing tier 1,
    /// `docs/checkpointing.md`), so a process that dies mid-turn keeps the steps it
    /// finished. Only called once every tool call in the history has its answer.
    /// The phase reads `execute` until the turn ends, when every exit sets `done`
    /// or `error`, so a saved state that still says `execute` is a turn cut off.
    /// A failed write loses one checkpoint, never the turn.
    fn checkpoint(&self, state: &mut AgentState) {
        state.phase = AgentPhase::Execute;
        if let Some(memory) = &self.memory {
            let _ = memory.persist(state);
        }
    }

    /// Close out a cancelled turn and say so, as Python's `_cancelled`. The partial
    /// exchange is kept: the user stopped the agent, they did not undo it. The phase
    /// is `error`; "cancelled" is carried by the trace's `status` and `reason`.
    /// Beyond Python, which trims silently: when fitting leaves messages out of a
    /// request, the trace gets a `context_trim` event each time the count changes,
    /// and the turn says so once (again after a compaction has written a summary).
    fn announce_trim(
        &self,
        sid: &str,
        fitted: &Fitted,
        budget: u64,
        window: &mut TurnWindow<'_>,
        emit: &mut impl FnMut(TurnEvent<'_>),
    ) {
        if fitted.dropped == window.trimmed {
            return;
        }
        window.trimmed = fitted.dropped;
        if fitted.dropped == 0 {
            return;
        }
        self.recorder.event(
            sid,
            "context_trim",
            vec![
                ("dropped", json!(fitted.dropped)),
                ("kept", json!(fitted.messages.len())),
                ("budget", json!(budget)),
                ("goal_pinned", json!(fitted.pinned)),
            ],
        );
        if !window.trim_announced {
            window.trim_announced = true;
            emit(TurnEvent::Notice(&trimmed_notice(fitted.dropped)));
        }
    }

    /// Beyond Python: say that the turn was going in circles and record it (a
    /// `stall` trace event, since Python has none); the loop then sends the
    /// model [`crate::stall::Stall::note`].
    fn remind_of_stall(
        &self,
        found: &crate::stall::Stall,
        sid: &str,
        emit: &mut impl FnMut(TurnEvent<'_>),
    ) {
        emit(TurnEvent::Notice(&found.notice()));
        let mut fields = vec![
            ("shape", json!(found.kind.as_str())),
            ("count", json!(found.count)),
        ];
        if let Some(tool) = &found.tool {
            fields.push(("tool", json!(tool)));
        }
        self.recorder.event(sid, "stall", fields);
    }

    /// Beyond Python: a `context_age` trace event whenever the number of tool
    /// results a request sends aged changes (`crate::memory::sizing::aged_view`).
    /// Nothing is said to the user: the session keeps every result whole.
    fn announce_age(&self, sid: &str, aged: usize, budget: u64, window: &mut TurnWindow<'_>) {
        if aged == window.aged {
            return;
        }
        window.aged = aged;
        self.recorder.event(
            sid,
            "context_age",
            vec![("aged", json!(aged)), ("budget", json!(budget))],
        );
    }

    fn cancelled(&self, state: &mut AgentState, steps: u32, emit: &mut impl FnMut(TurnEvent<'_>)) {
        state.phase = AgentPhase::Error;
        self.recorder.event(
            &state.session_id,
            "turn_end",
            vec![
                ("status", json!("cancelled")),
                ("reason", json!("cancelled")),
                ("steps", json!(steps)),
            ],
        );
        emit(TurnEvent::Notice(CANCELLED_NOTICE));
    }

    /// Run a step's tool calls and return their results in call order.
    ///
    /// Read-only calls may run concurrently with each other; side-effecting calls
    /// run sequentially, in the order the model asked for them, after the reads.
    /// Every approval is settled first, on this thread, because the gate may
    /// prompt a human.
    ///
    /// Beyond Python, adjacent delegations to specialist sub-agents among the
    /// calls left after the reads run at once (`Harness::fan_out`, `[agents]
    /// max_parallel`), so a write listed before a delegation still happens first.
    /// A batch with any specialty delegation takes this reads-first path whatever
    /// the number of reads; `spent` is what the turn has spent so far, for the
    /// fan-out's cost cap.
    async fn execute_batch(
        &self,
        calls: &[ToolCall],
        sid: &str,
        spent: f64,
    ) -> Vec<(ToolResult, Usage)> {
        let eligible = calls
            .iter()
            .filter(|call| self.parallel_eligible(call))
            .count();
        let specialty: Vec<bool> = calls
            .iter()
            .map(|call| self.is_specialty_delegation(call))
            .collect();
        let any_specialty = specialty.contains(&true);
        if !self.limits.parallel_tools || (eligible < 2 && !any_specialty) {
            // Strictly sequential: each call is resolved and run before the next.
            let mut results = Vec::with_capacity(calls.len());
            for call in calls {
                results.push(if self.cancel.is_cancelled() {
                    (not_executed(call, "cancelled"), Usage::default())
                } else {
                    self.execute_tool(call, sid).await
                });
            }
            return results;
        }

        let mut settled: Vec<Option<(ToolResult, Usage)>> = Vec::with_capacity(calls.len());
        let mut may_parallel = Vec::with_capacity(calls.len());
        for call in calls {
            let (result, parallel) = self.resolve_call(call, sid);
            settled.push(result.map(|result| (result, Usage::default())));
            may_parallel.push(parallel);
        }
        let pending = || (0..calls.len()).filter(|&index| settled[index].is_none());
        let reads: Vec<usize> = pending().filter(|&index| may_parallel[index]).collect();
        let writes: Vec<usize> = pending().filter(|&index| !may_parallel[index]).collect();

        if reads.len() > 1 {
            let collected: Vec<(usize, ToolResult)> = std::thread::scope(|scope| {
                let handles: Vec<_> = reads
                    .iter()
                    .map(|&index| {
                        (
                            index,
                            scope.spawn(move || self.dispatch(&calls[index], sid)),
                        )
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|(index, handle)| {
                        let result = handle
                            .join()
                            .unwrap_or_else(|_| not_executed(&calls[index], "the tool panicked"));
                        (index, result)
                    })
                    .collect()
            });
            for (index, result) in collected {
                settled[index] = Some((result, Usage::default()));
            }
        } else {
            for &index in &reads {
                settled[index] = Some(self.dispatch_unless_cancelled(&calls[index], sid).await);
            }
        }
        // Between calls, never mid-execution: a tool that started is allowed to finish.
        // A run of adjacent specialty delegations goes together; anything else
        // (a write, a plain delegation) runs alone and separates them.
        let mut next = 0;
        while next < writes.len() {
            let index = writes[next];
            let run_end = writes[next..]
                .iter()
                .position(|&index| !specialty[index])
                .map_or(writes.len(), |offset| next + offset);
            if specialty[index] && run_end - next > 1 {
                let group: Vec<usize> = writes[next..run_end].to_vec();
                let results = if self.cancel.is_cancelled() {
                    group
                        .iter()
                        .map(|&index| (not_executed(&calls[index], "cancelled"), Usage::default()))
                        .collect()
                } else {
                    let members: Vec<&ToolCall> =
                        group.iter().map(|&index| &calls[index]).collect();
                    self.fan_out(&members, sid, spent)
                };
                for (index, result) in group.into_iter().zip(results) {
                    settled[index] = Some(result);
                }
                next = run_end;
            } else {
                settled[index] = Some(self.dispatch_unless_cancelled(&calls[index], sid).await);
                next += 1;
            }
        }

        settled
            .into_iter()
            .zip(calls)
            .map(|(result, call)| {
                result.unwrap_or_else(|| (not_executed(call, "skipped"), Usage::default()))
            })
            .collect()
    }

    /// Could this call share a step with its siblings? Decided from the tool alone,
    /// never the gate, which may prompt and must be consulted exactly once.
    fn parallel_eligible(&self, call: &ToolCall) -> bool {
        call.invalid_arguments().is_none()
            && self
                .registry
                .get(&call.name)
                .is_some_and(|tool| !tool.side_effecting() && tool.parallel_safe())
    }

    /// Decide one call without running it: `(settled result, may run in parallel)`.
    ///
    /// Arguments the provider could not parse are answered here, before the
    /// registry, the approval gate, or any tool sees them.
    ///
    /// Beyond Python, a call answered here is recorded as a `tool_call` event
    /// too, with Python's fields, so the trace says why a step's calls never ran
    /// (Python records only the calls it dispatches).
    pub(crate) fn resolve_call(&self, call: &ToolCall, sid: &str) -> (Option<ToolResult>, bool) {
        let settled = |result: ToolResult| {
            self.record_tool_call(call, &result, sid);
            (Some(result), false)
        };
        if let Some(raw) = call.invalid_arguments() {
            return settled(invalid_arguments_result(call, raw));
        }
        let Some(tool) = self.registry.get(&call.name) else {
            // The registry's own "Unknown tool" answer.
            return settled(self.registry.dispatch(call, &self.context));
        };
        // Under `run --agent`, a call outside the specialty's tools gets the
        // registry's refusal, which says so, before the gate is consulted.
        if !self.context.scope_allows(&call.name) {
            return settled(self.registry.dispatch(call, &self.context));
        }
        let decision = self.gate.decide(tool.as_ref(), call, &self.context);
        if !decision.approved {
            return settled(not_executed(call, decision.reason));
        }
        (None, !tool.side_effecting() && tool.parallel_safe())
    }

    async fn dispatch_unless_cancelled(&self, call: &ToolCall, sid: &str) -> (ToolResult, Usage) {
        if self.cancel.is_cancelled() {
            (not_executed(call, "cancelled"), Usage::default())
        } else {
            self.dispatch_call(call, sid).await
        }
    }

    /// Run one approved call: a delegation runs its sub-agent here, on this
    /// runtime; anything else goes to the registry.
    async fn dispatch_call(&self, call: &ToolCall, sid: &str) -> (ToolResult, Usage) {
        let request = match self.delegation(call, &self.context) {
            Some(request) => request,
            None => return (self.dispatch(call, sid), Usage::default()),
        };
        let mut span = self.tool_span(call);
        let (result, usage) = match request {
            Ok(request) => self.delegate(call, request, 1, &self.context).await,
            Err(invalid) => (invalid, Usage::default()),
        };
        span.set(vec![("tool.ok", json!(result.ok))]);
        self.tracer.end(span);
        self.record_tool_call(call, &result, sid);
        (result, usage)
    }

    /// Run one approved call and record it in the flight recorder.
    pub(crate) fn dispatch(&self, call: &ToolCall, sid: &str) -> ToolResult {
        let mut span = self.tool_span(call);
        let result = self.registry.dispatch(call, &self.context);
        span.set(vec![("tool.ok", json!(result.ok))]);
        self.tracer.end(span);
        self.record_tool_call(call, &result, sid);
        result
    }

    /// A `tool_call` span for `call`, as Python's `_dispatch_call` opens.
    pub(crate) fn tool_span(&self, call: &ToolCall) -> crate::otel::Span {
        self.tracer.start(
            "tool_call",
            vec![
                ("tool.name", json!(call.name)),
                ("gen_ai.tool.name", json!(call.name)),
            ],
        )
    }

    pub(crate) fn record_tool_call(&self, call: &ToolCall, result: &ToolResult, sid: &str) {
        self.recorder.event(
            sid,
            "tool_call",
            vec![
                ("tool", json!(call.name)),
                ("ok", json!(result.ok)),
                ("arguments", Value::Object(call.arguments.clone())),
                ("error", json!(result.error)),
            ],
        );
        self.record_repair(call, result, sid);
    }

    /// Beyond Python: note a call whose JSON-string arguments the registry repaired
    /// (`tools::repair_arguments`). The call itself is recorded as the model sent it.
    pub(crate) fn record_repair(&self, call: &ToolCall, result: &ToolResult, sid: &str) {
        if let Some(repaired) = result.metadata.get(crate::tools::ARGS_REPAIRED_KEY) {
            self.recorder.event(
                sid,
                "tool_args_repaired",
                vec![("tool", json!(call.name)), ("arguments", repaired.clone())],
            );
        }
    }

    /// Beyond Python (ADR 0006): repair each call whose arguments did not parse
    /// but do once every closer that matches nothing is dropped
    /// (`broken_json::drop_unmatched_closers`). Unlike the JSON-string repair, the
    /// repaired arguments replace the raw text on the call itself, since that text
    /// is no call at all: the history, the approval gate, the tool, and replay on
    /// resume then all see the same call. A `tool_args_repaired` event records
    /// what was dropped. Returns the note each repaired call's result gets, by id.
    pub(crate) fn repair_broken_calls(
        &self,
        calls: &mut [ToolCall],
        sid: &str,
    ) -> HashMap<String, String> {
        let mut notes = HashMap::new();
        for call in calls.iter_mut() {
            let Some(raw) = call.invalid_arguments() else {
                continue;
            };
            let Some((arguments, dropped)) = crate::broken_json::drop_unmatched_closers(raw) else {
                continue;
            };
            self.recorder.event(
                sid,
                "tool_args_repaired",
                vec![
                    ("tool", json!(call.name)),
                    (
                        "dropped",
                        json!(dropped
                            .iter()
                            .map(|(at, closer)| json!({"at": at, "char": closer.to_string()}))
                            .collect::<Vec<_>>()),
                    ),
                ],
            );
            notes.insert(call.id.clone(), repair_note(&dropped));
            call.arguments = arguments;
        }
        notes
    }

    /// `result` with what the model should know about its call's JSON: the
    /// repair's note, for a call [`Harness::repair_broken_calls`] repaired, and,
    /// once a tool's arguments have failed to parse twice in a row, advice on
    /// sending less (beyond Python; ADR 0006).
    pub(crate) fn annotate_broken_json(
        &self,
        call: &ToolCall,
        result: ToolResult,
        repairs: &HashMap<String, String>,
        streak: &mut BrokenStreak,
    ) -> ToolResult {
        let mut result = result;
        if let Some(note) = repairs.get(&call.id) {
            append_note(&mut result, note);
        }
        if let Some(count) = streak.observe(call) {
            let mut advice = format!(
                "{count} calls in a row to {} have sent arguments that do not parse. Long, \
                 deeply nested arguments break most often: send less in each call.",
                call.name
            );
            if let Some(hint) = self
                .registry
                .get(&call.name)
                .and_then(|tool| tool.retry_hint().map(str::to_owned))
            {
                advice.push(' ');
                advice.push_str(&hint);
            }
            append_note(&mut result, &advice);
        }
        result
    }

    /// Egress-scan tool-call arguments, the other way a secret leaves the harness.
    /// Findings are recorded, not acted on; the approval gate stops side effects.
    fn scan_tool_arguments(&self, calls: &[ToolCall], sid: &str) {
        if self.guardrails.egress == EgressMode::Off {
            return;
        }
        for call in calls {
            let payload = python_json_dumps(&Value::Object(call.arguments.clone()), false);
            let (report, _) = self.guardrails.inspect_egress(&payload);
            if report.flagged() {
                self.recorder.event(
                    sid,
                    "guardrail",
                    vec![
                        ("stage", json!("egress_tool_args")),
                        ("tool", json!(call.name)),
                        ("findings", json!(report.summary())),
                    ],
                );
            }
        }
    }

    /// Resolve and run one call.
    async fn execute_tool(&self, call: &ToolCall, sid: &str) -> (ToolResult, Usage) {
        match self.resolve_call(call, sid) {
            (Some(result), _) => (result, Usage::default()),
            (None, _) => self.dispatch_call(call, sid).await,
        }
    }
}

/// How many calls in a row to each tool have sent arguments that did not parse,
/// in one loop's turn (beyond Python; ADR 0006).
#[derive(Debug, Default)]
pub(crate) struct BrokenStreak(HashMap<String, u32>);

impl BrokenStreak {
    /// Count `call`: the streak when its arguments did not parse and it is at
    /// least the second such call to its tool in a row, else `None`. A call
    /// that parsed (or was repaired) ends its tool's streak.
    fn observe(&mut self, call: &ToolCall) -> Option<u32> {
        let count = self.0.entry(call.name.clone()).or_default();
        if call.invalid_arguments().is_none() {
            *count = 0;
            return None;
        }
        *count += 1;
        (*count >= 2).then_some(*count)
    }
}

/// What the model is told about a call whose broken arguments were repaired.
fn repair_note(dropped: &[(usize, char)]) -> String {
    let closers: Vec<String> = dropped
        .iter()
        .map(|(at, closer)| format!("`{closer}` at character {at}"))
        .collect();
    let noun = if dropped.len() == 1 {
        "closer"
    } else {
        "closers"
    };
    format!(
        "Note: these arguments were not valid JSON. agent86 dropped the unmatched {noun} {}, \
         which matched nothing, and ran the call with the rest. Check next time that every \
         bracket closes in order.",
        closers.join(", ")
    )
}

/// Add `note` to what the model reads of `result`: after its output when the
/// call worked, after its error when it failed.
fn append_note(result: &mut ToolResult, note: &str) {
    let text = if result.ok {
        &mut result.content
    } else {
        result.error.get_or_insert_with(String::new)
    };
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(note);
}

/// The observation for a call that was not run.
pub(crate) fn not_executed(call: &ToolCall, reason: &str) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        ok: false,
        content: String::new(),
        error: Some(format!("Not executed: {reason}.")),
        metadata: serde_json::Map::new(),
    }
}

/// Closing attributes for a `model_call` span, as Python's `_model_call_attributes`.
fn model_call_attributes(
    completion: Option<&crate::types::Completion>,
) -> Vec<(&'static str, Value)> {
    let Some(completion) = completion else {
        return vec![("agent86.completed", json!(false))];
    };
    let finish = completion
        .stop_reason
        .as_deref()
        .filter(|reason| !reason.is_empty())
        .map_or(Value::Null, |reason| json!([reason]));
    vec![
        ("agent86.completed", json!(true)),
        (
            "gen_ai.usage.input_tokens",
            json!(completion.usage.input_tokens),
        ),
        (
            "gen_ai.usage.output_tokens",
            json!(completion.usage.output_tokens),
        ),
        ("gen_ai.response.finish_reasons", finish),
        ("agent86.cost_usd", json!(completion.usage.cost_usd)),
        ("agent86.tool_calls", json!(completion.tool_calls.len())),
    ]
}

/// Closing attributes for the `turn` span, read off `state.last_turn`, as Python's
/// `_turn_attributes`.
fn turn_attributes(state: &AgentState, model: &str) -> Vec<(&'static str, Value)> {
    let mut attributes = vec![("gen_ai.request.model", json!(model))];
    if let Some(summary) = &state.last_turn {
        attributes.extend([
            ("gen_ai.usage.input_tokens", json!(summary.input_tokens)),
            ("gen_ai.usage.output_tokens", json!(summary.output_tokens)),
            ("agent86.cost_usd", json!(summary.cost_usd)),
            ("agent86.steps", json!(summary.steps)),
            ("agent86.tool_calls", json!(summary.tool_calls)),
            ("agent86.duration_s", json!(summary.duration_s)),
            (
                "agent86.phase",
                serde_json::to_value(state.phase).unwrap_or(Value::Null),
            ),
        ]);
    }
    attributes
}

/// Append `step` to the session and add its usage, as Python's `AgentState.record_step`.
fn record_step(state: &mut AgentState, step: Step) {
    state.usage = state.usage + step.usage;
    state.steps.push(step);
}

fn message(role: Role, content: &str) -> Message {
    Message {
        role,
        content: content.to_owned(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}

/// The observation for a tool call whose arguments were not valid JSON.
///
/// Its first line is Python's. Beyond Python, the lines after it say where the
/// JSON broke (`broken_json::describe`): the first 200 characters Python quotes
/// are usually fine, and a model shown only them guesses at what to fix.
pub fn invalid_arguments_result(call: &ToolCall, raw: &str) -> ToolResult {
    let preview = truncate_chars(raw, 200);
    let mut error = format!(
        "invalid JSON in tool arguments: {} could not be parsed. \
         Call the tool again with its arguments as a single valid JSON object.",
        python_repr(&preview)
    );
    if let Some(location) = crate::broken_json::describe(raw) {
        error.push('\n');
        error.push_str(&location);
    }
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        ok: false,
        content: String::new(),
        error: Some(error),
        metadata: serde_json::Map::new(),
    }
}

/// The text the model observes for `result`. A failure keeps both its error and
/// any output, so the model can see what went wrong.
pub fn observation(result: &ToolResult) -> String {
    if result.ok {
        return result.content.clone();
    }
    let error = result.error.as_deref().unwrap_or_default().trim();
    let body = result.content.trim();
    match (error.is_empty(), body.is_empty()) {
        (false, false) => format!("{error}\n{body}"),
        (false, true) => error.to_owned(),
        (true, false) => body.to_owned(),
        (true, true) => "error".to_owned(),
    }
}

/// The temperature a request sends, beyond Python's fixed 0.0: the specialty's
/// own, else its model's provider section's, else 0.0. Summaries keep 0.0.
pub(crate) fn request_temperature(provider: &dyn Provider, specialty: Option<f32>) -> f32 {
    specialty.or_else(|| provider.temperature()).unwrap_or(0.0)
}

/// Tool arguments as Python's `json.dumps(..., ensure_ascii=False)`, cut at 160 characters.
pub fn preview_arguments(arguments: &serde_json::Map<String, Value>) -> String {
    let text = python_json_dumps(&Value::Object(arguments.clone()), false);
    truncate_chars(&text, 160)
}

/// The one-line summary of a result shown after `[tool] name ->`.
pub fn summarize(result: &ToolResult) -> String {
    if !result.ok {
        let error = result.error.as_deref().unwrap_or_default().trim();
        let content = result.content.trim();
        let detail = [error, content, "(failed)"]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("(failed)");
        return format!("error: {}", first_chars(first_line(detail), 160));
    }
    let content = result.content.trim();
    let head = if content.is_empty() {
        "(no output)"
    } else {
        first_line(content)
    };
    first_chars(head, 160)
}

/// The first line of `text`, splitting on every boundary Python's `splitlines()` uses.
fn first_line(text: &str) -> &str {
    let is_break = |c: char| {
        matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    text.split(is_break).next().unwrap_or_default()
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// `text` if it has at most `limit` characters, else its first `limit` and `" ..."`.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        format!("{} ...", first_chars(text, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits() -> LimitsConfig {
        LimitsConfig {
            max_steps: 2,
            max_cost_usd: 1.0,
            max_wall_clock_s: 900,
            max_consecutive_errors: 1,
            max_context_tokens: 0,
            max_output_tokens: 8192,
            context_reserve_tokens: 4096,
            compaction: CompactionMode::Summarize,
            parallel_tools: true,
            tool_timeout_s: 60,
            context_sizing: true,
            stall_nudge: true,
        }
    }

    #[test]
    fn the_breaker_trips_on_steps_cost_and_error_streaks() {
        let mut breaker = CircuitBreaker::new(&limits());
        breaker.before_step().unwrap();
        breaker.record_step(&Usage::default());
        breaker.record_step(&Usage::default());
        assert_eq!(
            breaker.before_step().unwrap_err().to_string(),
            "Circuit tripped: step budget reached (2 steps)"
        );

        let mut costly = CircuitBreaker::new(&limits());
        costly.record_step(&Usage {
            cost_usd: 1.25,
            ..Usage::default()
        });
        assert_eq!(
            costly.before_step().unwrap_err().to_string(),
            "Circuit tripped: cost cap reached ($1.2500 >= $1.0)"
        );

        let mut errors = CircuitBreaker::new(&limits());
        errors.record_tool_result(false).unwrap();
        errors.record_tool_result(true).unwrap();
        errors.record_tool_result(false).unwrap();
        assert_eq!(
            errors.record_tool_result(false).unwrap_err().to_string(),
            "Circuit tripped: aborted after 2 consecutive tool errors"
        );
    }

    fn result(ok: bool, content: &str, error: Option<&str>) -> ToolResult {
        ToolResult {
            call_id: "c".to_owned(),
            name: "t".to_owned(),
            ok,
            content: content.to_owned(),
            error: error.map(str::to_owned),
            metadata: serde_json::Map::new(),
        }
    }

    #[test]
    fn observations_keep_both_error_and_output() {
        assert_eq!(observation(&result(true, "data", None)), "data");
        assert_eq!(
            observation(&result(false, " out ", Some(" bad "))),
            "bad\nout"
        );
        assert_eq!(observation(&result(false, "", Some("bad"))), "bad");
        assert_eq!(observation(&result(false, "", None)), "error");
    }

    #[test]
    fn summaries_show_the_first_line() {
        assert_eq!(summarize(&result(true, "\n first\nsecond", None)), "first");
        assert_eq!(summarize(&result(true, "  ", None)), "(no output)");
        assert_eq!(
            summarize(&result(false, "", Some("No such file: x\nmore"))),
            "error: No such file: x"
        );
        assert_eq!(summarize(&result(false, "", None)), "error: (failed)");
        assert_eq!(summarize(&result(true, &"x".repeat(200), None)).len(), 160);
    }

    #[test]
    fn previews_use_python_json_and_cut_at_160_characters() {
        let Value::Object(short) = json!({"path": "src/main.rs"}) else {
            unreachable!()
        };
        assert_eq!(preview_arguments(&short), r#"{"path": "src/main.rs"}"#);

        let Value::Object(long) = json!({"path": "a".repeat(300)}) else {
            unreachable!()
        };
        let preview = preview_arguments(&long);
        assert!(preview.ends_with(" ..."));
        assert_eq!(preview.chars().count(), 164);
    }

    #[test]
    fn invalid_json_is_reported_with_a_python_repr_preview() {
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "read_file".to_owned(),
            arguments: serde_json::Map::new(),
        };
        let result = invalid_arguments_result(&call, "{\"path\": ");
        let error = result.error.as_deref().unwrap();
        // Python's line first, then where it broke (beyond Python).
        assert_eq!(
            error.lines().next(),
            Some(
                "invalid JSON in tool arguments: '{\"path\": ' could not be parsed. \
                 Call the tool again with its arguments as a single valid JSON object."
            )
        );
        assert_eq!(
            error.lines().nth(1),
            Some(
                "The JSON ends at character 9 inside the top-level object, before it is \
                 complete: EOF while parsing a value."
            )
        );
        assert!(!result.ok);
        assert_eq!(result.call_id, "c1");
    }
}
