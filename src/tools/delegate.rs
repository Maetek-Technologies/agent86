//! `delegate`, ported from Python's `tools/builtin/delegate.py`: model-driven
//! sub-agent spawning, with the main agent as supervisor.
//!
//! The model names a role and a self-contained task; a fresh sub-agent with the
//! same tools, sandbox, and guardrails runs it to completion, and its answer is
//! the tool's output. The run itself belongs to the harness, which awaits the
//! sub-agent's model calls (`Harness::delegate`); this tool carries the schema
//! and the argument rules.
//!
//! Beyond Python, the schema has optional arguments after Python's `role` and
//! `task` (`docs/subagents.md`): `agent` names a specialist sub-agent, `skill`
//! preloads a skill into it, and `files` and `summary` attach context to the task.
//! With `role` and `task` alone it is Python's tool, and old transcripts stay valid.

use super::{failure, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

pub const NAME: &str = "delegate";

/// Hand a focused subtask to a sub-agent.
pub struct DelegateTool;

/// What a `delegate` call asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegateRequest {
    /// Python's generic persona; ignored when `agent` is given.
    pub role: String,
    pub task: String,
    /// The specialist sub-agent to run.
    pub agent: Option<String>,
    /// A skill to preload for this run (needs `agent`).
    pub skill: Option<String>,
    /// Run in a git worktree of `HEAD` (needs `agent`).
    pub isolate: bool,
    /// Workspace paths whose contents go into the sub-agent's first message.
    pub files: Vec<String>,
    /// Context the parent wrote, for the first message.
    pub summary: Option<String>,
}

/// The arguments of a `delegate` call, validated with pydantic's messages.
pub fn request(args: &Map<String, Value>) -> Result<DelegateRequest, ToolError> {
    let mut arguments = Arguments::new(args);
    let role = arguments.string("role", Some("assistant"));
    let task = arguments.string("task", None);
    let agent = arguments.optional_string("agent");
    let skill = arguments.optional_string("skill");
    let isolate = arguments.boolean("isolate", false);
    let files = arguments.string_list("files");
    let summary = arguments.optional_string("summary");
    arguments.finish()?;
    let filled = |value: Option<String>| value.filter(|text| !text.trim().is_empty());
    Ok(DelegateRequest {
        role,
        task,
        agent: filled(agent).map(|name| name.trim().to_owned()),
        skill: filled(skill).map(|name| name.trim().to_owned()),
        isolate,
        files,
        summary: filled(summary),
    })
}

/// Python's `Args` schema: `role` and `task`, as pydantic generates it.
pub fn python_parameters() -> Value {
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
        let mut schema = python_parameters();
        let nullable_string = |title: &str, description: &str| {
            json!({
                "anyOf": [{"type": "string"}, {"type": "null"}],
                "default": null,
                "description": description,
                "title": title,
            })
        };
        let properties = schema["properties"]
            .as_object_mut()
            .expect("the schema has properties");
        properties.insert(
            "agent".into(),
            nullable_string(
                "Agent",
                "The name of a specialist sub-agent to run, from the list in the system \
                 prompt. It brings its own instructions and tools, and `role` is ignored.",
            ),
        );
        properties.insert(
            "skill".into(),
            nullable_string(
                "Skill",
                "A skill to preload into the specialist sub-agent for this run (needs `agent`).",
            ),
        );
        properties.insert(
            "isolate".into(),
            json!({
                "default": false,
                "description": "Run the specialist sub-agent (needs `agent`) in a git worktree of \
                    HEAD, and get its diff back; nothing is applied. It does not see uncommitted \
                    changes, MCP tools are left out unless the specialty names them, and under the \
                    Docker sandbox git does not work inside it.",
                "title": "Isolate",
                "type": "boolean",
            }),
        );
        properties.insert(
            "files".into(),
            json!({
                "default": [],
                "description": "Workspace paths whose contents are attached to the task.",
                "items": {"type": "string"},
                "title": "Files",
                "type": "array",
            }),
        );
        properties.insert(
            "summary".into(),
            nullable_string(
                "Summary",
                "Context for the sub-agent that the task does not say: what was decided, \
                 what was ruled out.",
            ),
        );
        schema
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
