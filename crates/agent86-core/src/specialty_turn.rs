//! The user's own entry points to specialist sub-agents (beyond Python;
//! `docs/subagents.md` section 3): `/agent <name> <task>`, which runs a
//! specialty directly with no main-agent call and puts its answer in the
//! conversation, and `run --agent`, which makes a specialty the main agent of a
//! run.

use crate::agent::{Harness, HarnessError, TurnEvent, CANCELLED_NOTICE};
use crate::agents_def::Specialty;
use crate::config::EgressMode;
use crate::session::AgentState;
use crate::subagent::Spawn;
use crate::tools::delegate::NAME as DELEGATE;
use crate::turn::TurnSummary;
use crate::types::{AgentPhase, Message, Role, Step, ToolCall, Usage};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

/// How a command asked a turn to run (beyond Python): plainly, with a skill the
/// user chose, or as a specialist sub-agent the user named with `/agent`.
#[derive(Debug, Clone)]
pub enum TurnKind {
    Plain,
    Skill(Arc<crate::skills::Skill>),
    Agent { name: String, with_context: bool },
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

/// The question a project specialty's approval belongs to, for an entry point
/// with no tool call of its own.
fn synthetic_call(name: &str, task: &str, n: u64) -> ToolCall {
    ToolCall {
        id: format!("agent-{name}-{n}"),
        name: DELEGATE.into(),
        arguments: serde_json::Map::from_iter([
            ("agent".to_owned(), json!(name)),
            ("task".to_owned(), json!(task)),
        ]),
    }
}

impl Harness {
    /// Run a turn of `kind`: [`Harness::run_turn_events`], [`Harness::run_skill_turn`],
    /// or [`Harness::run_agent_turn`]. Every surface goes through here.
    pub async fn run_kind(
        &self,
        state: &mut AgentState,
        kind: &TurnKind,
        prompt: &str,
        display_text: &str,
        on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<String, HarnessError> {
        match kind {
            TurnKind::Plain => {
                self.run_turn_events(state, prompt, display_text, on_event)
                    .await
            }
            TurnKind::Skill(skill) => {
                self.run_skill_turn(state, skill, prompt, display_text, on_event)
                    .await
            }
            TurnKind::Agent { name, with_context } => {
                self.run_agent_turn(state, name, prompt, display_text, *with_context, on_event)
                    .await
            }
        }
    }

    /// `/agent <name> <task>`: run the specialty `name` on `task` as a sub-agent at
    /// depth 1, with no main-agent call, and put the exchange in the conversation as
    /// an ordinary user and assistant message, so `--resume` and replay work
    /// unchanged. Everything a turn does around its model calls happens here too:
    /// the cancel token is cleared, the user's text and the answer go through the
    /// vault and the guardrails, a specialty with no `model:` routes on the task,
    /// usage reaches the session, and `turn_start`, `turn_end`, and the episode are
    /// recorded. With `with_context`, the conversation so far is summarized by the
    /// compaction summarizer and handed over as context.
    pub async fn run_agent_turn(
        &self,
        state: &mut AgentState,
        name: &str,
        task: &str,
        display_text: &str,
        with_context: bool,
        mut on_event: impl FnMut(TurnEvent<'_>),
    ) -> Result<String, HarnessError> {
        self.cancel.clear();
        self.context.clear_skill();
        let started = Instant::now();
        let mut output = String::new();
        let mut emit = |event: TurnEvent<'_>| {
            output.push_str(event.text());
            on_event(event);
        };
        let secrets = &self.context.secrets;
        let (task, display_text) = (secrets.redact(task), secrets.redact(display_text));
        let sid = state.session_id.clone();
        let mut summary = TurnSummary::default();
        self.recorder
            .event(&sid, "turn_start", vec![("task", json!(display_text))]);
        let finish = |state: &mut AgentState, summary: TurnSummary| {
            let mut summary = summary;
            summary.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
            state.last_turn = Some(summary);
        };

        // The ingress guardrail on the user's text.
        let report = self.guardrails.inspect_ingress(&task);
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
                state.messages.push(message(Role::User, &display_text));
                state.messages.push(message(Role::Assistant, &refusal));
                state.phase = AgentPhase::Error;
                self.recorder
                    .event(&sid, "turn_end", vec![("status", json!("blocked"))]);
                emit(TurnEvent::Text(&refusal));
                finish(state, summary);
                return Ok(output);
            }
            emit(TurnEvent::Notice(&format!(
                "[guardrail] input flagged: {}\n",
                report.summary()
            )));
        }

        // The specialty, approved: an unknown or refused one is answered here, with
        // no model call and nothing added to the conversation.
        let call = synthetic_call(name, &task, self.subagents.next_run());
        let specialty = match self.specialty_for(name, &self.context, &call) {
            Ok(specialty) => specialty,
            Err(error) => {
                emit(TurnEvent::Notice(&format!("[agent] {error}\n")));
                self.recorder.event(
                    &sid,
                    "turn_end",
                    vec![("status", json!("refused")), ("reason", json!(error))],
                );
                finish(state, summary);
                return Ok(output);
            }
        };
        // With no model of its own it runs on the turn's, routed on its task.
        if specialty.model.is_none() {
            if let Err(error) = self.route(&task, &sid) {
                state.phase = AgentPhase::Error;
                finish(state, summary);
                return Err(error.into());
            }
        }

