//! Surfaces: the state a renderer keeps for each surface, and what it derives
//! from it.
//!
//! [`Runtime::apply`] takes a batch the way [`crate::Validator`] does (all or
//! nothing) and, beyond validating it, applies it: components are stored, the
//! data model updated (`null` deletes), and every touched surface evaluated, so
//! a template that does not parse or an `@index` outside a list is reported to
//! the agent rather than shown as a broken surface.
//!
//! [`Runtime::tree`] resolves a surface for drawing: a [`Node`] per component
//! instance (a list template gives one per item), its properties with every
//! binding and call evaluated, its failing checks, and its children.
//! [`Runtime::input`] writes what the user enters (two-way binding), and
//! [`Runtime::dispatch`] runs a component's action on the user's gesture,
//! giving the `action` to send the agent or the effects of a local function.

use super::data::{self, DataModel};
use super::functions::{coerce_bool, validity, Effect, EvalError, Evaluator, LocalFunction, Scope};
use crate::catalog::{Catalog, ChildRef, FunctionDef};
use crate::message::Action;
use crate::schema::CompileError;
use crate::validate::{problem, Code, Problem, Report, Validator};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// How deep a component tree may nest before resolution stops, so a cycle the
/// validator did not see cannot recurse without end.
pub const MAX_TREE_DEPTH: usize = 64;

#[derive(Debug, Clone)]
struct SurfaceState {
    catalog_id: Option<String>,
    send_data_model: bool,
    components: BTreeMap<String, Value>,
    data: DataModel,
}

/// One component instance, resolved for drawing.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Node {
    /// The component's id.
    pub id: String,
    /// The component type (`""` for a placeholder).
    pub component: String,
    /// The data context: `/` at the root, the item's path inside a template.
    pub data_path: String,
    /// The item's index inside the innermost template.
    pub index: Option<usize>,
    /// The properties with every binding and call resolved. Properties that
    /// only hold child ids and actions are left out: children are in
    /// [`Node::children`], and an action runs through [`Runtime::dispatch`]. A
    /// property holding children with other fields (Tabs' `tabs`, with each
    /// tab's title) is kept.
    pub properties: Map<String, Value>,
    /// `@accessibility`, resolved.
    pub accessibility: Option<Map<String, Value>>,
    /// The failing check results, in order, each with `valid`, `message`,
    /// `severity` (default `error`), and any `code`.
    pub failures: Vec<Value>,
    /// What failed to evaluate on this node (`EXPRESSION_ERROR`): the property
    /// resolved to `null`, the node still shows.
    pub errors: Vec<String>,
    /// The child nodes, by the property that holds them, in catalog order.
    pub children: Vec<(String, Vec<Node>)>,
    /// The component is referenced but not defined yet: draw a placeholder.
    pub placeholder: bool,
}

impl Node {
    /// Whether no check fails with severity `error` (a button with a failing
    /// check is disabled).
    pub fn is_valid(&self) -> bool {
        !self.failures.iter().any(is_error)
    }

    /// The messages of the failing `error` checks, in order.
    pub fn validation_errors(&self) -> Vec<String> {
        self.failures
            .iter()
            .filter(|failure| is_error(failure))
            .map(|failure| {
                failure
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    /// The first failing check, or `{"valid": true}`.
    pub fn validation_result(&self) -> Value {
        self.failures
            .first()
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"valid": true}))
    }

    /// Every node in this subtree, this one first.
    pub fn walk(&self) -> Vec<&Node> {
        let mut out = vec![self];
        for (_, nodes) in &self.children {
            for node in nodes {
                out.extend(node.walk());
            }
        }
        out
    }
}

fn is_error(failure: &Value) -> bool {
    failure
        .get("severity")
        .and_then(Value::as_str)
        .unwrap_or("error")
        == "error"
}

/// Why an action could not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
#[non_exhaustive]
pub struct ActionError {
    /// `INVALID_ACTION`, `EXPRESSION_ERROR`, or `EXECUTION_ERROR`.
    pub code: String,
    /// What went wrong.
    pub message: String,
    /// The surface the action was on.
    pub surface_id: String,
}

/// What a gesture produced.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct Dispatch {
    /// The `action` to send the agent, for an event.
    pub action: Option<Action>,
    /// What local functions asked for (`openUrl`); data writes are already
    /// applied.
    pub effects: Vec<Effect>,
}

