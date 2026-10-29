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
use crate::memory::working::{
    apply_summary, compacted_notice, compaction_cut, compaction_failed_notice, conversation_budget,
    count_spec_tokens, count_tokens, fit, fits, python_strip, render_transcript, KEEP_RECENT,
    SUMMARY_MAX_TOKENS, SUMMARY_SYSTEM_PROMPT,
};
use crate::observability::Recorder;
use crate::prompt::time_note;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

/// What the harness says to a model that ran out of output tokens mid-answer.
pub const CONTINUE_PROMPT: &str = "Continue exactly where you left off; do not repeat.";

/// How many times one turn may ask for more after hitting the output cap.
pub const MAX_CONTINUATIONS: u64 = 3;

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
#[derive(Debug, Clone, Copy)]
pub enum TurnEvent<'a> {
    /// The model's answer, or a notice from the harness (`[cancelled]`, `[guardrail] ...`).
    Text(&'a str),
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
}

impl<'a> TurnEvent<'a> {
    /// The text this event adds to the turn's output.
    pub fn text(&self) -> &'a str {
        match *self {
            Self::Text(text) | Self::ToolCall { text, .. } | Self::ToolResult { text, .. } => text,
            Self::Thinking(_) => "",
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
}

impl Harness {
    /// The system message for the next request: the prompt plus, when MCP servers
    /// are live, their section, then the turn's recall note, as Python's
    /// `_system_content`. Beyond Python, the names of the secrets the user entered
    /// come before the recall note, when there are any.
    pub(crate) fn system_message(&self, extra: Option<&str>) -> Message {
        let mut system = self.system.clone();
        if let Some(section) = self
            .mcp
            .as_ref()
            .map(|mcp| mcp.prompt_section())
            .filter(|section| !section.is_empty())
        {
            system.content = format!("{}\n\n{section}", system.content);
        }
        if let Some(section) = self.context.secrets.prompt_section() {
            system.content = format!("{}\n\n{section}", system.content);
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
    pub(crate) fn context_budget(&self, system: &Message, specs: &[ToolSpec]) -> u64 {
        let overhead = count_tokens(std::slice::from_ref(system)) + count_spec_tokens(specs);
        conversation_budget(
            self.active_context_window(),
            overhead,
            self.limits.context_reserve_tokens,
            u64::from(self.output_cap()),
            self.limits.max_context_tokens,
        )
    }

    /// Replace the oldest over-budget span with a summary, in place, before a model
    /// call, as Python's `_compact_if_needed`. Dropping the oldest turns (still
    /// available as `compaction = "drop"`) makes a long session forget its own
    /// goal; a summary keeps the goal, the decisions, and the paths found.
    ///
    /// Never fails the turn: a summary that cannot be written falls back to the drop
    /// that fitting the request does anyway, and says so. Returns the notice to
    /// stream and how many messages the history lost, so indices into it can follow.
    async fn compact_if_needed(
        &self,
        state: &mut AgentState,
        extra_system: Option<&str>,
        summary: &mut TurnSummary,
        breaker: &mut CircuitBreaker,
        protect_from: usize,
    ) -> Option<(String, usize)> {
        if self.limits.compaction != CompactionMode::Summarize {
            return None;
        }
        let system = self.system_message(extra_system);
        let budget = self.context_budget(&system, &self.registry.specs());
        if fits(&state.messages, budget) {
            return None;
        }
        let cut = compaction_cut(&state.messages, budget, KEEP_RECENT, Some(protect_from));
        if cut == 0 {
            // Everything left is the current turn or its tool blocks: nothing is old
            // enough to compact, so fitting trims and the provider's limits take over.
            return None;
        }
        let sid = state.session_id.clone();
        let prefix = &state.messages[..cut];
        let dropped_tokens = count_tokens(prefix);
        let summarizer = self.summary_provider();
        let request = CompletionRequest {
            model: summarizer.model().to_owned(),
            messages: vec![
                message(Role::System, SUMMARY_SYSTEM_PROMPT),
                message(Role::User, &render_transcript(prefix)),
            ],
            tools: Vec::new(),
            temperature: 0.0,
            // A summary is a bounded artefact; it should not cost more than the span.
            max_tokens: Some(SUMMARY_MAX_TOKENS * 2),
            stream: false,
        };
        let completion = tokio::select! {
            biased;
            // The next safe point sees the cancel and closes the turn out.
            () = self.cancel.cancelled() => return None,
            completion = summarizer.complete(request) => completion,
        };
        let failed = |status: &str, error: Option<String>| {
            let mut fields = vec![
                ("status", json!(status)),
                ("fallback", json!("drop")),
                ("dropped", json!(cut)),
            ];
            if let Some(error) = error {
                fields.push(("error", json!(error)));
            }
            self.recorder.event(&sid, "compaction", fields);
            Some((compaction_failed_notice(cut), 0))
        };
        let completion = match completion {
            Ok(completion) => completion,
            Err(error) => return failed("failed", Some(format!("ProviderError: {error}"))),
        };
        let text = python_strip(&completion.text).to_owned();
        if text.is_empty() {
            return failed("empty", None);
        }

        // The originals are archived before they leave the live conversation.
        if let Some(memory) = &self.memory {
            let _ = memory.record_compaction(&sid, &text, prefix);
        }
        let before = state.messages.len();
        let kept = state.messages.split_off(cut);
        state.messages = apply_summary(&text, kept);
        summary.compactions += 1;
        summary.add_usage(&completion.usage);
        breaker.record_cost(completion.usage.cost_usd);
        self.recorder.event(
            &sid,
            "compaction",
            vec![
                ("status", json!("ok")),
                ("dropped", json!(cut)),
                ("dropped_tokens", json!(dropped_tokens)),
                (
                    "summary_tokens",
                    json!(count_tokens(&[message(Role::User, &text)])),
                ),
                ("kept", json!(state.messages.len())),
                ("model", json!(summarizer.model())),
            ],
        );
        // Saved at once: a resume must see the compacted history, not a stale copy
        // that would re-inflate the context on the next turn.
        if let Some(memory) = &self.memory {
            let _ = memory.persist(state);
        }
        Some((compacted_notice(cut), before - state.messages.len()))
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
        // A cancel requested while no turn was running must not kill this one.
        self.cancel.clear();
        // A skill activated with `use_skill` restricts tools for its turn only: the
        // restriction is lifted on entry and on every exit, so it can never keep
        // refusing tools for the rest of the session.
        self.context.clear_skill();
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
            emit(TurnEvent::Text(&format!(
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
        let time = time_note(&chrono::Local::now());
        let recall_note = Some(match recall_note {
            Some(recalled) => format!("{time}\n\n{recalled}"),
            None => time,
        });
        // Where this turn begins: compaction never reaches past it, so the ask being
        // answered right now is never summarized away.
        let mut turn_start = state.messages.len();
        state.messages.push(message(Role::User, goal));
        let mut limits = self.limits.clone();
        limits.max_steps = self.max_steps();
        let mut breaker = CircuitBreaker::new(&limits);
        // Where the first partial answer went, and the pieces collected so far.
        let mut continuation_start: Option<usize> = None;
        let mut continuation_parts: Vec<String> = Vec::new();
        // Beyond Python: an empty final reply is asked about once (`ask_again` puts
        // ANSWER_PROMPT on the next request only, never in the history), and the
        // reasoning it came with is kept in case the second try is empty too.
        let mut nudged = false;
        let mut ask_again = false;
        let mut unanswered_thinking = String::new();

        loop {
            // Never start another model call once cancelled: that is the expensive,
            // long-latency step the user is trying to escape.
            if self.cancel.is_cancelled() {
                self.cancelled(state, breaker.steps(), &mut emit);
                return Ok(output);
            }
            // Beyond Python: the interface may offer to raise a spent budget.
            if breaker.out_of_steps() {
                match self.raise_limit(LimitKind::Steps, None, &sid, breaker.max_steps()) {
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
            if let Some((notice, removed)) = self
                .compact_if_needed(
                    state,
                    recall_note.as_deref(),
                    summary,
                    &mut breaker,
                    turn_start,
                )
                .await
            {
                // Indices into the history move with it. (Python keeps the old
                // ones, which go stale after a compaction mid-turn.)
                turn_start -= removed;
                if let Some(start) = continuation_start.as_mut() {
                    *start -= removed;
                }
                // Said out loud: a conversation that quietly lost its oldest turns
                // looks, from the user's seat, like a model that forgot.
                emit(TurnEvent::Text(&notice));
            }

            let mut completion = {
                let system = self.system_message(recall_note.as_deref());
                let tools = self.registry.specs();
                let budget = self.context_budget(&system, &tools);
                let conversation = fit(&state.messages, budget);
                let mut request_messages = Vec::with_capacity(conversation.len() + 1);
                request_messages.push(system);
                request_messages.extend(conversation.iter().cloned());
                if std::mem::take(&mut ask_again) {
                    request_messages.push(message(Role::User, ANSWER_PROMPT));
                }
                let request = CompletionRequest {
                    model: provider.model().to_owned(),
                    messages: request_messages,
                    tools,
                    temperature: 0.0,
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
            summary.steps = u64::from(breaker.steps());
            summary.add_usage(&completion.usage);
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

            // Beyond Python: a reply that ends cleanly with nothing to say (a thinking
            // model can put its whole answer in its reasoning) is asked about once.
            // The empty step stays out of the history. If the second reply is empty
            // too, the reasoning stands in for the answer. A reply cut off at the cap
            // is the continuation path's, and a model out of steps is not asked again.
            let mut from_thinking = false;
            if completion.tool_calls.is_empty()
                && completion.stop_reason.as_deref() != Some("max_tokens")
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
                    emit(TurnEvent::Text(EMPTY_ANSWER_NOTICE));
                    continue;
                }
                if unanswered_thinking.trim().is_empty() {
                    emit(TurnEvent::Text(NO_ANSWER_NOTICE));
                } else {
                    completion.text = std::mem::take(&mut unanswered_thinking);
                    from_thinking = true;
                    emit(TurnEvent::Text(THINKING_ANSWER_NOTICE));
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
                emit(TurnEvent::Text(&format!(
                    "\n[guardrail] output flagged: {}\n",
                    egress.summary()
                )));
            }
            let tool_calls = completion.tool_calls;
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
                    .raise_limit(LimitKind::Output, None, &sid, cap)
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
                    emit(TurnEvent::Text(&continuation_notice(summary.continuations)));
                    continue;
                }
                if let Some(start) = continuation_start {
                    // Collapse [partial, "continue", partial, ...] into one answer.
                    continuation_parts.push(text);
                    let joined = continuation_parts.concat();
                    state.messages.truncate(start);
                    state.messages.push(message(Role::Assistant, &joined));
                    step.thought = joined;
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
            let results = self.execute_batch(&tool_calls, &sid).await;
            for (call, (result, delegated)) in tool_calls.iter().zip(results) {
                summary.tool_calls += 1;
                // What a sub-agent spent is what this turn spent: in the summary, the
                // step, and the cost cap, but not as one of this turn's steps.
                summary.add_usage(&delegated);
                step.usage = step.usage + delegated;
                breaker.record_cost(delegated.cost_usd);
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
                step.results.push(result);
                if let Err(tripped) = breaker.record_tool_result(ok) {
                    record_step(state, step);
                    return Err(tripped);
                }
            }
            // Every call has an observation by now, including any skipped because
            // the cancel landed mid-batch.
            record_step(state, step);
            if self.cancel.is_cancelled() {
                self.cancelled(state, breaker.steps(), &mut emit);
                return Ok(output);
            }
        }
    }

    /// Close out a cancelled turn and say so, as Python's `_cancelled`. The partial
    /// exchange is kept: the user stopped the agent, they did not undo it. The phase
    /// is `error`; "cancelled" is carried by the trace's `status` and `reason`.
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
        emit(TurnEvent::Text(CANCELLED_NOTICE));
    }

    /// Run a step's tool calls and return their results in call order.
    ///
    /// Read-only calls may run concurrently with each other; side-effecting calls
    /// run sequentially, in the order the model asked for them, after the reads.
    /// Every approval is settled first, on this thread, because the gate may
    /// prompt a human.
    async fn execute_batch(&self, calls: &[ToolCall], sid: &str) -> Vec<(ToolResult, Usage)> {
        let eligible = calls
            .iter()
            .filter(|call| self.parallel_eligible(call))
            .count();
        if !self.limits.parallel_tools || eligible < 2 {
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
            let (result, parallel) = self.resolve_call(call);
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
        for index in writes {
            settled[index] = Some(self.dispatch_unless_cancelled(&calls[index], sid).await);
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
    fn resolve_call(&self, call: &ToolCall) -> (Option<ToolResult>, bool) {
        if let Some(raw) = call.invalid_arguments() {
            return (Some(invalid_arguments_result(call, raw)), false);
        }
        let Some(tool) = self.registry.get(&call.name) else {
            // The registry's own "Unknown tool" answer.
            return (Some(self.registry.dispatch(call, &self.context)), false);
        };
        let decision = self.gate.decide(tool.as_ref(), call, &self.context);
        if !decision.approved {
            return (Some(not_executed(call, decision.reason)), false);
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
            Ok((role, task)) => self.delegate(call, &role, &task, 1).await,
            Err(invalid) => (invalid, Usage::default()),
        };
        span.set(vec![("tool.ok", json!(result.ok))]);
        self.tracer.end(span);
        self.record_tool_call(call, &result, sid);
        (result, usage)
    }

    /// Run one approved call and record it in the flight recorder.
    fn dispatch(&self, call: &ToolCall, sid: &str) -> ToolResult {
        let mut span = self.tool_span(call);
        let result = self.registry.dispatch(call, &self.context);
        span.set(vec![("tool.ok", json!(result.ok))]);
        self.tracer.end(span);
        self.record_tool_call(call, &result, sid);
        result
    }

    /// A `tool_call` span for `call`, as Python's `_dispatch_call` opens.
    fn tool_span(&self, call: &ToolCall) -> crate::otel::Span {
        self.tracer.start(
            "tool_call",
            vec![
                ("tool.name", json!(call.name)),
                ("gen_ai.tool.name", json!(call.name)),
            ],
        )
    }

    fn record_tool_call(&self, call: &ToolCall, result: &ToolResult, sid: &str) {
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
        match self.resolve_call(call) {
            (Some(result), _) => (result, Usage::default()),
            (None, _) => self.dispatch_call(call, sid).await,
        }
    }
}

/// The observation for a call that was not run.
fn not_executed(call: &ToolCall, reason: &str) -> ToolResult {
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
pub fn invalid_arguments_result(call: &ToolCall, raw: &str) -> ToolResult {
    let preview = truncate_chars(raw, 200);
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        ok: false,
        content: String::new(),
        error: Some(format!(
            "invalid JSON in tool arguments: {} could not be parsed. \
             Call the tool again with its arguments as a single valid JSON object.",
            python_repr(&preview)
        )),
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
        assert_eq!(
            result.error.as_deref(),
            Some(
                "invalid JSON in tool arguments: '{\"path\": ' could not be parsed. \
                 Call the tool again with its arguments as a single valid JSON object."
            )
        );
        assert!(!result.ok);
        assert_eq!(result.call_id, "c1");
    }
}
