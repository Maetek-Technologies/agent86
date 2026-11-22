//! The A2UI 1.0 messages, as typed Rust values.
//!
//! Every message on the wire is one JSON object carrying `"version": "1.0"` and
//! exactly one key naming its kind. [`AgentMessage`] and [`RendererMessage`]
//! keep that shape: a `version` and a flattened kind. Component properties stay
//! JSON ([`Component::properties`]), since what they may hold is the catalog's
//! business, not the protocol's.
//!
//! Deserializing is lenient about what the schema is strict about (unknown keys
//! are kept or ignored); [`crate::Validator`] runs the schema first, so a value
//! that reached these types through it is well-formed.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// An explicit `null` stays `Some(Value::Null)`; only an absent field is `None`.
/// (`value: null` in `updateDataModel` deletes, and a function may return
/// `null`, so the two must not collapse.)
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// A message from the agent to the renderer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentMessage {
    /// The protocol version, `"1.0"`.
    pub version: String,
    /// What the message does.
    #[serde(flatten)]
    pub kind: AgentMessageKind,
}

impl AgentMessage {
    /// A 1.0 message of this kind.
    pub fn new(kind: AgentMessageKind) -> Self {
        Self {
            version: crate::PROTOCOL_VERSION.to_string(),
            kind,
        }
    }

    /// The surface the message is about, when it names one.
    pub fn surface_id(&self) -> Option<&str> {
        match &self.kind {
            AgentMessageKind::CreateSurface(m) => Some(&m.surface_id),
            AgentMessageKind::UpdateComponents(m) => Some(&m.surface_id),
            AgentMessageKind::UpdateDataModel(m) => Some(&m.surface_id),
            AgentMessageKind::DeleteSurface(m) => Some(&m.surface_id),
            AgentMessageKind::CallRendererFunction(m) => m.surface_id.as_deref(),
            AgentMessageKind::AgentFunctionResponse(_) => None,
        }
    }
}

/// The kinds of agent-to-renderer message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum AgentMessageKind {
    /// Create a surface, optionally with its components and data model.
    CreateSurface(CreateSurface),
    /// Add or replace components on a surface.
    UpdateComponents(UpdateComponents),
    /// Set or delete a value in a surface's data model.
    UpdateDataModel(UpdateDataModel),
    /// Remove a surface.
    DeleteSurface(DeleteSurface),
    /// Ask the renderer to run one of its functions.
    CallRendererFunction(CallRendererFunction),
    /// Answer a `callAgentFunction` from the renderer.
    AgentFunctionResponse(FunctionResponse),
}

/// `createSurface`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CreateSurface {
    /// Unique for the renderer's lifetime.
    pub surface_id: String,
    /// The surface's default catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    /// Whether the renderer sends the data model back with each message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_data_model: Option<bool>,
    /// Components to start with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<Component>>,
    /// The initial data model, applied before the components.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_model: Option<Map<String, Value>>,
    /// Surface metadata (`extensions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// `updateComponents`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct UpdateComponents {
    /// The surface to update.
    pub surface_id: String,
    /// Components to add, or to replace by id.
    pub components: Vec<Component>,
}

/// `updateDataModel`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct UpdateDataModel {
    /// The surface whose data model changes.
    pub surface_id: String,
    /// A JSON Pointer; absent or `/` means the whole model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The new value; `null` deletes the key at `path`.
    pub value: Value,
}

/// `deleteSurface`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeleteSurface {
    /// The surface to remove.
    pub surface_id: String,
}

/// `callRendererFunction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CallRendererFunction {
    /// The surface whose data model the call reads, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_id: Option<String>,
    /// Echoed in the renderer's response.
    pub function_call_id: String,
    /// The call (its `catalogId` is required here).
    pub call_function: FunctionCall,
}

/// A call to a catalog function, as it appears in bindings, checks, actions,
/// and the function-call messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FunctionCall {
    /// The function's name.
    #[serde(rename = "@call")]
    pub call: String,
    /// The catalog to resolve it in, overriding the surface's default.
    #[serde(rename = "catalogId", default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    /// The arguments, as the function's schema defines them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Map<String, Value>>,
}

/// `agentFunctionResponse` and `rendererFunctionResponse`: a value or an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FunctionResponse {
    /// The id of the call this answers.
    pub function_call_id: String,
    /// The result, when the call succeeded (`null` is a result).
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
    /// The failure, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// A function response's `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ErrorBody {
    /// A machine-readable code.
    pub code: String,
    /// What went wrong.
    pub message: String,
}

/// One component on a surface. `id` and `component` are the protocol's; the
/// `@`-prefixed fields are its envelope mixins; everything else is the
/// catalog's, kept as JSON in [`Component::properties`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Component {
    /// Unique on its surface; a UAX #31 identifier.
    pub id: String,
    /// The component type, from the catalog.
    pub component: String,
    /// The catalog to resolve the type in, overriding the surface's default.
    #[serde(
        rename = "@catalogId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub catalog_id: Option<String>,
    /// `label`, `description`, `live`, and `hidden`, for assistive technologies.
    #[serde(
        rename = "@accessibility",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub accessibility: Option<Value>,
    /// Vendor metadata (`extensions`).
    #[serde(rename = "@metadata", default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Renderer-side checks, on checkable components.
    #[serde(rename = "@checks", default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<CheckRule>>,
    /// The catalog's properties for this type (`text`, `children`, `action`, ...).
    #[serde(flatten)]
    pub properties: Map<String, Value>,
}

