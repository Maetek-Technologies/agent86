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
#[derive(Debug, Clone, PartialEq)]
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
    /// Beyond Python: show the whole detail, never capped, because what is approved
    /// is all of it (a project specialty's file, whose every line becomes prompt text).
    pub full: bool,
    /// Beyond Python: the specialist sub-agent asking (or, for a project specialty's
    /// own approval, its name), so a surface can say who asks.
    pub agent: Option<String>,
    /// Beyond Python: the file text the call would change, before and after, for
    /// a surface that shows a reviewable diff (an editor over ACP). Built from the
    /// expanded call, then redacted, as `detail` is.
    pub change: Option<crate::tools::FileChange>,
    /// Beyond Python: the sub-agent the call comes from, with its card, worktree, and
    /// spend so far, for a surface that shows who asks (an editor's approval inbox).
    /// `None` for the main agent's calls.
    pub asker: Option<Box<Asker>>,
}

/// The sub-agent a question comes from (beyond Python), as it stood when the
/// question was built: an editor shows it beside the question, so the user knows
/// which of several sub-agents running at once asks, where it works, and what it
/// has cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Asker {
    /// The provider id of the card it reports on (its top-level delegation's call,
    /// or `/agent`'s synthetic one), when the surface observes sub-agents.
    pub card: Option<String>,
    /// Its per-spawn number, as sub-agent events and the trace carry it.
    pub run: u64,
    /// The specialty or role it runs as.
    pub agent: String,
    pub depth: u32,
    /// The git worktree it is jailed to: the worktree's directory and its branch.
    pub worktree: Option<(String, String)>,
    /// Model calls it has made so far.
    pub steps: u32,
    /// What it and anything it delegated have spent so far.
    pub cost_usd: f64,
}

impl Asker {
    /// The sub-agent `ctx` belongs to, or `None` for the main agent's context.
    pub fn of(ctx: &ToolContext) -> Option<Self> {
        let run = ctx.subagent.as_ref()?;
        let (steps, cost_usd) = run.progress();
        Some(Self {
            card: ctx.card.as_ref().map(|card| card.id.clone()),
            run: run.run,
            agent: run.name.clone(),
            depth: run.depth,
            worktree: ctx.worktree.as_ref().map(|worktree| {
                (
                    crate::tools::display_path(&worktree.path),
                    worktree.branch.clone(),
                )
            }),
            steps,
            cost_usd,
        })
    }
}

/// What a queued question is (beyond Python).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    /// May a tool call run?
    Approval,
    /// May a project specialty run?
    Specialty,
    /// Raise a spent limit?
    Limit,
    /// Enter a secret.
    Secret,
    /// Answer a surface a `render_ui` call waits on (beyond Python; ADR 0005).
    Ui,
    /// Answer an `ask_user` call's questions (beyond Python; ADR 0006).
    Ask,
}

impl QuestionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Specialty => "specialty",
            Self::Limit => "limit",
            Self::Secret => "secret",
            Self::Ui => "ui",
            Self::Ask => "ask",
        }
    }
}

/// A question in the [`PromptQueue`], described for a surface that lists the
/// questions waiting (beyond Python). It names who asks and about what, never the
/// arguments or a secret's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub kind: QuestionKind,
    /// The tool an approval is for.
    pub tool: Option<String>,
    /// The call it belongs to, by the id the gate sees (`<card>/<n>` for a
    /// sub-agent's call).
    pub call: Option<String>,
    /// The sub-agent asking, when one is.
    pub agent: Option<String>,
    /// The provider id of the asking sub-agent's card.
    pub card: Option<String>,
    /// What is asked about beyond the tool: the project specialty to run, or the
    /// limit to raise.
    pub subject: Option<String>,
}

impl Question {
    pub fn new(kind: QuestionKind) -> Self {
        Self {
            kind,
            tool: None,
            call: None,
            agent: None,
            card: None,
            subject: None,
        }
    }

