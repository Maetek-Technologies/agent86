//! `remember` and `recall`, ported from Python's `tools/builtin/memory.py`.
//!
//! They let the agent persist and look up knowledge across sessions in semantic
//! memory. They are registered only when memory is available, and they are
//! internal (not external-world) side effects, so they need no approval.

use super::{failure, success, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

const DISABLED: &str = "Memory is disabled.";

/// Store a durable fact.
pub struct RememberTool;

impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Persist a durable, user-specific fact to long-term memory for future sessions — a \
         stable preference, identity detail, or project constraint the user would expect you to \
         recall later. Do NOT use it for answers you just computed, transient conversation \
         state, general knowledge, or anything only relevant to this turn. Only call it when the \
         user asks you to remember something, or states a lasting fact about themselves or their \
         work. When in doubt, do not remember."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "text": {
                    "description": "The fact to remember, as a self-contained sentence.",
                    "title": "Text",
                    "type": "string",
                },
            },
            "required": ["text"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let text = arguments.string("text", None);
        arguments.finish()?;
        let Some(memory) = &ctx.memory else {
            return Ok(failure(self.name(), DISABLED.into()));
        };
        memory
            .add_memory(&text, "fact", &Map::new())
            .map_err(|error| ToolError::raised("OperationalError", error.to_string()))?;
        Ok(success(self.name(), "Remembered.".into()))
    }
}

/// Search the stored facts.
pub struct RecallTool;

impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search long-term semantic memory for facts relevant to a query."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "query": {
                    "description": "What to search memory for.",
                    "title": "Query",
                    "type": "string",
                },
                "k": {
                    "default": 5,
                    "description": "Maximum number of results.",
                    "title": "K",
                    "type": "integer",
                },
            },
            "required": ["query"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let query = arguments.string("query", None);
        let k = arguments.integer("k", 5);
        arguments.finish()?;
        let Some(memory) = &ctx.memory else {
            return Ok(failure(self.name(), DISABLED.into()));
        };
        let hits = memory
            .search_facts(&query, k.clamp(1, 20) as usize)
            .map_err(|error| ToolError::raised("OperationalError", error.to_string()))?;
        if hits.is_empty() {
            return Ok(success(self.name(), "(no relevant memories)".into()));
        }
        let body = hits
            .iter()
            .map(|hit| format!("- {}  [score {}]", hit.text, format_2f(hit.score)))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(success(self.name(), body))
    }
}

/// `f"{value:.2f}"`: correctly rounded, exact ties to even, as Python's.
pub fn format_2f(value: f64) -> String {
    format!("{value:.2}")
}
