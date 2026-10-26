//! The human-in-the-loop approval gate, ported from Python's
//! `guardrails/policy.py`, and the terminal prompt from `ui/repl.py`.
//!
//! Read-only tools always pass. Side-effecting tools pass under `auto`, are
//! blocked under `deny`, and are put to the user under `ask`. With no prompt
//! (piped stdin, CI, cron), `ask` declines: running unattended requires an
//! explicit `--yes` or `approval = "auto"`.

use crate::pyjson::python_json_dumps;
use crate::tools::{Tool, ToolContext};
use crate::types::{ApprovalMode, ToolCall};
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Write};

/// The one-line argument summary is capped at this many characters.
pub const SUMMARY_LIMIT: usize = 300;
/// The prompt shows at most this many lines of a tool's preview.
pub const APPROVAL_DETAIL_LINES: usize = 40;

/// What the user is asked to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPreview {
    /// The arguments as `json.dumps`, capped at [`SUMMARY_LIMIT`].
    pub summary: String,
    /// The tool's own description of the effect, such as a unified diff.
    pub detail: Option<String>,
}

/// Asks whether one side-effecting call may run: `(tool name, preview) -> approved`.
pub type ApprovalPrompt = Box<dyn Fn(&str, &ApprovalPreview) -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub approved: bool,
    pub reason: &'static str,
}

/// Decides whether a call may run, asking a human when the mode says so.
pub struct ApprovalGate {
    pub mode: ApprovalMode,
    pub prompt: Option<ApprovalPrompt>,
}

impl ApprovalGate {
    pub fn new(mode: ApprovalMode, prompt: Option<ApprovalPrompt>) -> Self {
        Self { mode, prompt }
    }

    pub fn decide(&self, tool: &dyn Tool, call: &ToolCall, ctx: &ToolContext) -> ApprovalDecision {
        let decision = |approved, reason| ApprovalDecision { approved, reason };
        if !tool.side_effecting() {
            return decision(true, "read-only");
        }
        match self.mode {
            ApprovalMode::Auto => decision(true, "auto-approved"),
            ApprovalMode::Deny => decision(false, "blocked by approval policy (deny)"),
            ApprovalMode::Ask => match &self.prompt {
                None => decision(false, "approval required but no prompt available"),
                Some(prompt) => {
                    if prompt(tool.name(), &build_preview(tool, call, ctx)) {
                        decision(true, "approved by user")
                    } else {
                        decision(false, "declined by user")
                    }
                }
            },
        }
    }
}

/// The approval payload: a one-line argument summary plus the tool's own detail.
pub fn build_preview(tool: &dyn Tool, call: &ToolCall, ctx: &ToolContext) -> ApprovalPreview {
    let text = python_json_dumps(&Value::Object(call.arguments.clone()), false);
    let summary = if text.chars().count() <= SUMMARY_LIMIT {
        text
    } else {
        format!(
            "{} ...",
            text.chars().take(SUMMARY_LIMIT).collect::<String>()
        )
    };
    ApprovalPreview {
        summary,
        detail: tool
            .preview(&call.arguments, ctx)
            .filter(|detail| !detail.is_empty()),
    }
}

/// The prompt to install for `agent86 run`: one that asks at the terminal when
/// stdin is a terminal, and none otherwise (so `ask` declines).
pub fn terminal_prompt_if_interactive() -> Option<ApprovalPrompt> {
    std::io::stdin()
        .is_terminal()
        .then(|| Box::new(terminal_prompt) as ApprovalPrompt)
}

/// Ask at the terminal whether one side-effecting call may run. Default: no.
///
/// The preview is printed above the question, because approving a write
/// sight-unseen is what this prompt exists to prevent. Python prints the prompt
/// on stdout; it goes to stderr here so `run --json` output stays one object.
pub fn terminal_prompt(tool_name: &str, preview: &ApprovalPreview) -> bool {
    let detail = preview.detail.as_deref().unwrap_or(&preview.summary);
    let color = std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr);
    let _ = writeln!(stderr, "{}", cap_lines(detail, APPROVAL_DETAIL_LINES));
    if color {
        let _ = writeln!(
            stderr,
            "\x1b[1;33mapprove \x1b[0m\x1b[1m{tool_name}?\x1b[0m"
        );
    } else {
        let _ = writeln!(stderr, "approve {tool_name}?");
    }
    let _ = write!(stderr, "  [y/N] ");
    let _ = stderr.flush();
    drop(stderr);

    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => {
            // Nobody is there to answer: decline, the safe default.
            eprintln!("declined");
            false
        }
        Ok(_) => matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"),
    }
}

/// At most `limit` lines of `text`, saying how many were dropped.
pub fn cap_lines(text: &str, limit: usize) -> String {
    let lines = crate::difflib::splitlines(text);
    if lines.len() <= limit {
        return text.to_owned();
    }
    format!(
        "{}\n… truncated ({} more lines)",
        lines[..limit].join("\n"),
        lines.len() - limit
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_previews_are_capped() {
        let text: Vec<String> = (0..45).map(|i| format!("line {i}")).collect();
        let capped = cap_lines(&text.join("\n"), 40);
        assert!(
            capped.ends_with("line 39\n… truncated (5 more lines)"),
            "{capped}"
        );
        assert_eq!(cap_lines("short", 40), "short");
    }
}
