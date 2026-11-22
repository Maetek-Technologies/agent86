//! agent86UI in the harness (beyond Python; ADR 0005): the model draws A2UI 1.0
//! surfaces through `render_ui` (`crate::tools::render_ui`), checked against
//! the renderer's catalogs by the `agent86-ui` crate, and the loop hands each
//! validated message to the surface as a [`crate::agent::TurnEvent::Ui`].
//!
//! A [`UiSession`] is a surface's renderer as the agent sees it: its catalogs
//! and the surfaces drawn so far (a headless [`Runtime`]), shared by clones of
//! the tool context like the secret vault. It exists only when the embedder
//! declared a renderer; sub-agents never get it.

use crate::config::EgressMode;
use agent86_ui::runtime::Runtime;
use agent86_ui::{Action, AgentMessage, Catalog, CompileError, Report, Rules};
use serde_json::{Map, Value};
use std::sync::{Mutex, PoisonError};

/// The tool's name.
pub const RENDER_UI: &str = "render_ui";

/// Where `render_ui` leaves the validated messages in its result's metadata,
/// for the loop to take out and emit as `TurnEvent::Ui` before the result is
/// recorded (so nothing new reaches the session or the trace).
pub const MESSAGES_KEY: &str = "agent86ui_messages";

/// Where it leaves what the trace's `ui_render` event records (the surfaces,
/// the component count, the verdict), taken out the same way.
pub const TRACE_KEY: &str = "agent86ui_trace";

/// Where it names the surfaces a `wait: true` call waits on, for the loop,
/// which asks once it has emitted the call's messages (so the program has
/// them to draw before it is asked for an answer).
pub const WAIT_KEY: &str = "agent86ui_wait";

/// What a waiting `render_ui` call asks the program (`sdk::Ui::on_wait`): the
/// first action the user takes on one of these surfaces, which the call has
/// just drawn (its `TurnEvent::Ui` messages came first).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UiWait {
    /// The surfaces the call drew that are up, in the order it named them.
    pub surface_ids: Vec<String>,
}

/// Answers a [`UiWait`]: the action, or `None` when the user dismissed it.
pub type UiWaiter = std::sync::Arc<dyn Fn(&UiWait) -> Option<Action> + Send + Sync>;

/// The completeness rules a `render_ui` batch is checked under: strict, but an
/// orphan is a warning, because A2UI 1.0 has no message that removes a
/// component, so one replaced as a child stays behind and the model could not
/// fix the error (ADR 0005, third amendment).
pub const RULES: Rules = Rules::STRICT.allowing_orphans(true);

