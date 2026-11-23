//! `render_ui`, beyond Python (ADR 0005): the model draws A2UI 1.0 surfaces on
//! the renderer the embedder declared. Its result is the validator's verdict,
//! so a model that gets a message wrong reads what to fix and tries again.
//!
//! Registered only where a renderer is (`sdk::AgentBuilder::ui`), so `run`, the
//! plain loop, and CI never see it. The schema stays shallow (an array of
//! objects): the catalogs' detail is in the prompt section, and correctness is
//! the validator's job, since providers' strict modes cap a schema's size.

use super::{failure, success, Tool, ToolContext, ToolError};
use crate::agent86ui::{
    complete, policy, sanitize, tidy, MESSAGES_KEY, RENDER_UI, TRACE_KEY, WAIT_KEY,
};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

pub struct RenderUiTool;

impl Tool for RenderUiTool {
    fn name(&self) -> &str {
        RENDER_UI
    }

    fn description(&self) -> &str {
        "Show the user interactive UI (a layout, a confirmation, a small dashboard, a \
         choice with a presentation of its own) by sending A2UI 1.0 messages, which their \
         screen draws natively. To ask the user questions, even when they ask for a form, \
         call ask_user instead wherever it is offered: it builds the form for you. The \
         result says whether the messages were valid; if not, nothing was drawn, and it \
         lists what to fix. A button the user presses comes back later as a user message \
         starting `[ui action]`. See the agent86UI section of the system prompt for the \
         messages and the catalogs' components."
    }

    /// What it draws reaches the user, and surfaces are built up in call
    /// order, so it never shares a parallel batch.
    fn parallel_safe(&self) -> bool {
        false
    }

    /// A surface's JSON breaks in its deepest nesting, most often in a large
    /// one (ADR 0006), so a surface built in parts breaks less, and loses less.
    fn retry_hint(&self) -> Option<&str> {
        Some(
            "Build the surface in parts: one call with createSurface holding the root and \
             the first few components, then further calls with updateComponents adding the \
             rest and sending the root again with its longer `children`, with `wait` only on \
             the last call.",
        )
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
                "wait": {
                    "default": false,
                    "description": "Wait for the user's first action on the surface and get it as this call's result, for a form in the middle of a task. Without it the turn goes on, and an action comes later as a user message.",
                    "title": "Wait",
                    "type": "boolean",
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
        let wait = match args.get("wait") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(wait)) => *wait,
            Some(_) => {
                return Err(ToolError::InvalidArguments(vec![
                    "wait: Input should be a valid boolean".into(),
                ]))
            }
        };
        if let Some(extra) = args.keys().find(|key| *key != "messages" && *key != "wait") {
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
        if wait && !ui.can_wait() {
            return Ok(failure(
                self.name(),
                "This screen cannot answer while the turn waits, so nothing was drawn. Send \
                 the messages again without wait; an action on the surface comes as a later \
                 user message."
                    .into(),
            ));
        }
        if messages.is_empty() {
            return Ok(failure(
                self.name(),
                "messages is empty, so there was nothing to draw.".into(),
            ));
        }

        // What a surface shows reaches the user as the model's text does: through
        // the vault's redaction and the egress guardrail first. Then agent86's
        // policy, then the protocol's rules.
        // A field beside the message's kind is moved inside it (`agent86ui::tidy`).
        let mut moved = Vec::new();
        let messages: Vec<Value> = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let (message, fields) = tidy(&sanitize(message, &ctx.secrets, ui.egress()));
                moved.extend(
                    fields
                        .into_iter()
                        .map(|field| format!("Note: moved {field} in messages/{index}; it belongs inside, not beside it.")),
                );
                message
            })
            .collect();
        // A binding written `{"path": ...}` becomes `@path`, and a Button event
        // with no context gets one carrying the surface's values (ADR 0006).
        let (messages, rebound) = complete(&messages, |id| ui.surface_keys(id));
        moved.extend(rebound);
        let components = component_count(&messages);
        let mut refusals = policy(&messages, &ctx.policy, ui.remote_media());
        refusals.extend(reserved_surfaces(&messages));
        if !refusals.is_empty() {
            let codes = vec!["REFUSED"; refusals.len()];
            let mut result = failure(
                self.name(),
                format!(
                    "agent86 does not allow some of these messages, so nothing was drawn. Change \
                     these and send the messages again (paths point into `messages`):\n{}",
                    bullets(&refusals)
                ),
            );
            result.metadata.insert(
                TRACE_KEY.into(),
                trace(&messages, components, "refused", &codes),
            );
            return Ok(result);
        }
        // A waiting surface the user cannot answer would hold the turn until they
        // dismissed it: only a Button with an event action sends an answer.
        if wait {
            let unanswerable = unanswerable(&messages, ui);
            if !unanswerable.is_empty() {
                let mut result = failure(
                    self.name(),
                    format!(
                        "Nothing on {} sends an answer, so nothing was drawn: choosing an \
                         option or typing only changes the data model. Add a Button whose \
                         action is an event (for example {{\"event\": {{\"name\": \
                         \"submit\"}}}}) and send the messages again.",
                        unanswerable
                            .iter()
                            .map(|id| format!("surface {id}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                result.metadata.insert(
                    TRACE_KEY.into(),
                    trace(&messages, components, "refused", &["REFUSED"]),
                );
                return Ok(result);
            }
        }
        let report = ui.apply(&messages);
        if !report.is_ok() {
            let codes: Vec<&str> = report.errors.iter().map(|p| p.code.as_str()).collect();
            let mut result = failure(
                self.name(),
                format!(
                    "The messages are not valid A2UI, so nothing was drawn. Fix these and \
                     send the corrected messages again (paths point into `messages`):\n{}",
                    bullets(&report.errors)
                ),
            );
            result.metadata.insert(
                TRACE_KEY.into(),
                trace(&messages, components, "invalid", &codes),
            );
            return Ok(result);
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
        lines.extend(moved);
        let mut result = success(self.name(), lines.join("\n"));
        let typed: Vec<Value> = report
            .messages
            .iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .collect();
        result
            .metadata
            .insert(MESSAGES_KEY.into(), Value::Array(typed));
        result.metadata.insert(
            TRACE_KEY.into(),
            trace(&messages, components, "rendered", &[]),
        );
        if wait {
            let waited: Vec<Value> = surfaces
                .iter()
                .filter(|id| live.iter().any(|s| s == *id))
                .map(|id| Value::from(*id))
                .collect();
            result
                .metadata
                .insert(WAIT_KEY.into(), Value::Array(waited));
        }
        Ok(result)
    }
}

/// Messages about a surface named like the forms `ask_user` builds
/// (`ask::FORM_SURFACE_PREFIX`, ADR 0006): refused, so the model cannot replace
/// a form while the user answers it.
fn reserved_surfaces(messages: &[Value]) -> Vec<crate::agent86ui::Refusal> {
    let mut refusals = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some((kind, body)) = message
            .as_object()
            .and_then(|envelope| envelope.iter().find(|(key, _)| *key != "version"))
        else {
            continue;
        };
        let reserved = body
            .get("surfaceId")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with(crate::ask::FORM_SURFACE_PREFIX));
        if reserved {
            refusals.push(crate::agent86ui::Refusal {
                path: format!("/{index}/{kind}/surfaceId"),
                message: format!(
                    "surface ids starting with {} are agent86's own forms; choose another id \
                     (and to ask the user questions, call ask_user)",
                    crate::ask::FORM_SURFACE_PREFIX
                ),
            });
        }
    }
    refusals
}

