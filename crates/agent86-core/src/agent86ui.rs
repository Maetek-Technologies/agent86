//! agent86UI in the harness (beyond Python; ADR 0005): the model draws A2UI 1.0
//! surfaces through `render_ui` (`crate::tools::render_ui`), checked against
//! the renderer's catalogs by the `agent86-ui` crate, and the loop hands each
//! validated message to the surface as a [`crate::agent::TurnEvent::Ui`].
//!
//! A [`UiSession`] is a surface's renderer as the agent sees it: its catalogs
//! and the surfaces drawn so far (a headless [`Runtime`]), shared by clones of
//! the tool context like the secret vault. It exists only when the embedder
//! declared a renderer; sub-agents never get it.

use agent86_ui::runtime::Runtime;
use agent86_ui::{AgentMessage, Catalog, CompileError, Report, Rules};
use serde_json::{Map, Value};
use std::sync::{Mutex, PoisonError};

/// The tool's name.
pub const RENDER_UI: &str = "render_ui";

/// Where `render_ui` leaves the validated messages in its result's metadata,
/// for the loop to take out and emit as `TurnEvent::Ui` before the result is
/// recorded (so nothing new reaches the session or the trace).
pub const MESSAGES_KEY: &str = "agent86ui_messages";

/// The completeness rules a `render_ui` batch is checked under: strict, but an
/// orphan is a warning, because A2UI 1.0 has no message that removes a
/// component, so one replaced as a child stays behind and the model could not
/// fix the error (ADR 0005, third amendment).
pub const RULES: Rules = Rules::STRICT.allowing_orphans(true);

/// A renderer's catalogs and the surfaces drawn on it so far.
pub struct UiSession {
    catalogs: Vec<Catalog>,
    runtime: Mutex<Runtime>,
}

impl std::fmt::Debug for UiSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiSession")
            .field(
                "catalogs",
                &self.catalogs.iter().map(Catalog::id).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl UiSession {
    /// A renderer that supports `catalogs`, in order of preference.
    pub fn new(catalogs: Vec<Catalog>) -> Result<Self, CompileError> {
        let mut runtime = Runtime::new(catalogs.clone())?;
        runtime.set_rules(RULES);
        Ok(Self {
            catalogs,
            runtime: Mutex::new(runtime),
        })
    }

    pub fn catalogs(&self) -> &[Catalog] {
        &self.catalogs
    }

    fn runtime(&self) -> std::sync::MutexGuard<'_, Runtime> {
        self.runtime.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Validate a batch and, when it has no errors, apply it to the surfaces.
    pub fn apply(&self, messages: &[Value]) -> Report {
        self.runtime().apply(messages)
    }

    /// The ids of the surfaces up now.
    pub fn surface_ids(&self) -> Vec<String> {
        self.runtime().surface_ids().map(String::from).collect()
    }

    /// The system-prompt section: how to use `render_ui`, then each catalog's
    /// own instructions and a compact reference to its components and
    /// functions. Rebuilt per request, like the MCP section.
    pub fn prompt_section(&self) -> String {
        let mut section = String::from(PROMPT);
        for catalog in &self.catalogs {
            section.push_str(&format!("\n\n### Catalog `{}`\n", catalog.id()));
            if let Some(instructions) = catalog.instructions() {
                section.push('\n');
                section.push_str(instructions.trim());
                section.push('\n');
            }
            section.push_str(&reference(catalog));
        }
        section
    }
}

/// What every `render_ui` prompt says, whatever the catalogs.
const PROMPT: &str = "## agent86UI: drawing UI with render_ui

You can show the user interactive UI (forms, choices, confirmations, small \
dashboards) by calling render_ui with A2UI 1.0 messages; the user's screen draws \
them with its own widgets. Use it when you need a structured answer (pick one of \
several options, fill in a few fields, confirm a plan) or when a layout says more \
than prose. Otherwise answer in plain text.

- `messages` is a list of A2UI messages, each `{\"version\": \"1.0\", ...}` with \
exactly one of `createSurface`, `updateComponents`, `updateDataModel`, or \
`deleteSurface`. Create a surface once with createSurface (a new `surfaceId`, a \
`catalogId` from below, and usually its `components` and `dataModel` inline), \
change it later with updateComponents (a component replaces the one with the same \
id) and updateDataModel (`path` and `value`), and remove it with deleteSurface.
- Components are a flat list linked by id, and exactly one has the id \"root\". \
Every child id must be defined in the same call or an earlier one.
- A value is a literal, a binding to the surface's data model (`{\"@path\": \
\"/form/name\"}`), or a function call (`{\"@call\": \"formatString\", \"args\": \
{...}}`). Input components write what the user enters back to the path they are \
bound to. A `ChildList` is an array of ids, or a template over a list in the data \
model (`{\"path\": \"/items\", \"componentId\": \"row\"}`) whose bindings are \
relative to each item.
- A button's action is `{\"event\": {\"name\": \"submit\", \"context\": {\"name\": \
{\"@path\": \"/form/name\"}}}}`. When the user presses it, a later user message \
reads `[ui action] submit on <surfaceId>: <context as JSON>`; the surface stays \
up meanwhile.
- render_ui's result is the validator's verdict. If it lists problems, nothing \
was drawn: fix them and send the corrected messages again.
- Renderer functions (`callRendererFunction`) and agent functions are not \
available.";

/// A compact reference to a catalog's components and functions, from their
/// schemas: each component's properties (`?` marks an optional one) with their
/// types, and each function's arguments and return type.
pub fn reference(catalog: &Catalog) -> String {
    let document = catalog.document();
    let mut out = String::from("\nComponents:\n");
    for (name, schema) in document
        .get("components")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let (properties, required, checkable) = gather(schema);
        let fields: Vec<String> = properties
            .iter()
            .filter(|(key, _)| *key != "component" && *key != "id")
            .map(|(key, property)| {
                let optional = if required.iter().any(|r| r == key) {
                    ""
                } else {
                    "?"
                };
                format!("{key}{optional}: {}", type_of(property))
            })
            .collect();
        let checks = if checkable { " (accepts @checks)" } else { "" };
        out.push_str(&format!("- {name} {{ {} }}{checks}\n", fields.join(", ")));
    }
    let functions: Vec<String> = document
        .get("functions")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(name, _)| {
            catalog
                .function(name)
                .is_none_or(|def| def.callers != agent86_ui::Callers::AgentOnly)
        })
        .map(|(name, schema)| {
            let args = schema.pointer("/properties/args");
            let required: Vec<&str> = args
                .and_then(|a| a.get("required"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let params: Vec<String> = args
                .and_then(|a| a.get("properties"))
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(key, property)| {
                    let optional = if required.contains(&key.as_str()) {
                        ""
                    } else {
                        "?"
                    };
                    format!("{key}{optional}: {}", type_of(property))
                })
                .collect();
            let returns = schema
                .get("returnType")
                .and_then(Value::as_str)
                .unwrap_or("any");
            format!("- {name}({}) -> {returns}\n", params.join(", "))
        })
        .collect();
    if !functions.is_empty() {
        out.push_str("\nFunctions:\n");
        out.extend(functions);
    }
    out
}

