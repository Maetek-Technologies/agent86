//! Sub-agents, ported from Python's `agents/subagent.py` and the harness's
//! `spawn_subagent`.
//!
//! A sub-agent is a role-scoped Reason, Act, Observe loop that reuses the
//! harness's provider, tool registry, sandbox, approval gate, and recorder, so a
//! delegated task gets the same guardrails and observability as the main agent.
//! It runs under its own step budget (`[agents] max_steps`, never more than
//! `[limits] max_steps`) and the main loop's output cap, reports every model call to the recorder tagged with
//! its role and depth, and hands its token usage back, so the parent turn's cost
//! includes what it spent. Depth is tracked so delegation cannot recurse without
//! bound.

use crate::agent::{
    invalid_arguments_result, observation, CircuitBreaker, Harness, HarnessError, ANSWER_PROMPT,
    CONTINUE_PROMPT, MAX_CONTINUATIONS,
};
use crate::limits::LimitKind;
use crate::memory::working::fit;
use crate::prompt::time_note;
use crate::provider::ProviderError;
use crate::tools::delegate::{self, NAME as DELEGATE};
use crate::tools::{success, ToolContext};
use crate::types::{CompletionRequest, Message, Role, ToolCall, ToolResult, Usage};
use futures_util::future::BoxFuture;
use serde_json::json;

/// The sub-agent's persona, ahead of the parent's system content.
pub const ROLE_PREAMBLE: &str = "You are a '{role}' sub-agent operating inside a larger agent system. You were delegated a focused task by a supervising agent. Use tools as needed, complete the task precisely, and return ONLY the final result \u{2014} no preamble.";

