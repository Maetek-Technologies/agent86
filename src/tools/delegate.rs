//! `delegate`, ported from Python's `tools/builtin/delegate.py`: model-driven
//! sub-agent spawning, with the main agent as supervisor.
//!
//! The model names a role and a self-contained task; a fresh sub-agent with the
//! same tools, sandbox, and guardrails runs it to completion, and its answer is
//! the tool's output. The run itself belongs to the harness, which awaits the
//! sub-agent's model calls (`Harness::delegate`); this tool carries the schema
//! and the argument rules.

use super::{failure, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

pub const NAME: &str = "delegate";

/// Hand a focused subtask to a sub-agent.
pub struct DelegateTool;

/// The role and task a `delegate` call asks for, validated as Python's `Args`.
pub fn request(args: &Map<String, Value>) -> Result<(String, String), ToolError> {
    let mut arguments = Arguments::new(args);
    let role = arguments.string("role", Some("assistant"));
    let task = arguments.string("task", None);
    arguments.finish()?;
    Ok((role, task))
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Delegate a focused subtask to a fresh sub-agent with a given role (e.g. 'researcher', \
         'coder', 'reviewer'). The sub-agent runs to completion and returns its result. Use for \
         well-scoped subtasks you want handled independently."
    }

    /// Read-only at this level: the sub-agent's own tools are gated one by one.
    fn side_effecting(&self) -> bool {
        false
    }

    /// A whole nested agent loop, with approval prompts of its own: two of those
    /// racing for one terminal is not something the gate can untangle.
    fn parallel_safe(&self) -> bool {
        false
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "role": {
                    "default": "assistant",
                    "description": "The sub-agent's role/persona.",
                    "title": "Role",
                    "type": "string",
                },
                "task": {
                    "description": "The self-contained task for the sub-agent.",
                    "title": "Task",
                    "type": "string",
                },
            },
            "required": ["task"],
            "type": "object",
        })
    }

    /// Reached only when no harness runs the delegation, as Python's tool with no
    /// `spawn` callable.
    fn execute(
        &self,
        args: &Map<String, Value>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        request(args)?;
        Ok(failure(self.name(), "Delegation is disabled.".into()))
    }
}