/// A component schema's properties and required names, from its own
/// `properties` and its `allOf` parts, and whether it takes `@checks`.
fn gather(schema: &Value) -> (Map<String, Value>, Vec<String>, bool) {
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut checkable = false;
    let mut parts = vec![schema];
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        parts.extend(all);
    }
    for part in parts {
        if part
            .get("$ref")
            .and_then(Value::as_str)
            .is_some_and(|r| r.ends_with("/Checkable"))
        {
            checkable = true;
        }
        if let Some(own) = part.get("properties").and_then(Value::as_object) {
            for (key, value) in own {
                properties.insert(key.clone(), value.clone());
            }
        }
        required.extend(
            part.get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(String::from),
        );
    }
    (properties, required, checkable)
}

/// A property's type, briefly: a common type by name, an enum's values, or the
/// JSON type.
fn type_of(property: &Value) -> String {
    if let Some(target) = property.get("$ref").and_then(Value::as_str) {
        return target.rsplit('/').next().unwrap_or(target).to_string();
    }
    if let Some(values) = property.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("|");
    }
    match property.get("type").and_then(Value::as_str) {
        Some("array") => match property.get("items") {
            Some(items) => format!("array of {}", type_of(items)),
            None => "array".into(),
        },
        Some(kind) => kind.to_string(),
        None => "any".into(),
    }
}

/// The messages a `render_ui` result carries for the loop, taken out of its
/// metadata, so they are emitted and never recorded.
pub fn take_messages(metadata: &mut Map<String, Value>) -> Vec<AgentMessage> {
    match metadata.remove(MESSAGES_KEY) {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| serde_json::from_value(item).ok())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_basic_catalog_reference_names_components_and_functions() {
        let reference = reference(Catalog::basic());
        assert!(
            reference.contains(
                "- Button { child: Child, variant?: \"default\"|\"primary\"|\"borderless\", action: Action, weight?: Weight } (accepts @checks)"
            ),
            "{reference}"
        );
        assert!(reference.contains("- TextField {"), "{reference}");
        assert!(
            reference.contains("- formatString(value: DynamicString) -> string"),
            "{reference}"
        );
        assert!(!reference.contains("component:"), "{reference}");
    }

    #[test]
    fn the_prompt_section_carries_the_catalog_instructions() {
        let session = UiSession::new(vec![Catalog::basic().clone()]).unwrap();
        let section = session.prompt_section();
        assert!(section.starts_with("## agent86UI: drawing UI with render_ui"));
        assert!(section.contains(&format!("### Catalog `{}`", agent86_ui::BASIC_CATALOG_ID)));
        assert!(section.contains("## Catalog Guidelines"));
        assert!(section.contains("[ui action] submit on <surfaceId>"));
    }

    #[test]
    fn orphans_are_warnings_and_dangling_references_errors() {
        let session = UiSession::new(vec![Catalog::basic().clone()]).unwrap();
        let create = |components: Value| {
            serde_json::json!({"version": "1.0", "createSurface": {
                "surfaceId": "s",
                "catalogId": agent86_ui::BASIC_CATALOG_ID,
                "components": components,
            }})
        };
        let report = session.apply(&[create(serde_json::json!([
            {"id": "root", "component": "Column", "children": ["missing"]}
        ]))]);
        assert!(report.errors[0].message.starts_with("Dangling reference"));
        let report = session.apply(&[create(serde_json::json!([
            {"id": "root", "component": "Text", "text": "hi"},
            {"id": "spare", "component": "Text", "text": "nobody shows me"}
        ]))]);
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(report.warnings[0].message.contains("not reachable"));
        assert_eq!(session.surface_ids(), ["s"]);
    }
}
