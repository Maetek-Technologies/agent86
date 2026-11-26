//! The stall nudge, beyond Python (slice 3 of the local-model work in
//! `docs/PROJECT_PLAN.md`). Python's only answer to a model going in circles is
//! the circuit breaker: three tool errors *in a row* end the turn. A model that
//! edits, runs the tests, and gets the same failure again never trips it, since
//! every edit resets the streak, and spends its whole step budget that way.
//!
//! [`StallWatch`] watches a turn's tool results for three shapes of going in
//! circles, and the loop sends the model one reminder (a user-role note, a
//! `[stall]` notice for the surfaces, and a `stall` trace event) for each shape,
//! at most once a turn:
//!
//! - [`StallKind::RepeatedCall`]: the same call, with the same arguments, failed
//!   the same way as before, with nothing changed in between.
//! - [`StallKind::RepeatedFailure`]: one tool has failed the same way
//!   [`REPEATED_FAILURES`] times, even with changes in between, so the fixes so far
//!   have not reached the cause.
//! - [`StallKind::RepeatedReads`]: [`REPEATED_READS`] reads that repeat an
//!   earlier one (the same tool and arguments) with nothing changed in between.
//!   Reads of something new never count, however many: reading a package in one
//!   batch, or following a chain of files one at a time, is not going in circles.
//!
//! "The same way" compares what the model was shown, with timings and addresses
//! masked ([`signature`]), so a test run that only took longer still matches.
//! The thresholds are starting points for the eval set to decide, not claims.

use crate::types::{ToolCall, ToolResult};
use regex::Regex;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

/// Failures of one tool with one [`signature`] that make a
/// [`StallKind::RepeatedFailure`].
pub const REPEATED_FAILURES: u32 = 3;

/// Repeated reads, since the last change, that make a [`StallKind::RepeatedReads`].
/// In the eval set, the runs that went in circles re-read the same files 6 and 24
/// times; of 216 others, none passed 2.
pub const REPEATED_READS: u32 = 3;

/// Tools that only look.
pub const READ_TOOLS: &[&str] = &["read_file", "list_dir", "search_files", "find_files"];

/// Which way a turn is going in circles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StallKind {
    RepeatedCall,
    RepeatedFailure,
    RepeatedReads,
}

impl StallKind {
    /// The trace's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepeatedCall => "repeated_call",
            Self::RepeatedFailure => "repeated_failure",
            Self::RepeatedReads => "repeated_reads",
        }
    }
}

/// A stall [`StallWatch`] saw, for the loop to remind the model of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    pub kind: StallKind,
    /// The tool, for the two failure kinds.
    pub tool: Option<String>,
    /// How many times (failures, or repeated reads).
    pub count: u32,
}

impl Stall {
    /// The note the model gets, as a user message after the step's results.
    pub fn note(&self) -> String {
        let tool = self.tool.as_deref().unwrap_or("the tool");
        match self.kind {
            StallKind::RepeatedCall => format!(
                "[stall] That call to `{tool}` is the same as an earlier one and failed the \
                 same way, with nothing changed in between, so running it again will not help. \
                 Read the error again, check the assumption behind the call, and try something \
                 different."
            ),
            StallKind::RepeatedFailure => format!(
                "[stall] `{tool}` has now failed the same way {} times, though things changed in \
                 between, so the fixes so far have not reached the cause. Stop adjusting the \
                 same thing: re-read the error and the code it points at, work out what really \
                 causes it, and change your approach.",
                self.count
            ),
            StallKind::RepeatedReads => format!(
                "[stall] You have read the same thing again {} times with nothing changed in \
                 between, so reading it again will not tell you more. If you know enough to \
                 answer or to make the change, do it now; otherwise decide exactly what you are \
                 still missing and look only for that.",
                self.count
            ),
        }
    }

    /// The notice the surfaces show.
    pub fn notice(&self) -> String {
        let what = match self.kind {
            StallKind::RepeatedCall => format!(
                "`{}` repeated a failed call",
                self.tool.as_deref().unwrap_or_default()
            ),
            StallKind::RepeatedFailure => format!(
                "`{}` failed the same way {} times",
                self.tool.as_deref().unwrap_or_default(),
                self.count
            ),
            StallKind::RepeatedReads => format!("{} repeated reads", self.count),
        };
        format!("\n[stall] {what}; reminded the model\n")
    }
}

/// `observation` with what changes between identical failures masked (timings
/// such as `0.003s` or `12 ms`, and `0x...` addresses), hashed.
pub fn signature(observation: &str) -> u64 {
    static VOLATILE: OnceLock<Regex> = OnceLock::new();
    let volatile = VOLATILE.get_or_init(|| {
        Regex::new(r"(?i)\b\d+(?:\.\d+)?\s?(?:s|ms|sec|secs|seconds)\b|0x[0-9a-f]+")
            .expect("valid regex")
    });
    let masked = volatile.replace_all(observation.trim(), "#");
    let mut hasher = DefaultHasher::new();
    masked.hash(&mut hasher);
    hasher.finish()
}

