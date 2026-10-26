//! The ReAct loop, ported from Python's `orchestration/loop.py` and
//! `orchestration/circuit.py`.
//!
//! One user turn: call the model, run the tools it asks for, feed the results
//! back, and repeat until it answers without tools. A circuit breaker bounds the
//! turn by steps, cost, wall-clock time, and consecutive tool errors. Tool
//! activity is reported inline as `[tool] ...` lines, exactly as Python streams it.
//!
//! Not ported yet: approvals and guardrails (no side-effecting tools exist),
//! context-window fitting and compaction, cancellation, sub-agents, and
//! persistence.

use crate::config::LimitsConfig;
use crate::provider::{Provider, ProviderError};
use crate::pyjson::{python_float_repr, python_json_dumps};
use crate::tools::{ToolContext, ToolRegistry};
use crate::turn::{new_session_id, RunOutput, TurnSummary};
use crate::types::{python_repr, CompletionRequest, Message, Role, ToolCall, ToolResult, Usage};
use futures_util::StreamExt;
use serde_json::Value;
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
pub struct Harness<'a> {
    pub provider: &'a dyn Provider,
    pub registry: ToolRegistry,
    pub context: ToolContext,
    pub limits: LimitsConfig,
    pub system: Message,
    /// The output cap sent with every model call.
    pub max_tokens: u32,
}

impl Harness<'_> {
    /// Run one user turn to completion, passing every piece of streamed text,
    /// including tool notices, to `on_text`.
    pub async fn run_turn(
        &self,
        goal: &str,
        mut on_text: impl FnMut(&str),
    ) -> Result<RunOutput, HarnessError> {
        let started = Instant::now();
        let mut output = String::new();
        let mut emit = |text: &str| {
            output.push_str(text);
            on_text(text);
        };

        let mut breaker = CircuitBreaker::new(&self.limits);
        let mut summary = TurnSummary::default();
        let mut messages = vec![message(Role::User, goal)];
        let mut steps = 0u64;
        let mut usage = Usage::default();
        // Where the first partial answer went, and the pieces collected so far.
        let mut continuation_start: Option<usize> = None;
        let mut continuation_parts: Vec<String> = Vec::new();

        loop {
            breaker.before_step()?;

            let completion = {
                let mut request_messages = Vec::with_capacity(messages.len() + 1);
                request_messages.push(self.system.clone());
                request_messages.extend(messages.iter().cloned());
                let request = CompletionRequest {
                    model: self.provider.model().to_owned(),
                    messages: request_messages,
                    tools: self.registry.specs(),
                    temperature: 0.0,
                    max_tokens: Some(self.max_tokens),
                    stream: true,
                };
                let mut stream = self.provider.stream(request);
                let mut completion = None;
                while let Some(delta) = stream.next().await {
                    let delta = delta?;
                    if !delta.text.is_empty() {
                        emit(&delta.text);
                    }
                    if delta.done && delta.completion.is_some() {
                        completion = delta.completion;
                    }
                }
                completion.ok_or_else(|| HarnessError::NoFinalCompletion {
                    provider: self.provider.name().to_owned(),
                    model: self.provider.model().to_owned(),
                })?
            };

            breaker.record_step(&completion.usage);
            summary.steps = u64::from(breaker.steps());
            summary.add_usage(&completion.usage);
            let step_usage = completion.usage;
            let text = completion.text;
            let tool_calls = completion.tool_calls;
            messages.push(Message {
                tool_calls: tool_calls.clone(),
                ..message(Role::Assistant, &text)
            });

            if tool_calls.is_empty() {
                let cut_off = completion.stop_reason.as_deref() == Some("max_tokens");
                if cut_off && summary.continuations < MAX_CONTINUATIONS {
                    summary.continuations += 1;
                    continuation_start.get_or_insert(messages.len() - 1);
                    continuation_parts.push(text);
                    messages.push(message(Role::User, CONTINUE_PROMPT));
                    steps += 1;
                    usage = usage + step_usage;
                    emit(&continuation_notice(summary.continuations));
                    continue;
                }
                if let Some(start) = continuation_start {
                    // Collapse [partial, "continue", partial, ...] into one answer.
                    continuation_parts.push(text);
                    messages.truncate(start);
                    messages.push(message(Role::Assistant, &continuation_parts.concat()));
                }
                steps += 1;
                usage = usage + step_usage;
                break;
            }

            // A step that wants tools ends any continuation in progress.
            continuation_start = None;
            continuation_parts.clear();

            // The whole batch is announced up front, then observed strictly in call order.
            for call in &tool_calls {
                emit(&format!(
                    "\n[tool] {}({})\n",
                    call.name,
                    preview_arguments(&call.arguments)
                ));
            }
            let results = self.execute_batch(&tool_calls);
            for (call, result) in tool_calls.iter().zip(&results) {
                summary.tool_calls += 1;
                messages.push(Message {
                    tool_call_id: Some(call.id.clone()),
                    name: Some(call.name.clone()),
                    ..message(Role::Tool, &observation(result))
                });
                emit(&format!("[tool] {} -> {}\n", call.name, summarize(result)));
                breaker.record_tool_result(result.ok)?;
            }
            steps += 1;
            usage = usage + step_usage;
        }

        summary.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        Ok(RunOutput {
            session_id: new_session_id(),
            output,
            steps,
            usage,
            turn: Some(summary),
        })
    }

    /// Run a step's tool calls and return their results in call order.
    ///
    /// Read-only calls may run concurrently with each other; anything else runs
    /// sequentially. All current tools are read-only.
    fn execute_batch(&self, calls: &[ToolCall]) -> Vec<ToolResult> {
        let parallel: Vec<bool> = calls
            .iter()
            .map(|call| self.parallel_eligible(call))
            .collect();
        let eligible = parallel.iter().filter(|&&flag| flag).count();
        if !self.limits.parallel_tools || eligible < 2 {
            return calls.iter().map(|call| self.execute_tool(call)).collect();
        }

        let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
        std::thread::scope(|scope| {
            let handles: Vec<_> = calls
                .iter()
                .enumerate()
                .filter(|(index, _)| parallel[*index])
                .map(|(index, call)| (index, scope.spawn(move || self.execute_tool(call))))
                .collect();
            for (index, handle) in handles {
                results[index] = handle.join().ok();
            }
        });
        for (index, call) in calls.iter().enumerate() {
            if results[index].is_none() {
                results[index] = Some(self.execute_tool(call));
            }
        }
        results.into_iter().flatten().collect()
    }

    fn parallel_eligible(&self, call: &ToolCall) -> bool {
        call.invalid_arguments().is_none()
            && self
                .registry
                .get(&call.name)
                .is_some_and(|tool| !tool.side_effecting())
    }

    /// Resolve and run one call. Arguments the provider could not parse are
    /// answered here, before any tool sees them.
    fn execute_tool(&self, call: &ToolCall) -> ToolResult {
        match call.invalid_arguments() {
            Some(raw) => invalid_arguments_result(call, raw),
            None => self.registry.dispatch(call, &self.context),
        }
    }
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
            max_output_tokens: 8192,
            parallel_tools: true,
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
