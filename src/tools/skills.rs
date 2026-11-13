//! `use_skill`, ported from Python's `tools/builtin/skills_tool.py`: progressive
//! disclosure of skill instructions.
//!
//! Skill names and descriptions are advertised in the system prompt. When one
//! applies, the model calls `use_skill` to load its full instructions into the
//! conversation, keeping the base context lean.

use super::{failure, success, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

/// Load a skill's full instructions.
pub struct UseSkillTool;

impl Tool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for a named skill. Call this when a skill from the \
         'Available skills' list applies to the task, then follow the returned instructions."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "name": {
                    "description": "The exact name of the skill to load.",
                    "title": "Name",
                    "type": "string",
                },
            },
            "required": ["name"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let name = arguments.string("name", None);
        arguments.finish()?;
        let Some(skill) = ctx.skills.get(&name) else {
            let mut names: Vec<&str> = ctx.skills.keys().map(String::as_str).collect();
            names.sort_unstable();
            let available = if names.is_empty() {
                "(none)".to_owned()
            } else {
                names.join(", ")
            };
            return Ok(failure(
                self.name(),
                format!("No skill named '{name}'. Available: {available}."),
            ));
        };
        let body = skill.load_text();
        // Activating a skill replaces any previously active one, so its list is the
        // restriction in force from here to the end of the turn.
        ctx.activate_skill(skill);
        let mut result = success(self.name(), body);
        result.metadata.insert("skill".into(), json!(skill.name));
        result
            .metadata
            .insert("allowed_tools".into(), json!(skill.allowed_tools));
        Ok(result)
    }
}
