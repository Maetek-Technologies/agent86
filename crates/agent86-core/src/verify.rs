//! The verification gate, beyond Python (slice 1 of the local-model work in
//! `docs/PROJECT_PLAN.md`).
//!
//! With `[verify] command` set, a turn that changed the workspace does not end on
//! the model's word: when the model gives its final answer, the harness runs the
//! command as a `run_command` call (through the approval gate and the sandbox
//! executor, as the model's own call would be), and a failure goes back to the
//! model as a user message holding the command's output, up to `max_attempts`
//! times. After that, or when the turn is out of steps, the answer stands, with
//! the failure said. A turn that only read things is never checked.
//!
//! The command is the user's: it comes from their own config, or from a project
//! file only once that file is trusted (`verify` is not in `trust::TRUST_FREE`).

use crate::agent::{observation, Harness, TurnEvent};
use crate::config::VerifyConfig;
use crate::types::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::Mutex;

/// How much of a failing command's output goes back to the model: its end, where
/// test runners put their summary.
pub const FEEDBACK_CHARS: usize = 6000;

/// Tools whose successful call means the turn may have changed the workspace.
pub const CHANGING_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "run_command",
    "python_exec",
    "delegate",
];

/// The gate's settings and what it found in the last turn.
#[derive(Debug, Default)]
pub struct Verify {
    /// The command; `None` turns the gate off.
    pub command: Option<String>,
    /// Failures sent back to the model in one turn before the answer stands.
    pub max_attempts: u32,
    last: Mutex<Option<Verification>>,
}

impl Verify {
    /// The gate as `[verify]` configures it: off when `command` is blank.
    pub fn from_config(config: &VerifyConfig) -> Self {
        Self::new(Some(config.command.as_str()), config.max_attempts)
    }

    /// A gate running `command` (off when it is blank or `None`).
    pub fn new(command: Option<&str>, max_attempts: u32) -> Self {
        Self {
            command: command
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(str::to_owned),
            max_attempts,
            last: Mutex::new(None),
        }
    }

    /// What the gate found in the last turn, if it was on (`run --json`'s
    /// `verification`).
    pub fn last(&self) -> Option<Verification> {
        self.last.lock().ok().and_then(|last| last.clone())
    }

    pub(crate) fn set_last(&self, verification: Option<Verification>) {
        if let Ok(mut last) = self.last.lock() {
            *last = verification;
        }
    }
}

/// What the gate did in one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Verification {
    pub command: String,
    /// `passed`, `failed` (the answer stands with the command still failing), or
    /// `skipped` (`reason` says why).
    pub status: VerifyStatus,
    /// Times the command ran.
    pub runs: u32,
    /// Failures sent back to the model.
    pub sent_back: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VerifyStatus {
    Passed,
    Failed,
    Skipped,
}

/// The gate's progress through one turn.
#[derive(Debug, Default)]
pub(crate) struct VerifyRun {
    /// A call that may have changed the workspace succeeded.
    pub changed: bool,
    pub runs: u32,
    pub sent_back: u32,
}

impl VerifyRun {
    /// Note a finished tool call.
    pub fn saw(&mut self, tool: &str, ok: bool) {
        if ok && CHANGING_TOOLS.contains(&tool) {
            self.changed = true;
        }
    }
}

/// What the loop does with the model's final answer.
pub(crate) enum Gate {
    /// The answer stands.
    Stand,
    /// The answer goes back: this user message, then another step.
    SendBack(String),
}

/// The message a failure goes back to the model as.
pub fn feedback(command: &str, exit: Option<i64>, output: &str) -> String {
    let status = match exit {
        Some(code) => format!("failed with exit code {code}"),
        None => "failed".to_owned(),
    };
    format!(
        "[verification] The check `{command}` {status}, so the task is not done yet. \
         Find and fix the cause, then give your final answer again.\n\n{}",
        tail(output, FEEDBACK_CHARS)
    )
}

/// The last `max` characters of `text`, saying how much was left out.
pub fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(count - max).collect();
    format!(
        "[... {} earlier characters left out ...]\n{kept}",
        count - max
    )
}