/// One `@checks` rule: a condition (a binding or a function call) and the
/// message to show when it fails.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CheckRule {
    /// A `{"@path": ...}` or a function call, evaluating to a boolean or a
    /// validation result.
    pub condition: Value,
    /// The fallback message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A message from the renderer to the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RendererMessage {
    /// The protocol version, `"1.0"`.
    pub version: String,
    /// What the message reports.
    #[serde(flatten)]
    pub kind: RendererMessageKind,
}

impl RendererMessage {
    /// A 1.0 message of this kind.
    pub fn new(kind: RendererMessageKind) -> Self {
        Self {
            version: crate::PROTOCOL_VERSION.to_string(),
            kind,
        }
    }
}

/// The kinds of renderer-to-agent message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum RendererMessageKind {
    /// The user did something a component's action sends to the agent.
    Action(Action),
    /// The renderer asks the agent to run a function.
    CallAgentFunction(CallAgentFunction),
    /// The answer to a `callRendererFunction`.
    RendererFunctionResponse(FunctionResponse),
    /// A validation, integrity, or evaluation error on the renderer.
    Error(RendererError),
}

/// `action`: a user interaction with a component whose action is an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Action {
    /// The event's name, from the component's `action.event.name`.
    pub name: String,
    /// The surface it happened on.
    pub surface_id: String,
    /// The component that triggered it.
    pub source_component_id: String,
    /// When, as an RFC 3339 date-time.
    pub timestamp: String,
    /// The event's `context`, with every binding resolved.
    pub context: Map<String, Value>,
    /// The event's `userMessage`, resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message: Option<String>,
    /// Renderer metadata (`extensions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl Action {
    /// An action as a renderer reports it.
    pub fn new(
        name: impl Into<String>,
        surface_id: impl Into<String>,
        source_component_id: impl Into<String>,
        timestamp: impl Into<String>,
        context: Map<String, Value>,
    ) -> Self {
        Self {
            name: name.into(),
            surface_id: surface_id.into(),
            source_component_id: source_component_id.into(),
            timestamp: timestamp.into(),
            context,
            user_message: None,
            metadata: None,
        }
    }
}

/// `callAgentFunction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CallAgentFunction {
    /// The surface the call came from.
    pub surface_id: String,
    /// To be echoed in the agent's response.
    pub function_call_id: String,
    /// The call.
    pub call_function: FunctionCall,
}

/// A renderer's `error` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RendererError {
    /// `VALIDATION_FAILED`, `UNALLOWED_PARENT`, `INTEGRITY_ERROR`, ...
    pub code: String,
    /// What went wrong.
    pub message: String,
    /// The surface, for surface errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_id: Option<String>,
    /// A JSON Pointer to the offending field, for validation errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The function call, for function invocation errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_call_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_agent_message_round_trips_with_its_version_first() {
        let wire = json!({
            "version": "1.0",
            "createSurface": {
                "surfaceId": "s1",
                "catalogId": "c",
                "components": [
                    {"id": "root", "component": "Text", "text": {"@path": "/name"},
                     "@accessibility": {"label": "Name"}}
                ],
                "dataModel": {"name": "Ada"}
            }
        });
        let message: AgentMessage = serde_json::from_value(wire.clone()).unwrap();
        let AgentMessageKind::CreateSurface(create) = &message.kind else {
            panic!("not createSurface")
        };
        let root = &create.components.as_ref().unwrap()[0];
        assert_eq!(root.component, "Text");
        assert_eq!(root.properties["text"], json!({"@path": "/name"}));
        assert_eq!(message.surface_id(), Some("s1"));
        let back = serde_json::to_value(&message).unwrap();
        assert_eq!(back, wire);
        assert_eq!(
            serde_json::to_string(&message).unwrap().find("\"version\""),
            Some(1)
        );
    }

    #[test]
    fn a_null_value_is_kept_apart_from_a_missing_one() {
        let delete: AgentMessage = serde_json::from_value(json!({
            "version": "1.0",
            "updateDataModel": {"surfaceId": "s", "path": "/a", "value": null}
        }))
        .unwrap();
        let AgentMessageKind::UpdateDataModel(update) = delete.kind else {
            panic!()
        };
        assert_eq!(update.value, Value::Null);

        let response: FunctionResponse =
            serde_json::from_value(json!({"functionCallId": "c", "value": null})).unwrap();
        assert_eq!(response.value, Some(Value::Null));
        let error: FunctionResponse = serde_json::from_value(
            json!({"functionCallId": "c", "error": {"code": "X", "message": "m"}}),
        )
        .unwrap();
        assert_eq!(error.value, None);
    }

    #[test]
    fn an_action_serializes_as_the_specification_shows() {
        let mut context = Map::new();
        context.insert("isSubscribed".into(), json!(true));
        let mut action = Action::new(
            "submitForm",
            "contact_form_1",
            "submit_button",
            "2026-06-02T08:57:23Z",
            context,
        );
        action.user_message = Some("Submitted contact form".into());
        let message = RendererMessage::new(RendererMessageKind::Action(action));
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({
                "version": "1.0",
                "action": {
                    "name": "submitForm",
                    "surfaceId": "contact_form_1",
                    "sourceComponentId": "submit_button",
                    "timestamp": "2026-06-02T08:57:23Z",
                    "context": {"isSubscribed": true},
                    "userMessage": "Submitted contact form"
                }
            })
        );
    }
}