    /// The question about `call`, whose prompt shows `preview`.
    pub fn about(
        kind: QuestionKind,
        tool: &str,
        call: &ToolCall,
        preview: &ApprovalPreview,
    ) -> Self {
        let asker = preview.asker.as_ref();
        Self {
            tool: Some(tool.to_owned()),
            call: Some(call.id.clone()),
            agent: asker.map(|asker| asker.agent.clone()),
            card: asker.and_then(|asker| asker.card.clone()),
            ..Self::new(kind)
        }
    }
}

/// Told the question being asked, if any, and those waiting behind it in the order
/// they will be asked, whenever either changes. Called with the queue locked, so it
/// must not block or ask anything itself.
pub type QueueWatcher = std::sync::Arc<dyn Fn(Option<&Question>, &[Question]) + Send + Sync>;

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

/// [`ApprovalPrompt`] for a surface that has to say which call it asks about
/// (beyond Python): the ACP server names the call's id in its permission request, so
/// the editor can match the question to the tool call it already showed.
pub type CallApprovalPrompt = Box<dyn Fn(&ToolCall, &ApprovalPreview) -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub approved: bool,
    pub reason: &'static str,
}

/// One question at a time (beyond Python): approval, limit, and secret prompts all
/// pass through one queue, because sub-agents running in parallel can each reach a
/// prompt, and a terminal or an editor answers one at a time. Questions are asked
/// in the order they arrived, and a watcher can be told which are waiting
/// ([`PromptQueue::set_watcher`]), so an editor can list them. A waiter that gets
/// its turn after the turn was cancelled declines without asking, so a cancel is
/// not followed by each queued question in turn. Clones share the queue.
#[derive(Clone, Default)]
pub struct PromptQueue {
    inner: std::sync::Arc<QueueInner>,
}

#[derive(Default)]
struct QueueInner {
    state: Mutex<QueueState>,
    /// Signalled whenever the question being asked ends or a waiter gives up its
    /// turn, so the next in line can go.
    turn_over: std::sync::Condvar,
    cancel: Mutex<Option<crate::agent::CancelToken>>,
    watcher: Mutex<Option<QueueWatcher>>,
}

#[derive(Default)]
struct QueueState {
    next: u64,
    asking: Option<Question>,
    /// By ticket, in arrival order.
    waiting: std::collections::VecDeque<(u64, Question)>,
}

/// Ends the question being asked, even if its prompt panics.
struct Asked<'a>(&'a PromptQueue);

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.asking = None;
        // With a question waiting, its turn reports next (asked, or given up), so
        // a watcher never sees the gap between the two.
        if state.waiting.is_empty() {
            self.0.report(&state);
        }
        self.0.inner.turn_over.notify_all();
    }
}

impl std::fmt::Debug for PromptQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PromptQueue")
    }
}

impl PromptQueue {
    /// Decline queued questions once `token` (the turn's) is cancelled.
    pub fn watch(&self, token: crate::agent::CancelToken) {
        *self
            .inner
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(token);
    }

    /// Tell `watcher` (or, with `None`, nobody) what is asked and waiting from now on.
    pub fn set_watcher(&self, watcher: Option<QueueWatcher>) {
        *self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = watcher;
    }

