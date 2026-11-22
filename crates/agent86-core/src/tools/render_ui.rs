//! `render_ui`, beyond Python (ADR 0005): the model draws A2UI 1.0 surfaces on
//! the renderer the embedder declared. Its result is the validator's verdict,
//! so a model that gets a message wrong reads what to fix and tries again.
//!
//! Registered only where a renderer is (`sdk::AgentBuilder::ui`), so `run`, the
//! plain loop, and CI never see it. The schema stays shallow (an array of
//! objects): the catalogs' detail is in the prompt section, and correctness is
//! the validator's job, since providers' strict modes cap a schema's size.

use super::{failure, success, Tool, ToolContext, ToolError};
use crate::agent86ui::{MESSAGES_KEY, RENDER_UI};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

pub struct RenderUiTool;

impl Tool for RenderUiTool {
    fn name(&self) -> &str {
        RENDER_UI
    }

    fn description(&self) -> &str {
        "Show the user interactive UI (a form, a choice, a confirmation, a small \
         dashboard) by sending A2UI 1.0 messages, which their screen draws natively. \
         The result says whether the messages were valid; if not, nothing was drawn, \
         and it lists what to fix. A button the user presses comes back later as a \
         user message starting `[ui action]`. See the agent86UI section of the system \
         prompt for the messages and the catalogs' components."
    }

    /// What it draws reaches the user, and surfaces are built up in call
    /// order, so it never shares a parallel batch.
    fn parallel_safe(&self) -> bool {
        false
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "messages": {
                    "description": "A2UI 1.0 agent-to-renderer messages, applied in order, all or nothing: each {\"version\": \"1.0\"} plus one of createSurface, updateComponents, updateDataModel, deleteSurface.",
                    "items": {"type": "object"},
                    "title": "Messages",
                    "type": "array",
                },
            },
            "required": ["messages"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let messages = match args.get("messages") {
            Some(Value::Array(items)) if items.iter().all(Value::is_object) => items,
            Some(Value::Array(_)) => {
                return Err(ToolError::InvalidArguments(vec![
                    "messages: each item should be a valid dictionary".into(),
                ]))
            }
            Some(_) => {
                return Err(ToolError::InvalidArguments(vec![
                    "messages: Input should be a valid list".into(),
                ]))
            }
            None => {
                return Err(ToolError::InvalidArguments(vec![
                    "messages: Field required".into(),
                ]))
            }
        };
        if let Some(extra) = args.keys().find(|key| *key != "messages") {
            return Err(ToolError::InvalidArguments(vec![format!(
                "{extra}: Extra inputs are not permitted"
            )]));
        }
        let Some(ui) = ctx.ui.as_deref() else {
            return Ok(failure(
                self.name(),
                "No screen that can draw UI is attached here, so nothing was drawn. \
                 Ask the user in plain text instead."
                    .into(),
            ));
        };
        if messages.is_empty() {
            return Ok(failure(
                self.name(),
                "messages is empty, so there was nothing to draw.".into(),
            ));
        }

        let report = ui.apply(messages);
        if !report.is_ok() {
            let problems: Vec<String> = report
                .errors
                .iter()
                .map(|problem| format!("- {problem}"))
                .collect();
            return Ok(failure(
                self.name(),
                format!(
                    "The messages are not valid A2UI, so nothing was drawn. Fix these and \
                     send the corrected messages again (paths point into `messages`):\n{}",
                    problems.join("\n")
                ),
            ));
        }

        let mut lines = Vec::new();
        let mut surfaces: Vec<&str> = Vec::new();
        for message in &report.messages {
            if let Some(id) = message.surface_id() {
                if !surfaces.contains(&id) {
                    surfaces.push(id);
                }
            }
        }
        let live = ui.surface_ids();
        for id in &surfaces {
            if live.iter().any(|s| s == id) {
                lines.push(format!("Rendered surface {id}."));
            } else {
                lines.push(format!("Deleted surface {id}."));
            }
        }
        if lines.is_empty() {
            lines.push("Applied.".into());
        }
        for warning in &report.warnings {
            lines.push(format!("Warning: {warning}"));
        }
        let mut result = success(self.name(), lines.join("\n"));
        let messages: Vec<Value> = report
            .messages
            .iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .collect();
        result
            .metadata
            .insert(MESSAGES_KEY.into(), Value::Array(messages));
        Ok(result)
    }
}