/// A renderer's catalogs and the surfaces drawn on it so far.
pub struct UiSession {
    catalogs: Vec<Catalog>,
    runtime: Mutex<Runtime>,
    /// The egress guardrail's mode: in `redact`, every string a surface shows
    /// is redacted first.
    egress: EgressMode,
    /// Media may load from `https` URLs, not only `data:` URLs and workspace
    /// files.
    remote_media: bool,
    /// Answers a waiting call, when the program can.
    waiter: Option<UiWaiter>,
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
            egress: EgressMode::Off,
            remote_media: false,
            waiter: None,
        })
    }

    /// Redact what surfaces show as the egress guardrail in `mode` redacts the
    /// model's text.
    pub fn with_egress(mut self, mode: EgressMode) -> Self {
        self.egress = mode;
        self
    }

    /// Let media load from `https` URLs too (ADR 0005: off by default, since a
    /// fetch can track the user or carry data out in its URL).
    pub fn with_remote_media(mut self, allow: bool) -> Self {
        self.remote_media = allow;
        self
    }

    /// Let a call wait for the user's answer, asking `waiter`.
    pub fn with_waiter(mut self, waiter: Option<UiWaiter>) -> Self {
        self.waiter = waiter;
        self
    }

    /// Whether a call may wait: someone answers.
    pub fn can_wait(&self) -> bool {
        self.waiter.is_some()
    }

    /// Ask the program for the user's answer (`None`: dismissed, or nobody
    /// answers).
    pub fn wait(&self, request: &UiWait) -> Option<Action> {
        self.waiter.as_ref().and_then(|waiter| waiter(request))
    }

    /// Whether `action` could have come from a surface up now: its surface
    /// exists (and is one of `among`, when given) and holds the component it
    /// names. What it carries is the user's and is checked as their input is.
    pub fn check_action(&self, action: &Action, among: Option<&[String]>) -> Result<(), String> {
        if action.name.is_empty() {
            return Err("the action has no name".into());
        }
        if among.is_some_and(|ids| !ids.contains(&action.surface_id)) {
            return Err(format!(
                "the action is for surface {:?}, which this call did not draw",
                action.surface_id
            ));
        }
        let runtime = self.runtime();
        let Some(mut components) = runtime.components(&action.surface_id) else {
            return Err(format!("there is no surface {:?}", action.surface_id));
        };
        if !components
            .any(|c| c.get("id").and_then(Value::as_str) == Some(&action.source_component_id))
        {
            return Err(format!(
                "surface {:?} has no component {:?}",
                action.surface_id, action.source_component_id
            ));
        }
        Ok(())
    }

    pub fn egress(&self) -> EgressMode {
        self.egress
    }

    pub fn remote_media(&self) -> bool {
        self.remote_media
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

    /// Every surface up now as the one `createSurface` that draws it as it is,
    /// for a program that reopens a session (or draws late) to bring its screen
    /// up to date.
    pub fn snapshot(&self) -> Vec<AgentMessage> {
        self.runtime()
            .snapshot()
            .into_iter()
            .filter_map(|message| serde_json::from_value(message).ok())
            .collect()
    }

    /// Rebuild the surfaces from a session's history (ADR 0005: resume replays,
    /// nothing is stored): every `render_ui` call that drew, in order, through
    /// the same redaction, policy, and validation as when it ran. A call that
    /// drew is one whose result is ok or opens with what the tool writes when it
    /// draws (a waiting call the user dismissed reports a failure, yet drew).
    /// Steps keep calls the history's compaction summarized away. Returns how
    /// many calls were replayed.
    pub fn replay(
        &self,
        state: &crate::session::AgentState,
        ctx: &crate::tools::ToolContext,
    ) -> usize {
        let mut runtime = match Runtime::new(self.catalogs.clone()) {
            Ok(runtime) => runtime,
            Err(_) => return 0,
        };
        runtime.set_rules(RULES);
        let mut replayed = 0;
        for step in &state.steps {
            for call in step.tool_calls.iter().filter(|call| call.name == RENDER_UI) {
                let drew = step
                    .results
                    .iter()
                    .find(|result| result.call_id == call.id)
                    .is_some_and(drew);
                if !drew {
                    continue;
                }
                // The history keeps the call as the model sent it, before the
                // registry repaired an array sent as a JSON string.
                let messages = match call.arguments.get("messages") {
                    Some(Value::Array(messages)) => messages.clone(),
                    Some(Value::String(text)) => match serde_json::from_str(text) {
                        Ok(Value::Array(messages)) => messages,
                        _ => continue,
                    },
                    _ => continue,
                };
                let messages: Vec<Value> = messages
                    .iter()
                    .map(|message| sanitize(message, &ctx.secrets, self.egress))
                    .collect();
                if !policy(&messages, &ctx.policy, self.remote_media).is_empty() {
                    continue;
                }
                if runtime.apply(&messages).is_ok() {
                    replayed += 1;
                }
            }
        }
        *self.runtime() = runtime;
        replayed
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
        let media = if self.remote_media {
            "a data: URL, an https URL, or the path of a file in the workspace"
        } else {
            "a data: URL or the path of a file in the workspace (remote URLs are not loaded)"
        };
        if self.can_wait() {
            section.push_str(
                "\n- With `\"wait\": true` the call waits for the user's first action on \
                 the surface and returns it as its result (`[ui action] ...`). Use it for a \
                 form you need answered before you can go on; otherwise leave it out and \
                 carry on.",
            );
        }
        section.push_str(&format!(
            "\n- Never ask for a password, key, or token in a surface (no TextField with \
             variant \"obscured\"): call request_secret, which asks through a masked prompt \
             and never shows you the value.\n- The url (and a Video's posterUrl) of Image, \
             Video, and AudioPlayer must be a literal string: {media}.\n- openUrl opens only \
             http and https URLs."
        ));
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

/// What a `render_ui` result carries for the loop, taken out of its metadata,
/// so it is emitted and recorded as agent86UI's own and never kept in the
/// session: the validated messages, and the `ui_render` trace fields.
pub fn take_drawn(metadata: &mut Map<String, Value>) -> (Vec<AgentMessage>, Option<Value>) {
    let messages = match metadata.remove(MESSAGES_KEY) {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| serde_json::from_value(item).ok())
            .collect(),
        _ => Vec::new(),
    };
    (messages, metadata.remove(TRACE_KEY))
}

/// What `render_ui`'s result opens with when the call drew.
const DREW: &[&str] = &["Rendered surface ", "Deleted surface ", "Applied."];

/// Whether a `render_ui` result says the call drew.
fn drew(result: &crate::types::ToolResult) -> bool {
    let text = if result.ok {
        result.content.as_str()
    } else {
        result.error.as_deref().unwrap_or_default()
    };
    result.ok || DREW.iter().any(|opening| text.starts_with(opening))
}

/// The surfaces a waiting call waits on, taken out of its result's metadata.
pub fn take_wait(metadata: &mut Map<String, Value>) -> Option<Vec<String>> {
    match metadata.remove(WAIT_KEY)? {
        Value::Array(ids) => Some(
            ids.into_iter()
                .filter_map(|id| id.as_str().map(String::from))
                .collect(),
        ),
        _ => None,
    }
}

/// An action as the model reads it, a waiting call's result or the next user
/// message: `[ui action] <name> on <surfaceId>: <context as JSON>`, then the
/// event's `userMessage` on a line of its own when it has one.
pub fn action_text(action: &Action) -> String {
    let context = crate::pyjson::python_json_dumps(&Value::Object(action.context.clone()), false);
    let mut text = format!(
        "[ui action] {} on {}: {context}",
        action.name, action.surface_id
    );
    if let Some(message) = action.user_message.as_deref().filter(|m| !m.is_empty()) {
        text.push('\n');
        text.push_str(message);
    }
    text
}

impl crate::agent::Harness {
    /// Wait for the user's answer to a `wait: true` call, through the prompt
    /// queue like a secret prompt (one question at a time, declined once the
    /// turn is cancelled), and make it the call's result. The answer goes
    /// through the vault and the ingress guardrail, as typed input does, and is
    /// recorded as `ui_action`, without its values.
    pub(crate) fn ui_wait(
        &self,
        call: &crate::types::ToolCall,
        drawn: crate::types::ToolResult,
        surfaces: Vec<String>,
        sid: &str,
    ) -> crate::types::ToolResult {
        use crate::tools::{failure, success};
        let Some(ui) = self.context.ui.as_deref() else {
            return drawn;
        };
        if surfaces.is_empty() {
            return drawn;
        }
        let request = UiWait {
            surface_ids: surfaces.clone(),
        };
        let question = crate::approval::Question {
            tool: Some(RENDER_UI.into()),
            call: Some(call.id.clone()),
            ..crate::approval::Question::new(crate::approval::QuestionKind::Ui)
        };
        let shown = drawn.content.clone();
        let answered = |result: crate::types::ToolResult| crate::types::ToolResult {
            call_id: drawn.call_id.clone(),
            ..result
        };
        let action = match self.gate.queue.ask(question, || ui.wait(&request)) {
            None => {
                return answered(failure(
                    RENDER_UI,
                    format!(
                        "{shown}\nThe turn was cancelled before the user answered. The surface \
                         stays up; an action on it later comes as a user message."
                    ),
                ))
            }
            Some(None) => {
                return answered(failure(
                    RENDER_UI,
                    format!(
                        "{shown}\nThe user did not answer (they dismissed it). The surface stays \
                         up; an action on it later comes as a user message."
                    ),
                ))
            }
            Some(Some(action)) => action,
        };
        if let Err(why) = ui.check_action(&action, Some(&surfaces)) {
            return answered(failure(
                RENDER_UI,
                format!("{shown}\nThe screen's answer could not be used: {why}."),
            ));
        }
        let text = self.context.secrets.redact(&action_text(&action));
        let report = self.guardrails.inspect_ingress(&text);
        if report.flagged() {
            self.recorder.event(
                sid,
                "guardrail",
                vec![
                    ("stage", serde_json::json!("ingress")),
                    ("source", serde_json::json!("ui_action")),
                    ("findings", serde_json::json!(report.summary())),
                ],
            );
        }
        self.recorder.event(
            sid,
            "ui_action",
            vec![
                ("surface", serde_json::json!(action.surface_id)),
                ("name", serde_json::json!(action.name)),
                ("component", serde_json::json!(action.source_component_id)),
                ("waited", serde_json::json!(true)),
            ],
        );
        if self.guardrails.should_block(&report) {
            return answered(failure(
                RENDER_UI,
                format!(
                    "{shown}\nThe user's answer was refused by the input guardrail ({}).",
                    report.summary()
                ),
            ));
        }
        answered(success(RENDER_UI, format!("{shown}\n{text}")))
    }
}

/// Every string in `value`, through the secret vault's redaction and, with the
/// egress guardrail in `redact` mode, its redaction too: what a surface shows
/// reaches the user as the model's text does. Object keys are left alone.
pub fn sanitize(value: &Value, vault: &crate::vault::SecretVault, egress: EgressMode) -> Value {
    match value {
        Value::String(text) => {
            let text = vault.redact(text);
            Value::String(if egress == EgressMode::Redact {
                crate::guardrails::redact(&text).0
            } else {
                text
            })
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| sanitize(item, vault, egress))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), sanitize(item, vault, egress)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Something agent86's policy refuses in a batch (ADR 0005), where the
/// protocol would allow it: a JSON Pointer into the batch and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "REFUSED at {}: {}", self.path, self.message)
    }
}