/// The call's arguments as text with every object's keys sorted, so the same
/// call written in another key order (models alternate) is still the same call.
fn arguments_key(call: &ToolCall) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), sorted(&map[key])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sorted(&Value::Object(call.arguments.clone()))).unwrap_or_default()
}

/// One turn's watch (see the module docs).
#[derive(Debug, Default)]
pub struct StallWatch {
    enabled: bool,
    /// Successful changes so far (`crate::verify::CHANGING_TOOLS`).
    changes: u32,
    /// Each failed call (tool and arguments): its signature and the change count
    /// when it failed.
    failed_calls: HashMap<(String, String), (u64, u32)>,
    /// Failures per tool and signature.
    failures: HashMap<(String, u64), u32>,
    /// Each successful read (tool and arguments): the change count when it ran.
    read_calls: HashMap<(String, String), u32>,
    /// Reads that repeated an earlier one since the last change.
    repeated_reads: u32,
    nudged: Vec<StallKind>,
    pending: Option<Stall>,
}

impl StallWatch {
    /// A watch that sees nothing when `enabled` is false.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    /// Fold in one result, with the text the model was shown for it.
    pub fn saw(&mut self, call: &ToolCall, result: &ToolResult, observation: &str) {
        if !self.enabled {
            return;
        }
        let name = call.name.as_str();
        if READ_TOOLS.contains(&name) && result.ok {
            let key = (name.to_owned(), arguments_key(call));
            if self.read_calls.insert(key, self.changes) == Some(self.changes) {
                self.repeated_reads += 1;
                if self.repeated_reads >= REPEATED_READS {
                    self.raise(StallKind::RepeatedReads, Some(name), self.repeated_reads);
                }
            }
            return;
        }
        if result.ok {
            if crate::verify::CHANGING_TOOLS.contains(&name) {
                self.changes += 1;
                self.repeated_reads = 0;
            }
            return;
        }
        let sig = signature(observation);
        let key = (name.to_owned(), arguments_key(call));
        let previous = self.failed_calls.insert(key, (sig, self.changes));
        if previous == Some((sig, self.changes)) {
            self.raise(StallKind::RepeatedCall, Some(name), 2);
        }
        let count = self.failures.entry((name.to_owned(), sig)).or_default();
        *count += 1;
        let count = *count;
        if count >= REPEATED_FAILURES {
            self.raise(StallKind::RepeatedFailure, Some(name), count);
        }
    }

    fn raise(&mut self, kind: StallKind, tool: Option<&str>, count: u32) {
        if self.nudged.contains(&kind) || self.pending.is_some() {
            return;
        }
        self.nudged.push(kind);
        self.pending = Some(Stall {
            kind,
            tool: tool.map(str::to_owned),
            count,
        });
    }

    /// The stall to remind the model of after this step, if any: one a step.
    pub fn take(&mut self) -> Option<Stall> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    fn result(ok: bool) -> ToolResult {
        ToolResult {
            call_id: "c".into(),
            name: "t".into(),
            ok,
            content: String::new(),
            error: None,
            metadata: Default::default(),
        }
    }

    fn tests_run() -> ToolCall {
        call("run_command", json!({"command": "python -m unittest"}))
    }

    #[test]
    fn the_same_failed_call_with_nothing_changed_is_a_repeated_call() {
        let mut watch = StallWatch::new(true);
        watch.saw(&tests_run(), &result(false), "FAILED in 0.003s");
        assert_eq!(watch.take(), None);
        watch.saw(&tests_run(), &result(false), "FAILED in 0.010s");
        let stall = watch.take().unwrap();
        assert_eq!(stall.kind, StallKind::RepeatedCall);
        assert_eq!(stall.tool.as_deref(), Some("run_command"));
        assert!(stall
            .note()
            .starts_with("[stall] That call to `run_command`"));
        // Once a turn.
        watch.saw(&tests_run(), &result(false), "FAILED in 0.010s");
        assert_eq!(
            watch.take().map(|s| s.kind),
            Some(StallKind::RepeatedFailure)
        );
        watch.saw(&tests_run(), &result(false), "FAILED in 0.010s");
        assert_eq!(watch.take(), None);
    }