/// Where a gesture came from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Gesture {
    /// The surface.
    pub surface_id: String,
    /// The component the user interacted with.
    pub source_component_id: String,
    /// The component instance's data context (its [`Node::data_path`]).
    pub data_path: String,
    /// Its template index (its [`Node::index`]).
    pub index: Option<usize>,
    /// Whether this is a real user activation (a click, a key press), which
    /// `openUrl` and other gesture-only functions require.
    pub activation: bool,
    /// When it happened, RFC 3339; `None` uses the current time.
    pub timestamp: Option<String>,
    /// Renderer metadata sent with the action (`{"extensions": {...}}`).
    pub metadata: Option<Value>,
}

impl Gesture {
    /// A user activation on a component at the root data context, now.
    pub fn new(surface_id: impl Into<String>, source_component_id: impl Into<String>) -> Self {
        Self {
            surface_id: surface_id.into(),
            source_component_id: source_component_id.into(),
            data_path: "/".into(),
            index: None,
            activation: true,
            timestamp: None,
            metadata: None,
        }
    }

    /// A user activation on a component instance.
    pub fn on(node: &Node, surface_id: impl Into<String>) -> Self {
        Self {
            surface_id: surface_id.into(),
            source_component_id: node.id.clone(),
            data_path: node.data_path.clone(),
            index: node.index,
            activation: true,
            timestamp: None,
            metadata: None,
        }
    }
}

/// A renderer's surfaces, headless.
#[derive(Debug)]
pub struct Runtime {
    validator: Validator,
    catalogs: Vec<Catalog>,
    evaluator: Evaluator,
    surfaces: BTreeMap<String, SurfaceState>,
}

impl Runtime {
    /// A runtime for a renderer that supports `catalogs`.
    pub fn new(catalogs: impl IntoIterator<Item = Catalog>) -> Result<Self, CompileError> {
        let catalogs: Vec<Catalog> = catalogs.into_iter().collect();
        Ok(Self {
            validator: Validator::new(catalogs.clone())?,
            evaluator: Evaluator::new(catalogs.clone()),
            catalogs,
            surfaces: BTreeMap::new(),
        })
    }

    /// A runtime for the basic catalog alone.
    pub fn basic() -> Self {
        Self::new([Catalog::basic().clone()]).expect("the basic catalog compiles")
    }

    /// The completeness rules batches are applied under from now on (see
    /// [`crate::Rules`]; strict by default).
    pub fn set_rules(&mut self, rules: crate::Rules) {
        self.validator.set_rules(rules);
    }

    /// Implement a function on the renderer (see [`Evaluator::register`]).
    pub fn register(&mut self, name: impl Into<String>, def: FunctionDef, function: LocalFunction) {
        self.evaluator.register(name, def, function);
    }

    /// The surfaces, by id.
    pub fn surface_ids(&self) -> impl Iterator<Item = &str> {
        self.surfaces.keys().map(String::as_str)
    }

    /// A surface's data model.
    pub fn data_model(&self, surface_id: &str) -> Option<&Value> {
        self.surfaces
            .get(surface_id)
            .map(|surface| surface.data.root())
    }

    /// A surface's components as the agent sent them (the latest of each id).
    pub fn components(&self, surface_id: &str) -> Option<impl Iterator<Item = &Value>> {
        self.surfaces
            .get(surface_id)
            .map(|surface| surface.components.values())
    }

    /// Whether the surface asked for its data model with every message.
    pub fn sends_data_model(&self, surface_id: &str) -> bool {
        self.surfaces
            .get(surface_id)
            .is_some_and(|surface| surface.send_data_model)
    }

    /// Each surface as the one `createSurface` message that draws it as it is
    /// now: its catalog, `sendDataModel` when set, its components (the latest of
    /// each id, by id), and its data model. A renderer that reopens, or joins
    /// late, can be brought up to date with these alone, in surface id order.
    pub fn snapshot(&self) -> Vec<Value> {
        self.surfaces
            .iter()
            .map(|(id, surface)| {
                let mut body = Map::new();
                body.insert("surfaceId".into(), Value::from(id.as_str()));
                if let Some(catalog) = &surface.catalog_id {
                    body.insert("catalogId".into(), Value::from(catalog.as_str()));
                }
                if surface.send_data_model {
                    body.insert("sendDataModel".into(), Value::Bool(true));
                }
                body.insert(
                    "components".into(),
                    surface.components.values().cloned().collect(),
                );
                body.insert("dataModel".into(), surface.data.root().clone());
                serde_json::json!({"version": crate::PROTOCOL_VERSION, "createSurface": body})
            })
            .collect()
    }