        let mut first = task.clone();
        if with_context {
            if let Some((text, usage)) = self.conversation_summary(state).await {
                summary.add_usage(&usage);
                state.usage = state.usage + usage;
                let (text, _) = self.guardrails.observe(secrets.redact(&text));
                first.push_str(&format!("\n\n[Context from the supervising agent]\n{text}"));
            }
        }
        let preloaded = self.preloaded_skills(&specialty);
        emit(TurnEvent::Notice(&format!(
            "[agent] {} is working on it\n",
            specialty.name
        )));
        let spawn = Spawn {
            role: specialty.name.clone(),
            task: first,
            specialty: Some(Arc::clone(&specialty)),
            preloaded,
            worktree: None,
            // No tool call stands for it, so its card is announced when it starts.
            card: Some(Arc::new(crate::subagent::Card::new(call.id.clone(), false))),
        };
        let result = self.spawn_resolved(spawn, 1, &self.context).await;
        for notice in self.take_notices() {
            emit(TurnEvent::Notice(&format!("{notice}\n")));
        }
        let user_text = format!(
            "[The user ran the specialist sub-agent '{}' with this task]\n{task}",
            specialty.name
        );
        let (answer, usage) = match result {
            Ok(done) => done,
            Err(failed) => {
                let (error, usage) = *failed;
                summary.add_usage(&usage);
                state.usage = state.usage + usage;
                state.phase = AgentPhase::Error;
                let reason = format!("provider stream failed: ProviderError: {error}");
                self.recorder.event(
                    &sid,
                    "turn_end",
                    vec![("status", json!("error")), ("reason", json!(reason))],
                );
                finish(state, summary);
                return Err(error.into());
            }
        };
        summary.add_usage(&usage);
        if self.cancel.is_cancelled() {
            // The user stopped it: the ask stays, the answer it never gave does not.
            state.messages.push(message(Role::User, &user_text));
            state.usage = state.usage + usage;
            state.phase = AgentPhase::Error;
            self.recorder.event(
                &sid,
                "turn_end",
                vec![
                    ("status", json!("cancelled")),
                    ("reason", json!("cancelled")),
                ],
            );
            emit(TurnEvent::Notice(CANCELLED_NOTICE));
            finish(state, summary);
            return Ok(output);
        }

        // The answer goes straight to the user, not through a parent: the egress
        // guardrail applies, and in redact mode the redacted copy is what is kept.
        let answer = secrets.redact(&answer);
        let (egress, redacted) = self.guardrails.inspect_egress(&answer);
        let text = if self.guardrails.egress == EgressMode::Redact {
            redacted
        } else {
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
        }
        emit(TurnEvent::Text(&text));
        if egress.flagged() {
            emit(TurnEvent::Notice(&format!(
                "\n[guardrail] output flagged: {}\n",
                egress.summary()
            )));
        }
        state.messages.push(message(Role::User, &user_text));
        state.messages.push(message(Role::Assistant, &text));
        state.usage = state.usage + usage;
        state.steps.push(Step {
            index: state.steps.len() as u64 + 1,
            phase: AgentPhase::Execute,
            thought: text.clone(),
            tool_calls: Vec::new(),
            results: Vec::new(),
            usage,
        });
        state.phase = AgentPhase::Done;
        self.recorder.event(
            &sid,
            "turn_end",
            vec![("status", json!("done")), ("agent", json!(specialty.name))],
        );
        if let Some(memory) = &self.memory {
            let _ = memory.add_episode(&sid, &task, &text, &Default::default());
        }
        finish(state, summary);
        Ok(output)
    }

    /// The skills `specialty` preloads that are installed; a missing one is noted.
    pub(crate) fn preloaded_skills(&self, specialty: &Specialty) -> Vec<Arc<crate::skills::Skill>> {
        let mut preloaded = Vec::new();
        for name in &specialty.skills {
            match self.context.skills.get(name) {
                Some(skill) => preloaded.push(Arc::clone(skill)),
                None => self.notice(format!(
                    "[agent] specialty '{}' names the skill '{name}', which is not installed; \
                     it runs without it.",
                    specialty.name
                )),
            }
        }
        preloaded
    }

    /// The conversation so far, summarized by the compaction summarizer (its
    /// prompt, its cap, and the cheap route when routing is on), for `/agent
    /// --with-context`. `None` with nothing to summarize or when the call fails.
    async fn conversation_summary(&self, state: &AgentState) -> Option<(String, Usage)> {
        if state.messages.is_empty() {
            return None;
        }
        let written = self.write_summary(&state.messages).await;
        written.outcome.ok().map(|text| (text, written.usage))
    }

    /// `run --agent`: make `specialty` the main agent of this harness. Its
    /// instructions go ahead of the base identity, its tools narrow the main loop
    /// (with no `tools` line, all but `delegate`), and its approval mode and
    /// always-asked tools apply to the turn itself. The caller resolves the model,
    /// the step budget, and the approval beforehand.
    pub fn become_specialty(&mut self, specialty: &Specialty) {
        let preloaded = self.preloaded_skills(specialty);
        let scope = Self::specialty_scope(specialty, &preloaded, &self.context);
        self.context.scope = Some(Arc::new(scope));
        self.context.can_delegate = self.delegation_offered(&self.context, 0);
        self.subagents
            .set_main(Some((Self::specialty_persona(specialty), preloaded)));
    }

    /// The approval `run --agent` needs before it starts.
    pub fn approve_main_specialty(&self, specialty: &Specialty, goal: &str) -> Result<(), String> {
        if specialty.worktree {
            return Err(format!(
                "the specialty '{}' asks for `isolation: worktree`, which `run --agent` refuses: \
                 nobody would be there to take the diff",
                specialty.name
            ));
        }
        self.approve_specialty(specialty, &synthetic_call(&specialty.name, goal, 0))
    }
}