fn message(role: Role, content: String) -> Message {
    Message {
        role,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}

impl Harness {
    /// The role and task of a `delegate` call the harness should run itself, or
    /// `None` when the registry should answer it (delegation off, the tool not
    /// registered, or refused by the active skill). Invalid arguments come back as
    /// the tool's own validation failure.
    pub(crate) fn delegation(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Option<Result<(String, String), ToolResult>> {
        if call.name != DELEGATE || !self.agents.enabled || !ctx.allowed_tool(&call.name) {
            return None;
        }
        let tool = self.registry.get(DELEGATE)?;
        Some(delegate::request(&call.arguments).map_err(|_| tool.run(call, ctx)))
    }

    /// Run an approved delegation at `depth` and answer the call with the
    /// sub-agent's result, or with the failure that stopped it.
    pub(crate) async fn delegate(
        &self,
        call: &ToolCall,
        role: &str,
        task: &str,
        depth: u32,
    ) -> (ToolResult, Usage) {
        let (mut result, usage) = match self.spawn_subagent(role, task, depth).await {
            Ok((text, usage)) => (success(DELEGATE, text), usage),
            // A model call that failed inside the sub-agent is the tool's error,
            // as Python's `Tool.run` reports an exception.
            Err(error) => (
                crate::tools::failure(DELEGATE, format!("ProviderError: {error}")),
                Usage::default(),
            ),
        };
        result.call_id = call.id.clone();
        (result, usage)
    }

    /// Run a delegated task on a role-scoped sub-agent, as Python's
    /// `spawn_subagent`: its answer and everything it (and anything it delegated
    /// to) spent.
    pub fn spawn_subagent<'a>(
        &'a self,
        role: &'a str,
        task: &'a str,
        depth: u32,
    ) -> BoxFuture<'a, Result<(String, Usage), ProviderError>> {
        Box::pin(async move {
            let max_depth = self.agents.max_depth;
            if depth > max_depth {
                return Ok((
                    format!("[delegation refused: max agent depth {max_depth} reached]"),
                    Usage::default(),
                ));
            }
            let preview: String = task.chars().take(200).collect();
            self.recorder.event(
                "sub",
                "spawn",
                vec![
                    ("role", json!(role)),
                    ("depth", json!(depth)),
                    ("task", json!(preview)),
                ],
            );
            let role = if role.is_empty() { "assistant" } else { role };
            self.run_subagent(role, task, depth).await
        })
    }

    async fn run_subagent(
        &self,
        role: &str,
        task: &str,
        depth: u32,
    ) -> Result<(String, Usage), ProviderError> {
        let sid = format!("sub:{role}");
        // The turn's model, routed or not, as Python's sub-agent uses `h.provider`.
        let provider = self.active_provider();
        // The persona, then the parent's system content, so the sub-agent sees the
        // environment, the skills it can load, and the MCP tools it can call, and
        // (beyond Python) the time it started.
        let time = time_note(&chrono::Local::now());
        let system = message(
            Role::System,
            format!(
                "{}\n\n{}",
                ROLE_PREAMBLE.replace("{role}", role),
                self.system_message(Some(&time)).content
            ),
        );
        let mut messages = vec![message(Role::User, task.to_owned())];
        let mut limits = self.limits.clone();
        limits.max_steps = self.agent_max_steps();
        let mut breaker = CircuitBreaker::new(&limits);
        // Its own context: a skill it activates restricts only its own calls.
        let ctx = self.context.fork();
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
            // Beyond Python: the interface may offer to raise a spent budget.
            if breaker.out_of_steps() {
                if let Some(raised) =
                    self.raise_limit(LimitKind::AgentSteps, Some(role), &sid, breaker.max_steps())
                {
                    breaker.set_max_steps(raised);
                }
            }
            if let Err(error) = breaker.before_step() {
                return Ok((halted(error), spent + delegated));
            }
            let mut specs = self.registry.specs();
            // At the deepest level the sub-agent cannot spawn further.
            if depth >= self.agents.max_depth {
                specs.retain(|spec| spec.name != DELEGATE);
            }
            // The same window discipline as the main loop, with the system message
            // held out of the trim so it is never the part dropped.
            let budget = self.context_budget(&self.system_message(None), &self.registry.specs());
            let mut request_messages = vec![system.clone()];
            request_messages.extend(fit(&messages, budget).iter().cloned());
            if std::mem::take(&mut ask_again) {
                request_messages.push(message(Role::User, ANSWER_PROMPT.to_owned()));
            }
            let completion = self
                .provider
                .complete(CompletionRequest {
                    model: provider.model().to_owned(),
                    messages: request_messages,
                    tools: specs,
                    temperature: 0.0,
                    // A deliberate fix: Python sends none, so a sub-agent gets the
                    // provider's default (8192 on Anthropic, the server's own on an
                    // OpenAI-compatible endpoint), which no setting but the provider's
                    // `max_tokens` changes. It gets the main loop's cap instead.
                    max_tokens: Some(self.output_cap()),
                    stream: false,
                })
                .await?;
            breaker.record_step(&completion.usage);
            spent = spent + completion.usage;
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
            // request sent again, the empty reply left out of the history.
            if cut_off && !has_text && completion.tool_calls.is_empty() && parts.is_empty() {
                let cap = self.output_cap();
                if self
                    .raise_limit(LimitKind::Output, Some(role), &sid, cap)
                    .is_some()
                {
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
                        format!(
                            "[sub-agent '{role}' wrote no answer: its last reply reached the output limit ({} tokens) before any text. Raise {} or delegate a smaller task.]",
                            completion.usage.output_tokens,
                            self.output_setting()
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
                        ("steps", json!(breaker.steps())),
                        ("cost_usd", json!(spent.cost_usd)),
                    ],
                );
                return Ok((answer, spent + delegated));
            }

            for call in &completion.tool_calls {
                let (result, usage) = self.run_subagent_tool(&ctx, call, &sid, depth).await;
                delegated = delegated + usage;
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

    /// One of a sub-agent's calls: the same interception, approval, and dispatch
    /// as the main loop, one at a time, on the sub-agent's own context. A
    /// delegation from here runs one level deeper.
    async fn run_subagent_tool(
        &self,
        ctx: &ToolContext,
        call: &ToolCall,
        sid: &str,
        depth: u32,
    ) -> (ToolResult, Usage) {
        if let Some(raw) = call.invalid_arguments() {
            return (invalid_arguments_result(call, raw), Usage::default());
        }
        let Some(tool) = self.registry.get(&call.name) else {
            return (self.registry.dispatch(call, ctx), Usage::default());
        };
        let decision = self.gate.decide(tool.as_ref(), call, ctx);
        if !decision.approved {
            let mut result =
                crate::tools::failure(&call.name, format!("Not executed: {}.", decision.reason));
            result.call_id = call.id.clone();
            return (result, Usage::default());
        }
        let (result, usage) = match self.delegation(call, ctx) {
            Some(Ok((role, task))) => self.delegate(call, &role, &task, depth + 1).await,
            Some(Err(invalid)) => (invalid, Usage::default()),
            None => (self.registry.dispatch(call, ctx), Usage::default()),
        };
        self.recorder.event(
            sid,
            "tool_call",
            vec![
                ("tool", json!(call.name)),
                ("ok", json!(result.ok)),
                ("depth", json!(depth)),
            ],
        );
        (result, usage)
    }
}