    /// The renderer's capabilities (`renderer_capabilities.json`): the ids of
    /// its catalogs and, with `inline`, the catalog documents themselves.
    pub fn capabilities(&self, inline: bool) -> Value {
        let mut caps = Map::new();
        caps.insert(
            "supportedCatalogIds".into(),
            self.catalogs.iter().map(|c| Value::from(c.id())).collect(),
        );
        if inline {
            caps.insert(
                "inlineCatalogs".into(),
                self.catalogs.iter().map(|c| c.document().clone()).collect(),
            );
        }
        serde_json::json!({ crate::PROTOCOL_VERSION: caps })
    }

    /// The renderer data model to send with a message (`renderer_data_model.json`):
    /// the surfaces that set `sendDataModel`, or `None` if none did.
    pub fn renderer_data_model(&self) -> Option<Value> {
        let surfaces: Map<String, Value> = self
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.send_data_model)
            .map(|(id, surface)| (id.clone(), surface.data.root().clone()))
            .collect();
        (!surfaces.is_empty())
            .then(|| serde_json::json!({"version": crate::PROTOCOL_VERSION, "surfaces": surfaces}))
    }

    /// Validate and apply a batch, all or nothing. See the module docs.
    pub fn apply(&mut self, messages: &[Value]) -> Report {
        let mut report = self.validator.check(messages);
        if !report.is_ok() {
            return report;
        }
        let mut surfaces = self.surfaces.clone();
        let mut touched = BTreeSet::new();
        for (index, message) in messages.iter().enumerate() {
            if let Err(problem) = apply_one(&mut surfaces, message, index, &mut touched) {
                report.errors.push(problem);
            }
        }
        for id in &touched {
            if let Some(surface) = surfaces.get(id) {
                for (path, error) in self.evaluate_all(surface) {
                    report
                        .errors
                        .push(problem(Code::ExpressionError, Some(id), path, error));
                }
            }
        }
        if report.is_ok() {
            // Record the batch in the validator too, so later batches are
            // checked against these surfaces.
            let recorded = self.validator.apply(messages);
            debug_assert!(recorded.is_ok());
            self.surfaces = surfaces;
        } else {
            report.messages.clear();
        }
        report
    }

    /// Every evaluation error a surface's tree has, as (path, message).
    fn evaluate_all(&self, surface: &SurfaceState) -> Vec<(String, String)> {
        let mut errors = Vec::new();
        if surface.components.contains_key("root") {
            let tree = self.resolve(surface, "root", "/", None, 0, &mut Vec::new());
            for node in tree.walk() {
                for error in &node.errors {
                    errors.push((String::new(), format!("{}: {error}", node.id)));
                }
            }
        }
        errors
    }

    /// Resolve a surface for drawing, from its `root` (`None` when the
    /// surface does not exist or has no root yet).
    pub fn tree(&self, surface_id: &str) -> Option<Node> {
        let surface = self.surfaces.get(surface_id)?;
        surface
            .components
            .contains_key("root")
            .then(|| self.resolve(surface, "root", "/", None, 0, &mut Vec::new()))
    }

    /// Resolve one component at the root data context, whether or not it is
    /// reachable from `root`.
    pub fn node(&self, surface_id: &str, component_id: &str) -> Option<Node> {
        let surface = self.surfaces.get(surface_id)?;
        surface
            .components
            .contains_key(component_id)
            .then(|| self.resolve(surface, component_id, "/", None, 0, &mut Vec::new()))
    }

    fn resolve(
        &self,
        surface: &SurfaceState,
        id: &str,
        data_path: &str,
        index: Option<usize>,
        depth: usize,
        stack: &mut Vec<String>,
    ) -> Node {
        let mut node = Node {
            id: id.to_string(),
            component: String::new(),
            data_path: data_path.to_string(),
            index,
            properties: Map::new(),
            accessibility: None,
            failures: Vec::new(),
            errors: Vec::new(),
            children: Vec::new(),
            placeholder: false,
        };
        let Some(component) = surface.components.get(id) else {
            node.placeholder = true;
            return node;
        };
        if depth > MAX_TREE_DEPTH || stack.iter().any(|seen| seen == id) {
            node.placeholder = true;
            node.errors
                .push("the component tree nests too deeply or forms a cycle here".into());
            return node;
        }
        node.component = component
            .get("component")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let catalog_id = component
            .get("@catalogId")
            .and_then(Value::as_str)
            .or(surface.catalog_id.as_deref());
        let def = catalog_id
            .and_then(|id| self.catalogs.iter().find(|catalog| catalog.id() == id))
            .and_then(|catalog| catalog.component(&node.component))
            .cloned()
            .unwrap_or_default();

        let mut scope = Scope::root(&surface.data, surface.catalog_id.as_deref());
        scope.path = data_path.to_string();
        scope.index = index;

        let object = component.as_object().cloned().unwrap_or_default();
        for (key, value) in &object {
            // An items property (Tabs' `tabs`) holds more than ids (each tab's
            // title), so it stays among the properties as well.
            let only_ids = matches!(
                def.children.get(key),
                Some(ChildRef::Single | ChildRef::List)
            );
            if matches!(
                key.as_str(),
                "id" | "component" | "@catalogId" | "@metadata" | "@checks"
            ) || only_ids
                || def.actions.contains(key)
            {
                continue;
            }
            if key == "@accessibility" {
                let resolved = self.deep(value, &scope, &mut node.errors, key);
                node.accessibility = resolved.as_object().cloned();
                continue;
            }
            let resolved = self.deep(value, &scope, &mut node.errors, key);
            node.properties.insert(key.clone(), resolved);
        }
        // Checks: in order, failures only.
        if let Some(checks) = object.get("@checks").and_then(Value::as_array) {
            for rule in checks {
                if let Some(failure) = self.check(rule, &scope) {
                    node.failures.push(failure);
                }
            }
        }
        // Children.
        stack.push(id.to_string());
        for (property, kind) in &def.children {
            let Some(value) = object.get(property) else {
                continue;
            };
            let mut nodes = Vec::new();
            match (kind, value) {
                (ChildRef::Single, Value::String(child)) => {
                    nodes.push(self.resolve(surface, child, data_path, index, depth + 1, stack));
                }
                (ChildRef::List, Value::Array(ids)) => {
                    for child in ids.iter().filter_map(Value::as_str) {
                        nodes.push(self.resolve(
                            surface,
                            child,
                            data_path,
                            index,
                            depth + 1,
                            stack,
                        ));
                    }
                }
                (ChildRef::List, Value::Object(template)) => {
                    let template_id = template
                        .get("componentId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let list_path = data::resolve(
                        data_path,
                        template.get("path").and_then(Value::as_str).unwrap_or(""),
                    );
                    match surface.data.get(&list_path) {
                        Ok(Some(Value::Array(items))) => {
                            let base = list_path.trim_end_matches('/');
                            for i in 0..items.len() {
                                let item_path = format!("{base}/{i}");
                                nodes.push(self.resolve(
                                    surface,
                                    template_id,
                                    &item_path,
                                    Some(i),
                                    depth + 1,
                                    stack,
                                ));
                            }
                        }
                        Ok(_) => {}
                        Err(error) => node.errors.push(format!("{property}: {error}")),
                    }
                }
                (ChildRef::Items(field), Value::Array(items)) => {
                    for child in items
                        .iter()
                        .filter_map(|item| item.get(field).and_then(Value::as_str))
                    {
                        nodes.push(self.resolve(
                            surface,
                            child,
                            data_path,
                            index,
                            depth + 1,
                            stack,
                        ));
                    }
                }
                _ => {}
            }
            node.children.push((property.clone(), nodes));
        }
        stack.pop();
        node
    }

    /// Resolve every binding and call inside a property value, deeply; a
    /// failure becomes `null` and a recorded error.
    fn deep(&self, value: &Value, scope: &Scope<'_>, errors: &mut Vec<String>, at: &str) -> Value {
        let dynamic = value.as_object().is_some_and(|map| {
            map.contains_key("@path") || map.get("@call").is_some_and(Value::is_string)
        });
        if dynamic {
            return match self.evaluator.evaluate(value, scope) {
                Ok(resolved) => {
                    // Effects from a binding (a write, an open) never happen.
                    scope.take_effects();
                    resolved
                }
                Err(error) => {
                    errors.push(format!("{at}: {error}"));
                    Value::Null
                }
            };
        }
        match value {
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| self.deep(item, scope, errors, &format!("{at}/{i}")))
                    .collect(),
            ),
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, inner) in map {
                    let key_out = key
                        .strip_prefix("@@")
                        .map_or_else(|| key.clone(), |rest| format!("@{rest}"));
                    out.insert(
                        key_out,
                        self.deep(inner, scope, errors, &format!("{at}/{key}")),
                    );
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    /// Evaluate one check rule, returning its failure (normalized) or `None`
    /// when it passes. A boolean or a validation result from a function takes
    /// the rule's `message` when it has one; a validation result read from the
    /// data model keeps its own message (the specification's coercion rules
    /// and the shared suites' comparison rules).
    fn check(&self, rule: &Value, scope: &Scope<'_>) -> Option<Value> {
        let condition = rule.get("condition")?;
        let rule_message = rule.get("message").and_then(Value::as_str);
        let from_data = condition.get("@path").is_some();
        let outcome = self.evaluator.evaluate(condition, scope);
        scope.take_effects();
        let mut failure = match outcome {
            Ok(value) => match validity(&value) {
                Some(true) => return None,
                Some(false) => {
                    let mut failure = value.as_object().cloned().unwrap_or_default();
                    let own = failure
                        .get("message")
                        .and_then(Value::as_str)
                        .map(String::from);
                    let message = if from_data {
                        own.or(rule_message.map(String::from))
                    } else {
                        rule_message.map(String::from).or(own)
                    };
                    if let Some(message) = message {
                        failure.insert("message".into(), Value::String(message));
                    }
                    failure
                }
                None => {
                    if coerce_bool(&value) {
                        return None;
                    }
                    let mut failure = Map::new();
                    failure.insert("valid".into(), Value::Bool(false));
                    if let Some(message) = rule_message {
                        failure.insert("message".into(), Value::String(message.into()));
                    }
                    failure
                }
            },
            Err(error) => {
                let mut failure = Map::new();
                failure.insert("valid".into(), Value::Bool(false));
                failure.insert(
                    "message".into(),
                    Value::String(rule_message.map_or_else(|| error.to_string(), String::from)),
                );
                failure
            }
        };
        failure
            .entry("severity")
            .or_insert_with(|| Value::String("error".into()));
        Some(Value::Object(failure))
    }

    /// Write what the user entered at an absolute data model path (two-way
    /// binding): the input component's bound path, resolved against its
    /// node's data context.
    pub fn input(
        &mut self,
        surface_id: &str,
        path: &str,
        value: Value,
    ) -> Result<(), data::DataError> {
        let Some(surface) = self.surfaces.get_mut(surface_id) else {
            return Ok(());
        };
        surface.data.set(path, value).map(|_| ())
    }

    /// Run an action (a component's `action` property, as sent) for a
    /// gesture: an event becomes the `action` message to send, with its context
    /// resolved; a function call runs locally, its data writes applied and its
    /// other effects returned.
    pub fn dispatch(&mut self, action: &Value, gesture: &Gesture) -> Result<Dispatch, ActionError> {
        let fail = |code: &str, message: String| ActionError {
            code: code.into(),
            message,
            surface_id: gesture.surface_id.clone(),
        };
        let Some(surface) = self.surfaces.get(&gesture.surface_id) else {
            return Err(fail(
                "INVALID_ACTION",
                format!("surface {:?} does not exist", gesture.surface_id),
            ));
        };
        let mut scope = Scope::root(&surface.data, surface.catalog_id.as_deref());
        scope.path = gesture.data_path.clone();
        scope.index = gesture.index;
        scope.activation = gesture.activation;

        if let Some(call) = action.get("functionCall").and_then(Value::as_object) {
            let outcome = self.evaluator.call(call, &scope);
            let effects = scope.take_effects();
            match outcome {
                Ok(_) => {}
                Err(EvalError::Execution(message)) => return Err(fail("EXECUTION_ERROR", message)),
                Err(error) => return Err(fail("EXPRESSION_ERROR", error.to_string())),
            }
            let (writes, effects): (Vec<Effect>, Vec<Effect>) = effects
                .into_iter()
                .partition(|effect| matches!(effect, Effect::SetData { .. }));
            drop(scope);
            if let Some(surface) = self.surfaces.get_mut(&gesture.surface_id) {
                for write in writes {
                    if let Effect::SetData { path, value } = write {
                        surface
                            .data
                            .set(&path, value)
                            .map_err(|e| fail("EXECUTION_ERROR", e.message))?;
                    }
                }
            }
            return Ok(Dispatch {
                action: None,
                effects,
            });
        }

        let Some(event) = action.get("event").and_then(Value::as_object) else {
            return Err(fail(
                "INVALID_ACTION",
                "an action holds an event or a functionCall".into(),
            ));
        };
        let Some(name) = event.get("name").and_then(Value::as_str) else {
            return Err(fail(
                "INVALID_ACTION",
                "an event needs a string name".into(),
            ));
        };
        let mut context = Map::new();
        if let Some(entries) = event.get("context").and_then(Value::as_object) {
            for (key, value) in entries {
                let resolved = self
                    .evaluator
                    .evaluate(value, &scope)
                    .map_err(|error| fail("EXPRESSION_ERROR", format!("context {key}: {error}")))?;
                context.insert(key.clone(), resolved);
            }
        }
        let user_message = match event.get("userMessage") {
            None => None,
            Some(value @ (Value::String(_) | Value::Object(_))) => {
                match self.evaluator.evaluate(value, &scope) {
                    Ok(Value::String(text)) => Some(text),
                    Ok(Value::Null) => None,
                    Ok(other) => Some(super::format::to_string(&other)),
                    Err(error) => {
                        return Err(fail("EXPRESSION_ERROR", format!("userMessage: {error}")))
                    }
                }
            }
            Some(_) => {
                return Err(fail(
                    "INVALID_ACTION",
                    "userMessage must be a string or a binding".into(),
                ))
            }
        };
        scope.take_effects();
        if let Some(metadata) = &gesture.metadata {
            let keys = metadata
                .get("extensions")
                .and_then(Value::as_object)
                .into_iter()
                .flatten();
            for (key, _) in keys {
                if !crate::ident::is_identifier(key) {
                    return Err(fail(
                        "VALIDATION_FAILED",
                        format!("metadata extension key {key:?} is not an identifier"),
                    ));
                }
            }
        }
        let timestamp = gesture
            .timestamp
            .clone()
            .unwrap_or_else(|| jiff::Timestamp::now().to_string());
        let mut action = Action::new(
            name,
            &gesture.surface_id,
            &gesture.source_component_id,
            timestamp,
            context,
        );
        action.user_message = user_message;
        action.metadata = gesture.metadata.clone();
        Ok(Dispatch {
            action: Some(action),
            effects: Vec::new(),
        })
    }
}

