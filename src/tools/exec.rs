//! Command and Python tools, ported from Python's `tools/builtin/shell.py` and
//! `tools/builtin/python_exec.py`. Both run under the sandbox executor and are
//! side-effecting, so they pass through the approval gate.

use super::files::head_of;
use super::sandbox::{ExecRequest, ExecResult};
use super::{success, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

/// Previews show at most this many lines of a command or snippet.
pub const PREVIEW_MAX_LINES: usize = 60;

pub struct RunCommandTool;

impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace and return its stdout, stderr, and exit \
         code. Runs under the sandbox: scrubbed environment, workspace cwd, and a timeout."
    }

    fn side_effecting(&self) -> bool {
        true
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "command": {
                    "description": "The shell command to execute.",
                    "title": "Command",
                    "type": "string",
                },
            },
            "required": ["command"],
            "type": "object",
        })
    }

    /// The whole command: the one thing the user must read before approving it.
    fn preview(&self, args: &Map<String, Value>, _ctx: &ToolContext) -> Option<String> {
        let command = args.get("command")?.as_str()?;
        Some(head_of(command, PREVIEW_MAX_LINES))
    }

    fn preview_lexer(&self) -> Option<&'static str> {
        Some("bash")
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let command = arguments.string("command", None);
        arguments.finish()?;

        let result = ctx
            .executor
            .run(&ctx.policy, &ExecRequest::Shell(command), None)
            .map_err(|error| ToolError::io(&error))?;
        Ok(report(self.name(), &result))
    }
}

pub struct PythonExecTool;

impl Tool for PythonExecTool {
    fn name(&self) -> &str {
        "python_exec"
    }

    fn description(&self) -> &str {
        "Execute a Python 3 snippet in a fresh sandboxed process and return its stdout, \
         stderr, and exit code. Print results you want to see."
    }

    fn side_effecting(&self) -> bool {
        true
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "code": {
                    "description": "Python source to execute. Use print() for output.",
                    "title": "Code",
                    "type": "string",
                },
            },
            "required": ["code"],
            "type": "object",
        })
    }

    /// The whole snippet: a truncated argument summary hides the line that matters.
    fn preview(&self, args: &Map<String, Value>, _ctx: &ToolContext) -> Option<String> {
        let code = args.get("code")?.as_str()?;
        Some(head_of(code, PREVIEW_MAX_LINES))
    }

    fn preview_lexer(&self) -> Option<&'static str> {
        Some("python")
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let code = arguments.string("code", None);
        arguments.finish()?;

        // Code goes over stdin, so multi-line programs work.
        let request = ExecRequest::Argv(ctx.executor.python_argv());
        let result = ctx
            .executor
            .run(&ctx.policy, &request, Some(&code))
            .map_err(|error| ToolError::io(&error))?;
        Ok(report(self.name(), &result))
    }
}

/// `exit code: N`, then stdout and stderr when they say anything.
fn report(name: &str, result: &ExecResult) -> ToolResult {
    let mut parts = vec![format!("exit code: {}", result.returncode)];
    if !result.stdout.trim().is_empty() {
        parts.push(format!("stdout:\n{}", result.stdout.trim_end()));
    }
    if !result.stderr.trim().is_empty() {
        parts.push(format!("stderr:\n{}", result.stderr.trim_end()));
    }
    let mut tool_result = success(name, parts.join("\n"));
    tool_result.ok = result.ok();
    tool_result
        .metadata
        .insert("returncode".to_owned(), json!(result.returncode));
    tool_result
        .metadata
        .insert("timed_out".to_owned(), json!(result.timed_out));
    tool_result
}