/// The media components of the basic catalog and the properties that load a
/// URL. A custom catalog's component of the same name is held to the same rule.
const MEDIA: &[(&str, &[&str])] = &[
    ("Image", &["url"]),
    ("Video", &["url", "posterUrl"]),
    ("AudioPlayer", &["url"]),
];

/// What agent86 refuses in a batch the protocol allows (ADR 0005):
/// - a `TextField` with `"variant": "obscured"`: secrets are never collected
///   through a surface, so the value never reaches the model;
/// - media from anywhere but a `data:` URL or a file in the workspace (or an
///   `https` URL, with `remote_media`), and from a binding or a call, whose
///   value is not known until the renderer resolves it;
/// - `openUrl` with a literal URL that is not `http` or `https` (a dynamic one
///   is the renderer's to check before it opens it, after showing it);
/// - `callRendererFunction` and agent functions, left out of the first version.
pub fn policy(
    messages: &[Value],
    jail: &crate::tools::SandboxPolicy,
    remote_media: bool,
) -> Vec<Refusal> {
    let mut refusals = Vec::new();
    let mut refuse = |path: String, message: String| refusals.push(Refusal { path, message });
    for (index, message) in messages.iter().enumerate() {
        if message.get("callRendererFunction").is_some() {
            refuse(
                format!("/{index}/callRendererFunction"),
                "renderer functions are not available here; draw with components and change \
                 them with updateComponents and updateDataModel instead"
                    .into(),
            );
        }
        if message.get("agentFunctionResponse").is_some() {
            refuse(
                format!("/{index}/agentFunctionResponse"),
                "agent functions are not available here".into(),
            );
        }
        for kind in ["createSurface", "updateComponents"] {
            let Some(components) = message
                .get(kind)
                .and_then(|body| body.get("components"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for (number, component) in components.iter().enumerate() {
                let at = format!("/{index}/{kind}/components/{number}");
                let type_name = component.get("component").and_then(Value::as_str);
                let id = component
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("this component");
                if type_name == Some("TextField")
                    && component.get("variant").and_then(Value::as_str) == Some("obscured")
                {
                    refuse(
                        format!("{at}/variant"),
                        format!(
                            "TextField {id} is \"obscured\", a password field, but a surface never \
                             collects secrets: call request_secret instead, which asks the user \
                             through a masked prompt and never shows you the value"
                        ),
                    );
                }
                if let Some((_, properties)) =
                    MEDIA.iter().find(|(name, _)| Some(*name) == type_name)
                {
                    for property in *properties {
                        let Some(value) = component.get(*property) else {
                            continue;
                        };
                        let path = format!("{at}/{property}");
                        match value.as_str() {
                            Some(url) => {
                                if let Some(why) = media_problem(url, jail, remote_media) {
                                    refuse(path, format!("{id}'s {property} {why}"));
                                }
                            }
                            None => refuse(
                                path,
                                format!(
                                    "{id}'s {property} must be a literal string: what a surface \
                                     loads is checked before it is drawn, so it cannot come from a \
                                     binding or a function call"
                                ),
                            ),
                        }
                    }
                }
                open_urls(component, &at, &mut refuse);
            }
        }
    }
    refusals
}

/// Why a media URL may not load, or `None` when it may.
fn media_problem(
    url: &str,
    jail: &crate::tools::SandboxPolicy,
    remote_media: bool,
) -> Option<String> {
    match scheme(url) {
        Some(scheme) if scheme == "data" => None,
        Some(scheme) if scheme == "https" && remote_media => None,
        Some(scheme) if scheme == "https" || scheme == "http" => Some(format!(
            "{url:?} is a remote URL, and remote media is off here (a fetch can track the user \
             or carry data out in its URL): use a data: URL or a file in the workspace"
        )),
        Some(scheme) => Some(format!(
            "{url:?} uses {scheme}:, which a surface may not load: use a data: URL or a file in \
             the workspace"
        )),
        None => match jail.resolve_within(url) {
            Err(_) => Some(format!("{url:?} is outside the workspace")),
            Ok(path) if !path.is_file() => Some(format!("{url:?} is not a file in the workspace")),
            Ok(_) => None,
        },
    }
}

/// A URL's scheme, lowercased, when it has one. A single letter before the
/// colon is a Windows drive, not a scheme.
fn scheme(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once(':')?;
    let valid = scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}

/// Refuse each `openUrl` call in `value` whose literal URL is not `http` or
/// `https`.
fn open_urls(value: &Value, at: &str, refuse: &mut impl FnMut(String, String)) {
    match value {
        Value::Object(map) => {
            if map.get("@call").and_then(Value::as_str) == Some("openUrl") {
                if let Some(url) = value.pointer("/args/url").and_then(Value::as_str) {
                    let scheme = scheme(url);
                    if !matches!(scheme.as_deref(), Some("http" | "https")) {
                        refuse(
                            format!("{at}/args/url"),
                            format!("openUrl only opens http and https URLs, not {url:?}"),
                        );
                    }
                }
            }
            for (key, item) in map {
                let segment = key.replace('~', "~0").replace('/', "~1");
                open_urls(item, &format!("{at}/{segment}"), refuse);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                open_urls(item, &format!("{at}/{index}"), refuse);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tools::SandboxPolicy;
    use serde_json::json;

    struct Jail(std::path::PathBuf, SandboxPolicy);

    fn jail(name: &str) -> Jail {
        let root = std::env::temp_dir().join(format!("agent86-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("img")).unwrap();
        std::fs::write(root.join("img/cat.png"), b"png").unwrap();
        let policy = SandboxPolicy::new(&root).unwrap();
        Jail(root, policy)
    }

    impl Drop for Jail {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn with_components(components: Value) -> Vec<Value> {
        vec![json!({"version": "1.0", "createSurface": {
            "surfaceId": "s", "catalogId": "c", "components": components
        }})]
    }

    fn refused(messages: &[Value], jail: &Jail, remote: bool) -> Vec<String> {
        policy(messages, &jail.1, remote)
            .into_iter()
            .map(|r| format!("{} {}", r.path, r.message))
            .collect()
    }

    #[test]
    fn a_password_field_is_refused_with_request_secret_named() {
        let jail = jail("obscured");
        let got = refused(
            &with_components(json!([
                {"id": "root", "component": "TextField", "label": "Password", "variant": "obscured"},
                {"id": "note", "component": "TextField", "label": "Note", "variant": "longText"}
            ])),
            &jail,
            false,
        );
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].starts_with("/0/createSurface/components/0/variant TextField root"));
        assert!(got[0].contains("request_secret"));
    }

    #[test]
    fn media_loads_only_data_urls_and_workspace_files_unless_remote_is_on() {
        let jail = jail("media");
        let image =
            |url: Value| with_components(json!([{"id": "root", "component": "Image", "url": url}]));
        let ok = |url: Value, remote| refused(&image(url), &jail, remote).is_empty();
        assert!(ok(json!("data:image/png;base64,iVBORw0KGgo="), false));
        assert!(ok(json!("img/cat.png"), false));
        assert!(!ok(json!("img/dog.png"), false), "missing file");
        assert!(!ok(json!("../outside.png"), false), "outside the jail");
        assert!(!ok(json!("https://example.com/cat.png"), false));
        assert!(ok(json!("https://example.com/cat.png"), true));
        assert!(!ok(json!("http://example.com/cat.png"), true), "only https");
        assert!(!ok(json!("file:///etc/passwd"), true));
        let bound = refused(&image(json!({"@path": "/url"})), &jail, true);
        assert!(bound[0].contains("must be a literal string"), "{bound:?}");
        // A Video's poster is held to the same rule.
        let video = with_components(json!([{"id": "root", "component": "Video",
            "url": "data:video/mp4;base64,AAAA", "posterUrl": "https://example.com/p.png"}]));
        let got = refused(&video, &jail, false);
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with("/0/createSurface/components/0/posterUrl"));
    }

    #[test]
    fn open_url_takes_only_http_and_https_literals() {
        let jail = jail("open");
        let button = |url: Value| {
            with_components(json!([
                {"id": "root", "component": "Button", "child": "t",
                 "action": {"functionCall": {"@call": "openUrl", "args": {"url": url}}}},
                {"id": "t", "component": "Text", "text": "Go"}
            ]))
        };
        assert!(refused(&button(json!("https://example.com")), &jail, false).is_empty());
        assert!(refused(&button(json!("http://example.com")), &jail, false).is_empty());
        // A dynamic URL is the renderer's to check, after showing it.
        assert!(refused(&button(json!({"@path": "/link"})), &jail, false).is_empty());
        let got = refused(&button(json!("javascript:alert(1)")), &jail, false);
        assert_eq!(
            got,
            ["/0/createSurface/components/0/action/functionCall/args/url openUrl only opens http and https URLs, not \"javascript:alert(1)\""]
        );
    }

    #[test]
    fn renderer_and_agent_functions_are_refused() {
        let jail = jail("functions");
        let got = refused(
            &[
                json!({"version": "1.0", "callRendererFunction": {"surfaceId": "s", "functionCallId": "f", "call": {"@call": "x"}}}),
                json!({"version": "1.0", "agentFunctionResponse": {"functionCallId": "f", "value": 1}}),
            ],
            &jail,
            false,
        );
        assert!(got[0].starts_with("/0/callRendererFunction renderer functions are not available"));
        assert!(got[1].starts_with("/1/agentFunctionResponse agent functions are not available"));
    }

    #[test]
    fn a_drive_letter_is_not_a_scheme() {
        assert_eq!(scheme("C:\\img\\cat.png"), None);
        assert_eq!(scheme("HTTPS://x"), Some("https".into()));
        assert_eq!(scheme("img/cat.png"), None);
    }

    #[test]
    fn sanitize_redacts_secrets_and_with_egress_redact_what_the_guardrail_finds() {
        let vault = crate::vault::SecretVault::default();
        vault.set("API_TOKEN", "tok-abcdef-123456".into());
        let message =
            json!({"text": "use tok-abcdef-123456", "aws": "AKIAABCDEFGHIJKLMNOP", "n": 3});
        let kept = sanitize(&message, &vault, EgressMode::Warn);
        assert_eq!(kept["text"], "use ${secret:API_TOKEN}");
        assert_eq!(kept["aws"], "AKIAABCDEFGHIJKLMNOP");
        assert_eq!(kept["n"], 3);
        let redacted = sanitize(&message, &vault, EgressMode::Redact);
        assert_ne!(redacted["aws"], "AKIAABCDEFGHIJKLMNOP");
        assert!(!redacted.to_string().contains("AKIA"), "{redacted}");
    }

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