/// Apply one validated message to the surfaces.
fn apply_one(
    surfaces: &mut BTreeMap<String, SurfaceState>,
    message: &Value,
    index: usize,
    touched: &mut BTreeSet<String>,
) -> Result<(), Problem> {
    let data_error = |id: &str, kind: &str, error: data::DataError| {
        problem(
            Code::DataError,
            Some(id),
            format!("/{index}/{kind}/path"),
            error.message,
        )
    };
    if let Some(body) = message.get("createSurface") {
        let id = body["surfaceId"].as_str().unwrap_or_default().to_string();
        let mut state = SurfaceState {
            catalog_id: body
                .get("catalogId")
                .and_then(Value::as_str)
                .map(String::from),
            send_data_model: body
                .get("sendDataModel")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            components: BTreeMap::new(),
            data: DataModel::new(
                body.get("dataModel")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new())),
            ),
        };
        store(&mut state, body);
        surfaces.insert(id.clone(), state);
        touched.insert(id);
    } else if let Some(body) = message.get("updateComponents") {
        let id = body["surfaceId"].as_str().unwrap_or_default();
        if let Some(state) = surfaces.get_mut(id) {
            store(state, body);
            touched.insert(id.to_string());
        }
    } else if let Some(body) = message.get("updateDataModel") {
        let id = body["surfaceId"].as_str().unwrap_or_default();
        if let Some(state) = surfaces.get_mut(id) {
            let path = body.get("path").and_then(Value::as_str).unwrap_or("/");
            let value = body.get("value").cloned().unwrap_or(Value::Null);
            let outcome = if value.is_null() {
                state.data.delete(path)
            } else {
                state.data.set(path, value)
            };
            outcome.map_err(|error| data_error(id, "updateDataModel", error))?;
            touched.insert(id.to_string());
        }
    } else if let Some(body) = message.get("deleteSurface") {
        let id = body["surfaceId"].as_str().unwrap_or_default();
        surfaces.remove(id);
        touched.remove(id);
    }
    Ok(())
}

fn store(state: &mut SurfaceState, body: &Value) {
    for component in body
        .get("components")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = component.get("id").and_then(Value::as_str) {
            state.components.insert(id.to_string(), component.clone());
        }
    }
}
