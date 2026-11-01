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
use std::sync::{Mutex, PoisonError};

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
    /// What `detail` is written in (`diff`, `bash`, `python`), as Python's `lexer`.
    pub lexer: Option<&'static str>,
    /// Beyond Python: a caution shown above the detail, for a write into agent86's
    /// own configuration or skills ([`guarded_warning`]).
    pub warning: Option<String>,
}

/// The caution for a write into agent86's own configuration or skills.
pub fn guarded_warning(path: &str) -> String {
    format!(
        "This writes agent86's own configuration or skills ({}), which changes what \
         agent86 runs or is told from now on. Asked even in auto mode.",
        crate::escape::show_controls(path)
    )
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
    /// Behind a lock, so the interactive interface can switch it (Shift+Tab) while a
    /// turn holds the harness; the next call is decided under the new mode, as in Python.
    mode: Mutex<ApprovalMode>,
    pub prompt: Option<ApprovalPrompt>,
}

impl ApprovalGate {
    pub fn new(mode: ApprovalMode, prompt: Option<ApprovalPrompt>) -> Self {
        Self {
            mode: Mutex::new(mode),
            prompt,
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_mode(&self, mode: ApprovalMode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    pub fn decide(&self, tool: &dyn Tool, call: &ToolCall, ctx: &ToolContext) -> ApprovalDecision {
        let decision = |approved, reason| ApprovalDecision { approved, reason };
        if !tool.side_effecting() {
            return decision(true, "read-only");
        }
        // Beyond Python: agent86's own files are asked about whatever the mode,
        // except that `deny` still denies.
        if let Some(path) = tool.guarded_target(&call.arguments, ctx) {
            if self.mode() == ApprovalMode::Deny {
                return decision(false, "blocked by approval policy (deny)");
            }
            let Some(prompt) = &self.prompt else {
                return decision(
                    false,
                    "writing agent86's own configuration or skills needs approval, and no \
                     prompt is available",
                );
            };
            let mut preview = build_preview(tool, call, ctx);
            preview.warning = Some(guarded_warning(&path));
            return if prompt(tool.name(), &preview) {
                decision(true, "approved by user")
            } else {
                decision(false, "declined by user")
            };
        }
        match self.mode() {
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
///
/// A tool that expands `${secret:NAME}` previews what it will really do (so a file
/// that already holds the value shows no change on that line), and the detail is
/// then redacted, so the user approves the placeholder, never reads the value.
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
            .preview(
                &crate::tools::with_secrets(tool, call, ctx)
                    .map_or_else(|_| call.arguments.clone(), |call| call.arguments.clone()),
                ctx,
            )
            .map(|detail| ctx.secrets.redact(&detail))
            .filter(|detail| !detail.is_empty()),
        lexer: tool.preview_lexer(),
        warning: None,
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
    let color = std::io::stderr().is_terminal() && color_allowed();
    ask(&mut std::io::stderr(), color, tool_name, preview)
}

/// [`terminal_prompt`] on stdout, as Python's REPL asks; the REPL has no JSON
/// output to protect.
pub fn stdout_prompt(tool_name: &str, preview: &ApprovalPreview) -> bool {
    let color = std::io::stdout().is_terminal() && color_allowed();
    ask(&mut std::io::stdout(), color, tool_name, preview)
}

fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// What the terminal prompt prints before reading the answer.
///
/// Beyond Python: the detail, the summary it falls back to, and the tool's name
/// come from the model (an MCP server names its own tools), so their control
/// characters are shown, not sent ([`crate::escape::show_controls`]). Otherwise a
/// `\r` or an escape sequence could draw a harmless command over the one that runs.
pub fn prompt_text(color: bool, tool_name: &str, preview: &ApprovalPreview) -> String {
    use crate::escape::show_controls;
    let detail = preview.detail.as_deref().unwrap_or(&preview.summary);
    let tool_name = show_controls(tool_name);
    let mut text = String::from("\n");
    if let Some(warning) = &preview.warning {
        let warning = show_controls(warning);
        text.push_str(&if color {
            format!("\x1b[1;33mwarning:\x1b[0m {warning}\n")
        } else {
            format!("warning: {warning}\n")
        });
    }
    text.push_str(&show_controls(&cap_lines(detail, APPROVAL_DETAIL_LINES)));
    text.push('\n');
    text.push_str(&if color {
        format!("\x1b[1;33mapprove \x1b[0m\x1b[1m{tool_name}?\x1b[0m\n")
    } else {
        format!("approve {tool_name}?\n")
    });
    text.push_str("  [y/N] ");
    text
}

fn ask(out: &mut dyn Write, color: bool, tool_name: &str, preview: &ApprovalPreview) -> bool {
    let _ = write!(out, "{}", prompt_text(color, tool_name, preview));
    let _ = out.flush();

    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => {
            // Nobody is there to answer: decline, the safe default.
            let _ = writeln!(out, "declined");
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

    fn preview(summary: &str, detail: Option<&str>) -> ApprovalPreview {
        ApprovalPreview {
            summary: summary.to_owned(),
            detail: detail.map(str::to_owned),
            lexer: None,
            warning: None,
        }
    }

    #[test]
    fn the_prompt_shows_control_characters_instead_of_obeying_them() {
        // `\r` would return the cursor and draw `echo hi` over `rm -rf x`.
        let text = prompt_text(
            false,
            "run_command",
            &preview("{}", Some("rm -rf x\recho hi")),
        );
        assert_eq!(text, "\nrm -rf x␍echo hi\napprove run_command?\n  [y/N] ");

        // The summary it falls back to, an escape sequence, and a bidi override.
        let text = prompt_text(
            false,
            "mcp\u{1b}[8m_tool",
            &preview("{\"path\": \"a\u{202e}txt.sh\"}\u{1b}[2K", None),
        );
        assert_eq!(
            text,
            "\n{\"path\": \"a<U+202E>txt.sh\"}␛[2K\napprove mcp␛[8m_tool?\n  [y/N] "
        );
        assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}'));
    }

    #[test]
    fn the_prompts_own_colours_survive() {
        let mut shown = preview("{}", Some("ls"));
        shown.warning = Some("careful".to_owned());
        let text = prompt_text(true, "run_command", &shown);
        assert_eq!(
            text,
            "\n\u{1b}[1;33mwarning:\u{1b}[0m careful\nls\n\
             \u{1b}[1;33mapprove \u{1b}[0m\u{1b}[1mrun_command?\u{1b}[0m\n  [y/N] "
        );
    }
}