    #[test]
    fn fixes_that_leave_the_failure_unchanged_are_a_repeated_failure() {
        let mut watch = StallWatch::new(true);
        let edit = call("edit_file", json!({"path": "money.py"}));
        for _ in 0..2 {
            watch.saw(&tests_run(), &result(false), "AssertionError: 2.67 != 2.68");
            watch.saw(&edit, &result(true), "ok");
            assert_eq!(watch.take(), None);
        }
        watch.saw(&tests_run(), &result(false), "AssertionError: 2.67 != 2.68");
        let stall = watch.take().unwrap();
        assert_eq!(stall.kind, StallKind::RepeatedFailure);
        assert_eq!(stall.count, 3);
        assert_eq!(
            stall.notice(),
            "\n[stall] `run_command` failed the same way 3 times; reminded the model\n"
        );
    }

    #[test]
    fn a_failure_that_changes_is_progress() {
        let mut watch = StallWatch::new(true);
        let edit = call("edit_file", json!({"path": "money.py"}));
        for failures in 3..6 {
            watch.saw(
                &tests_run(),
                &result(false),
                &format!("FAILED (failures={failures})"),
            );
            watch.saw(&edit, &result(true), "ok");
        }
        assert_eq!(watch.take(), None);
        // A change between two identical calls is not a repeated call.
        watch.saw(&tests_run(), &result(false), "same");
        watch.saw(&edit, &result(true), "ok");
        watch.saw(&tests_run(), &result(false), "same");
        assert_eq!(watch.take(), None);
    }

    #[test]
    fn new_reads_never_count_however_many() {
        // A package read in one batch, or a chain of notes one at a time.
        let mut watch = StallWatch::new(true);
        for index in 0..40 {
            let read = call("read_file", json!({"path": format!("shop/m{index}.py")}));
            watch.saw(&read, &result(true), "text");
        }
        assert_eq!(watch.take(), None);
    }

    #[test]
    fn reading_the_same_thing_again_with_nothing_changed_is_repeated_reads() {
        let mut watch = StallWatch::new(true);
        let read = call("read_file", json!({"path": "a.py"}));
        let search = call("search_files", json!({"pattern": "x", "path": "."}));
        watch.saw(&read, &result(true), "text");
        watch.saw(&search, &result(true), "text");
        // A change in between: reading again may show something new.
        watch.saw(
            &call("edit_file", json!({"path": "a.py"})),
            &result(true),
            "ok",
        );
        watch.saw(&read, &result(true), "text");
        watch.saw(&search, &result(true), "text");
        // A read of something new does not reset the count.
        watch.saw(
            &call("read_file", json!({"path": "b.py"})),
            &result(true),
            "text",
        );
        watch.saw(&read, &result(true), "text");
        watch.saw(&search, &result(true), "text");
        assert_eq!(watch.take(), None);
        // Keys in another order are the same read.
        let reordered = call("search_files", json!({"path": ".", "pattern": "x"}));
        watch.saw(&reordered, &result(true), "text");
        let stall = watch.take().unwrap();
        assert_eq!(stall.kind, StallKind::RepeatedReads);
        assert_eq!(stall.count, REPEATED_READS);
        assert_eq!(stall.tool.as_deref(), Some("search_files"));
        assert_eq!(
            stall.notice(),
            "\n[stall] 3 repeated reads; reminded the model\n"
        );
        assert!(stall
            .note()
            .starts_with("[stall] You have read the same thing again 3 times"));
        // Once a turn.
        watch.saw(&read, &result(true), "text");
        assert_eq!(watch.take(), None);
    }

    #[test]
    fn the_same_call_in_another_key_order_is_still_a_repeat() {
        // Seen in the eval: `edit_file` sent six times, keys in alternating order.
        let mut watch = StallWatch::new(true);
        let first = call(
            "edit_file",
            json!({"new_string": "x", "old_string": "x", "path": "t.py"}),
        );
        let second = call(
            "edit_file",
            json!({"path": "t.py", "old_string": "x", "new_string": "x"}),
        );
        let error = "old_string and new_string are identical; nothing to do.";
        watch.saw(&first, &result(false), error);
        watch.saw(&second, &result(false), error);
        assert_eq!(watch.take().map(|s| s.kind), Some(StallKind::RepeatedCall));
    }

    #[test]
    fn a_watch_that_is_off_sees_nothing() {
        let mut watch = StallWatch::new(false);
        for _ in 0..5 {
            watch.saw(&tests_run(), &result(false), "same");
        }
        assert_eq!(watch.take(), None);
    }

    #[test]
    fn signatures_ignore_timings_and_addresses_only() {
        assert_eq!(
            signature("Ran 3 tests in 0.003s\nFAILED"),
            signature("Ran 3 tests in 1.250s\nFAILED")
        );
        assert_eq!(signature("<obj at 0x7f3a>"), signature("<obj at 0x91bc>"));
        assert_ne!(signature("failures=2"), signature("failures=1"));
    }
}