    /// Ask `question` with `ask` once every question that arrived before it has
    /// been asked, or `None` when the turn was cancelled while it waited.
    pub fn ask<T>(&self, question: Question, ask: impl FnOnce() -> T) -> Option<T> {
        let mut state = self.lock();
        let waited = state.asking.is_some() || !state.waiting.is_empty();
        let question = if waited {
            let ticket = state.next;
            state.next += 1;
            state.waiting.push_back((ticket, question));
            self.report(&state);
            while state.asking.is_some()
                || state.waiting.front().map(|(first, _)| *first) != Some(ticket)
            {
                state = self
                    .inner
                    .turn_over
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state
                .waiting
                .pop_front()
                .map_or_else(|| unreachable!("first in line"), |(_, question)| question)
        } else {
            question
        };
        if self.cancelled() {
            if waited {
                self.report(&state);
                self.inner.turn_over.notify_all();
            }
            return None;
        }
        state.asking = Some(question);
        self.report(&state);
        drop(state);
        let _asked = Asked(self);
        Some(ask())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn cancelled(&self) -> bool {
        self.inner
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(crate::agent::CancelToken::is_cancelled)
    }

    /// Tell the watcher, if there is one, about `state`.
    fn report(&self, state: &QueueState) {
        let watcher = self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(watcher) = watcher {
            let waiting: Vec<Question> = state
                .waiting
                .iter()
                .map(|(_, question)| question.clone())
                .collect();
            watcher(state.asking.as_ref(), &waiting);
        }
    }
}

/// Decides whether a call may run, asking a human when the mode says so.
pub struct ApprovalGate {
    /// Behind a lock, so the interactive interface can switch it (Shift+Tab) while a
    /// turn holds the harness; the next call is decided under the new mode, as in Python.
    mode: Mutex<ApprovalMode>,
    pub prompt: Option<ApprovalPrompt>,
    /// Asked in place of `prompt` when set.
    pub call_prompt: Option<CallApprovalPrompt>,
    /// Every question goes through it, one at a time (beyond Python).
    pub queue: PromptQueue,
    /// Asked in place of the others about a project specialty's file (beyond
    /// Python): an editor offers one remembering option for it, which a call's
    /// prompt cannot.
    pub specialty_prompt: Option<CallApprovalPrompt>,
}

impl ApprovalGate {
    pub fn new(mode: ApprovalMode, prompt: Option<ApprovalPrompt>) -> Self {
        Self {
            mode: Mutex::new(mode),
            prompt,
            call_prompt: None,
            queue: PromptQueue::default(),
            specialty_prompt: None,
        }
    }

    /// Ask `preview` of whichever prompt is installed, or `None` when nobody can be
    /// asked.
    fn ask(&self, tool: &dyn Tool, call: &ToolCall, preview: &ApprovalPreview) -> Option<bool> {
        self.ask_about(tool.name(), call, preview)
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_mode(&self, mode: ApprovalMode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    pub fn decide(&self, tool: &dyn Tool, call: &ToolCall, ctx: &ToolContext) -> ApprovalDecision {
        let decision = |approved, reason| ApprovalDecision { approved, reason };
        let scope = ctx.scope.as_deref();
        // Beyond Python: a specialist sub-agent's list comes before anything else,
        // so nobody is asked about a call that would then be refused.
        if !ctx.scope_allows(tool.name()) {
            return decision(false, "not permitted for this sub-agent");
        }
        // Its approval mode applies only when stricter than the session's, read
        // for each call so a switch mid-turn still counts.
        let mode = crate::tools::stricter(Some(self.mode()), scope.and_then(|s| s.approval))
            .unwrap_or(ApprovalMode::Ask);
        // Beyond Python: a specialty's `confirm` tools are asked about in every mode
        // but `deny`, read-only ones included.
        if let Some(scope) = scope.filter(|s| s.confirm.iter().any(|t| t == tool.name())) {
            if mode == ApprovalMode::Deny {
                return decision(false, "blocked by approval policy (deny)");
            }
            if !self.can_ask() {
                return decision(
                    false,
                    "this sub-agent's calls to this tool need approval, and no prompt is available",
                );
            }
            let mut preview = build_preview(tool, call, ctx);
            preview.warning = Some(format!(
                "The '{}' sub-agent always asks before running {}.",
                scope.label,
                tool.name()
            ));
            preview.agent = Some(scope.label.clone());
            return if self.ask(tool, call, &preview) == Some(true) {
                decision(true, "approved by user")
            } else {
                decision(false, "declined by user")
            };
        }
        if !tool.side_effecting() {
            return decision(true, "read-only");
        }
        // Beyond Python: agent86's own files are asked about whatever the mode,
        // except that `deny` still denies.
        if let Some(path) = tool.guarded_target(&call.arguments, ctx) {
            if mode == ApprovalMode::Deny {
                return decision(false, "blocked by approval policy (deny)");
            }
            if !self.can_ask() {
                return decision(
                    false,
                    "writing agent86's own configuration or skills needs approval, and no \
                     prompt is available",
                );
            }
            let mut preview = build_preview(tool, call, ctx);
            preview.warning = Some(guarded_warning(&path));
            preview.agent = scope.map(|scope| scope.label.clone());
            return if self.ask(tool, call, &preview) == Some(true) {
                decision(true, "approved by user")
            } else {
                decision(false, "declined by user")
            };
        }
        match mode {
            ApprovalMode::Auto => decision(true, "auto-approved"),
            ApprovalMode::Deny => decision(false, "blocked by approval policy (deny)"),
            ApprovalMode::Ask => {
                if !self.can_ask() {
                    return decision(false, "approval required but no prompt available");
                }
                let mut preview = build_preview(tool, call, ctx);
                // Beyond Python: the user is told which sub-agent asks.
                if let Some(scope) = scope {
                    preview.warning = Some(format!("Asked by the '{}' sub-agent.", scope.label));
                    preview.agent = Some(scope.label.clone());
                }
                if self.ask(tool, call, &preview) == Some(true) {
                    decision(true, "approved by user")
                } else {
                    decision(false, "declined by user")
                }
            }
        }
    }

    /// Ask about a project specialty's file: the specialty hook when one is
    /// installed, else the call prompts. `None` when nobody can be asked.
    pub fn ask_specialty(&self, call: &ToolCall, preview: &ApprovalPreview) -> Option<bool> {
        match &self.specialty_prompt {
            Some(prompt) => {
                let mut question =
                    Question::about(QuestionKind::Specialty, "delegate", call, preview);
                question.subject = preview.agent.clone();
                Some(
                    self.queue
                        .ask(question, || prompt(call, preview))
                        .unwrap_or(false),
                )
            }
            None => self.ask_about("delegate", call, preview),
        }
    }

    /// Whether anybody can be asked.
    pub fn can_ask(&self) -> bool {
        self.prompt.is_some() || self.call_prompt.is_some()
    }

    /// Put a question that is not a tool's own to whichever prompt is installed (a
    /// project specialty's approval). `call` is the call it belongs to, for a surface
    /// that has to say which; `name` is what the plain prompt shows. `None` when
    /// nobody can be asked.
    pub fn ask_about(
        &self,
        name: &str,
        call: &ToolCall,
        preview: &ApprovalPreview,
    ) -> Option<bool> {
        if !self.can_ask() {
            return None;
        }
        // A question that waited past a cancel is declined without being put.
        Some(
            self.queue
                .ask(
                    Question::about(QuestionKind::Approval, name, call, preview),
                    || match &self.call_prompt {
                        Some(prompt) => prompt(call, preview),
                        None => self
                            .prompt
                            .as_ref()
                            .is_some_and(|prompt| prompt(name, preview)),
                    },
                )
                .unwrap_or(false),
        )
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
    let expanded = crate::tools::with_secrets(tool, call, ctx)
        .map_or_else(|_| call.arguments.clone(), |call| call.arguments.clone());
    ApprovalPreview {
        summary,
        detail: tool
            .preview(&expanded, ctx)
            .map(|detail| ctx.secrets.redact(&detail))
            .filter(|detail| !detail.is_empty()),
        lexer: tool.preview_lexer(),
        warning: None,
        full: false,
        agent: None,
        change: tool
            .change(&expanded, ctx)
            .map(|change| change.redacted(&ctx.secrets)),
        asker: Asker::of(ctx).map(Box::new),
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
    text.push_str(&show_controls(&if preview.full {
        detail.to_owned()
    } else {
        cap_lines(detail, APPROVAL_DETAIL_LINES)
    }));
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
            full: false,
            agent: None,
            change: None,
            asker: None,
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

    type Views = std::sync::Arc<Mutex<Vec<(Option<String>, Vec<String>)>>>;

    /// A queue whose watcher keeps every view it is shown, by each question's call.
    fn watched_queue() -> (PromptQueue, Views) {
        let queue = PromptQueue::default();
        let views: Views = std::sync::Arc::default();
        let seen = std::sync::Arc::clone(&views);
        queue.set_watcher(Some(std::sync::Arc::new(
            move |asking: Option<&Question>, waiting: &[Question]| {
                let call = |question: &Question| question.call.clone().unwrap_or_default();
                seen.lock()
                    .unwrap()
                    .push((asking.map(call), waiting.iter().map(call).collect()));
            },
        )));
        (queue, views)
    }

    fn question(call: &str) -> Question {
        Question {
            call: Some(call.to_owned()),
            ..Question::new(QuestionKind::Approval)
        }
    }

    /// Wait until the queue has shown `waiting` questions behind the one asked.
    fn until_waiting(views: &Views, waiting: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while views
            .lock()
            .unwrap()
            .last()
            .is_none_or(|(_, queued)| queued.len() != waiting)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                views.lock().unwrap()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn questions_are_asked_in_arrival_order_and_the_watcher_sees_who_waits() {
        let (queue, views) = watched_queue();
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (release, hold) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let hold = hold;
                queue.ask(question("a"), || {
                    asked.lock().unwrap().push("a");
                    hold.recv().unwrap();
                })
            });
            until_waiting(&views, 0);
            let mut rest = Vec::new();
            for (call, behind) in [("b", 1), ("c", 2)] {
                let (queue, asked) = (&queue, &asked);
                rest.push(
                    scope.spawn(move || {
                        queue.ask(question(call), || asked.lock().unwrap().push(call))
                    }),
                );
                until_waiting(&views, behind);
            }
            release.send(()).unwrap();
            assert!(first.join().unwrap().is_some());
            for waiter in rest {
                assert!(waiter.join().unwrap().is_some());
            }
        });
        assert_eq!(*asked.lock().unwrap(), ["a", "b", "c"]);
        let view = |asking: Option<&str>, waiting: &[&str]| {
            (
                asking.map(str::to_owned),
                waiting
                    .iter()
                    .map(|call| (*call).to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            *views.lock().unwrap(),
            [
                view(Some("a"), &[]),
                view(Some("a"), &["b"]),
                view(Some("a"), &["b", "c"]),
                view(Some("b"), &["c"]),
                view(Some("c"), &[]),
                view(None, &[]),
            ]
        );
    }

    #[test]
    fn questions_waiting_past_a_cancel_are_declined_without_being_asked() {
        let (queue, views) = watched_queue();
        let token = crate::agent::CancelToken::default();
        queue.watch(token.clone());
        let (release, hold) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let hold = hold;
                queue.ask(question("a"), || hold.recv().unwrap())
            });
            until_waiting(&views, 0);
            let second =
                scope.spawn(|| queue.ask(question("b"), || panic!("asked after a cancel")));
            until_waiting(&views, 1);
            token.cancel();
            release.send(()).unwrap();
            assert!(first.join().unwrap().is_some());
            assert!(second.join().unwrap().is_none());
        });
        assert_eq!(
            views.lock().unwrap().last(),
            Some(&(None, Vec::new())),
            "the declined question left the queue empty"
        );
        // A question arriving after the cancel is declined at once.
        assert!(queue.ask(question("c"), || ()).is_none());
    }

    #[test]
    fn a_sub_agents_call_is_previewed_with_who_asks() {
        let dir = std::env::temp_dir().join(format!("agent86-asker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = ToolContext::new(&dir).unwrap();
        assert_eq!(Asker::of(&ctx), None);
        let run = std::sync::Arc::new(crate::subagent::SubagentRun::new(7, "writer", 2));
        ctx.subagent = Some(run);
        ctx.card = Some(std::sync::Arc::new(crate::subagent::Card::new(
            "call_9", true,
        )));
        let asker = Asker::of(&ctx).unwrap();
        assert_eq!(
            asker,
            Asker {
                card: Some("call_9".into()),
                run: 7,
                agent: "writer".into(),
                depth: 2,
                worktree: None,
                steps: 0,
                cost_usd: 0.0,
            }
        );
        let call = ToolCall {
            id: "call_9/3".into(),
            name: "write_file".into(),
            arguments: serde_json::Map::new(),
        };
        let shown = ApprovalPreview {
            asker: Some(Box::new(asker)),
            ..preview("{}", None)
        };
        let question = Question::about(QuestionKind::Approval, "write_file", &call, &shown);
        assert_eq!(question.call.as_deref(), Some("call_9/3"));
        assert_eq!(question.agent.as_deref(), Some("writer"));
        assert_eq!(question.card.as_deref(), Some("call_9"));
        let _ = std::fs::remove_dir_all(&dir);
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