impl Harness {
    /// Check the model's final answer (see the module docs). `steps_left` says
    /// whether another step may follow; with none, a failure is said and the
    /// answer stands.
    pub(crate) fn verify_answer(
        &self,
        run: &mut VerifyRun,
        steps_left: bool,
        sid: &str,
        emit: &mut impl FnMut(TurnEvent<'_>),
    ) -> Gate {
        let Some(command) = self.verify.command.clone() else {
            return Gate::Stand;
        };
        let record = |status, reason: Option<String>, run: &VerifyRun| {
            self.verify.set_last(Some(Verification {
                command: command.clone(),
                status,
                runs: run.runs,
                sent_back: run.sent_back,
                reason,
            }));
        };
        if !run.changed {
            record(VerifyStatus::Skipped, Some("nothing changed".into()), run);
            return Gate::Stand;
        }
        if self.cancel.is_cancelled() {
            record(VerifyStatus::Skipped, Some("cancelled".into()), run);
            return Gate::Stand;
        }

        let mut arguments = Map::new();
        arguments.insert("command".into(), Value::String(command.clone()));
        let call = ToolCall {
            id: format!("verify_{}", run.runs + 1),
            name: "run_command".into(),
            arguments,
        };
        // The gate decides as for the model's own call, so `deny`, a declined
        // prompt, or a scope without `run_command` keeps the check from running.
        let (refused, _) = self.resolve_call(&call, sid);
        let result = match refused {
            Some(result) => {
                let reason = observation(&result);
                emit(TurnEvent::Notice(&format!(
                    "\n[verify] `{command}` not run: {reason}\n"
                )));
                self.record_verify(sid, &command, None, run, "skipped");
                record(VerifyStatus::Skipped, Some(reason), run);
                return Gate::Stand;
            }
            None => self.dispatch(&call, sid),
        };
        run.runs += 1;
        let exit = result.metadata.get("returncode").and_then(Value::as_i64);
        if result.ok {
            emit(TurnEvent::Notice(&format!(
                "\n[verify] `{command}` passed\n"
            )));
            self.record_verify(sid, &command, exit, run, "passed");
            record(VerifyStatus::Passed, None, run);
            return Gate::Stand;
        }
        if run.sent_back < self.verify.max_attempts && steps_left {
            run.sent_back += 1;
            emit(TurnEvent::Notice(&format!(
                "\n[verify] `{command}` failed; sent back to the model ({} of {})\n",
                run.sent_back, self.verify.max_attempts
            )));
            self.record_verify(sid, &command, exit, run, "sent_back");
            let (output, _) = self.guardrails.observe(observation(&result));
            return Gate::SendBack(feedback(&command, exit, &output));
        }
        emit(TurnEvent::Notice(&format!(
            "\n[verify] `{command}` still fails; the answer stands unverified\n"
        )));
        self.record_verify(sid, &command, exit, run, "failed");
        record(VerifyStatus::Failed, None, run);
        Gate::Stand
    }

    /// A `verify` trace event (agent86's own kind; Python has no gate).
    fn record_verify(
        &self,
        sid: &str,
        command: &str,
        exit: Option<i64>,
        run: &VerifyRun,
        outcome: &str,
    ) {
        self.recorder.event(
            sid,
            "verify",
            vec![
                ("command", json!(command)),
                ("outcome", json!(outcome)),
                ("returncode", json!(exit)),
                ("runs", json!(run.runs)),
                ("sent_back", json!(run.sent_back)),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_command_turns_the_gate_off() {
        assert_eq!(Verify::new(Some("  "), 2).command, None);
        assert_eq!(Verify::new(None, 2).command, None);
        assert_eq!(
            Verify::new(Some(" cargo test "), 2).command.as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn only_a_successful_change_counts() {
        let mut run = VerifyRun::default();
        run.saw("read_file", true);
        run.saw("write_file", false);
        assert!(!run.changed);
        run.saw("edit_file", true);
        assert!(run.changed);
    }

    #[test]
    fn feedback_keeps_the_end_of_the_output() {
        let output = format!("{}FAILED (failures=1)", "x".repeat(FEEDBACK_CHARS));
        let text = feedback("python -m unittest", Some(1), &output);
        assert!(text
            .starts_with("[verification] The check `python -m unittest` failed with exit code 1"));
        assert!(text.ends_with("FAILED (failures=1)"));
        assert!(text.contains("[... 19 earlier characters left out ...]"));
        assert_eq!(tail("short", 10), "short");
    }
}