fn bullets<T: std::fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many components a batch sends.
fn component_count(messages: &[Value]) -> usize {
    messages
        .iter()
        .flat_map(|message| {
            ["createSurface", "updateComponents"]
                .into_iter()
                .filter_map(move |kind| message.get(kind)?.get("components")?.as_array())
        })
        .map(Vec::len)
        .sum()
}

/// The `ui_render` trace fields: the surfaces a batch names, its component
/// count, the verdict, and the codes of what was wrong. Never the components.
fn trace(messages: &[Value], components: usize, verdict: &str, codes: &[&str]) -> Value {
    let mut surfaces: Vec<&str> = Vec::new();
    for message in messages {
        let id = message
            .as_object()
            .into_iter()
            .flat_map(|object| object.values())
            .find_map(|body| body.get("surfaceId").and_then(Value::as_str));
        if let Some(id) = id {
            if !surfaces.contains(&id) {
                surfaces.push(id);
            }
        }
    }
    json!({
        "surfaces": surfaces,
        "components": components,
        "verdict": verdict,
        "problems": codes,
    })
}

/// The surfaces a waiting batch creates or changes that would have no Button
/// with an event action, counting the batch's components and those already
/// drawn. Deleted surfaces wait on nothing and are left out.
fn unanswerable(messages: &[Value], ui: &crate::agent86ui::UiSession) -> Vec<String> {
    let mut named: Vec<String> = Vec::new();
    let mut answerable: Vec<String> = Vec::new();
    for message in messages {
        for key in ["createSurface", "updateComponents"] {
            let Some(body) = message.get(key) else {
                continue;
            };
            let Some(id) = body.get("surfaceId").and_then(Value::as_str) else {
                continue;
            };
            if !named.iter().any(|known| known == id) {
                named.push(id.to_owned());
            }
            let has = body
                .get("components")
                .and_then(Value::as_array)
                .is_some_and(|components| components.iter().any(crate::agent86ui::is_event_button));
            if has {
                answerable.push(id.to_owned());
            }
        }
    }
    named
        .into_iter()
        .filter(|id| !answerable.contains(id) && !ui.has_event_button(id))
        .collect()
}
