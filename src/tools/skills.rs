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
        let mut body = skill.instructions().to_owned();
        let resources = skill.resources();
        if !resources.is_empty() {
            body.push_str(&format!(
                "\n\nBundled resources in the skill directory ({}), readable with read_file: {}",
                skill.directory().display(),
                resources.join(", ")
            ));
        }
        // Activating a skill replaces any previously active one, so its list is the
        // restriction in force from here to the end of the turn.
        ctx.activate_skill(skill);
        if !skill.allowed_tools.is_empty() {
            body.push_str(&format!(
                "\n\nWhile this skill is active you may only call these tools: {} (plus \
                 use_skill). Other tools will be refused.",
                skill.allowed_tools.join(", ")
            ));
        }
        let mut result = success(self.name(), body);
        result.metadata.insert("skill".into(), json!(skill.name));
        result
            .metadata
            .insert("allowed_tools".into(), json!(skill.allowed_tools));
        Ok(result)
    }
}
