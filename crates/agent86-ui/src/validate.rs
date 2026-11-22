//! Validation of what an agent sends, as the agent's side of A2UI's
//! "prompt, generate, validate" loop: the schema check, then the rules a schema
//! cannot express, against the surfaces earlier messages created.
//!
//! [`Validator::check`] takes a batch of messages (one model tool call, say) and
//! reports every problem it finds; [`Validator::apply`] does the same and, when
//! there are no errors, records the batch's effect on the surfaces, so the next
//! batch is checked against them. A batch is all or nothing.
//!
//! Errors use the specification's codes and paths, so they can go back to the
//! model as the standard validation error format. Warnings are things a renderer
//! tolerates (a child not defined yet renders as a placeholder) but a model
//! probably did not mean.

use crate::catalog::{Callers, Catalog, ChildRef, ComponentDef};
use crate::ident::is_identifier;
use crate::message::AgentMessage;
use crate::schema::{CompileError, SchemaError, SchemaSet};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The specification's error codes that agent-side validation reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Code {
    /// The message or a component does not match the schema or a rule.
    ValidationFailed,
    /// A component sits under a parent its `allowedParents` does not name.
    UnallowedParent,
    /// A container holds a child its `allowedChildren` does not name.
    UnallowedChild,
    /// The message does not fit the surfaces' state (a surface created twice,
    /// or one updated before it exists).
    IntegrityError,
    /// A `callRendererFunction` the renderer would refuse.
    InvalidFunctionCall,
}

impl Code {
    /// The code as the specification writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::ValidationFailed => "VALIDATION_FAILED",
            Code::UnallowedParent => "UNALLOWED_PARENT",
            Code::UnallowedChild => "UNALLOWED_CHILD",
            Code::IntegrityError => "INTEGRITY_ERROR",
            Code::InvalidFunctionCall => "INVALID_FUNCTION_CALL",
        }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One problem found in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Problem {
    /// The specification's code.
    pub code: Code,
    /// The surface it concerns, when there is one.
    pub surface_id: Option<String>,
    /// A JSON Pointer into the batch (`/0/createSurface/components/2/text`),
    /// or `""` for a problem with no single location.
    pub path: String,
    /// One sentence on what is wrong.
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code)?;
        if !self.path.is_empty() {
            write!(f, " at {}", self.path)?;
        }
        write!(f, ": {}", self.message)
    }
}

/// What [`Validator::check`] found.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Report {
    /// Problems that make the batch invalid.
    pub errors: Vec<Problem>,
    /// Problems a renderer tolerates.
    pub warnings: Vec<Problem>,
    /// The batch as typed messages, when it has no errors.
    pub messages: Vec<AgentMessage>,
}

impl Report {
    /// Whether the batch is valid.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// What the validator remembers about a surface.
#[derive(Debug, Clone, Default)]
struct Surface {
    catalog_id: Option<String>,
    components: BTreeMap<String, Value>,
}

struct Entry {
    catalog: Catalog,
    schemas: SchemaSet,
}

/// The agent's view of a renderer: the catalogs it supports and the surfaces
/// sent to it so far.
pub struct Validator {
    catalogs: Vec<Entry>,
    surfaces: BTreeMap<String, Surface>,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator")
            .field("catalogs", &self.supported_catalog_ids())
            .field("surfaces", &self.surfaces.keys().collect::<Vec<_>>())
            .finish()
    }
}

const KINDS: &[&str] = &[
    "createSurface",
    "updateComponents",
    "updateDataModel",
    "deleteSurface",
    "callRendererFunction",
    "agentFunctionResponse",
];

fn problem(code: Code, surface: Option<&str>, path: String, message: impl Into<String>) -> Problem {
    Problem {
        code,
        surface_id: surface.map(String::from),
        path,
        message: message.into(),
    }
}

/// Escape one JSON Pointer segment.
fn segment(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

impl Validator {
    /// A validator for a renderer that supports `catalogs`. The first is used
    /// to check messages that name no catalog (a function response).
    pub fn new(catalogs: impl IntoIterator<Item = Catalog>) -> Result<Self, CompileError> {
        let catalogs = catalogs
            .into_iter()
            .map(|catalog| {
                let schemas = SchemaSet::for_catalog(&catalog)?;
                Ok(Entry { catalog, schemas })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;
        Ok(Self {
            catalogs,
            surfaces: BTreeMap::new(),
        })
    }

    /// A validator for a renderer that supports the basic catalog only.
    pub fn basic() -> Self {
        Self::new([Catalog::basic().clone()]).expect("the basic catalog compiles")
    }

    /// The ids of the catalogs this renderer supports, in the order given.
    pub fn supported_catalog_ids(&self) -> Vec<&str> {
        self.catalogs
            .iter()
            .map(|entry| entry.catalog.id())
            .collect()
    }

    /// The catalogs this renderer supports.
    pub fn catalogs(&self) -> impl Iterator<Item = &Catalog> {
        self.catalogs.iter().map(|entry| &entry.catalog)
    }

    /// The surfaces created and not deleted, as of the last applied batch.
    pub fn surface_ids(&self) -> impl Iterator<Item = &str> {
        self.surfaces.keys().map(String::as_str)
    }

    /// Forget every surface (the renderer was reset).
    pub fn clear(&mut self) {
        self.surfaces.clear();
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.catalogs.iter().find(|entry| entry.catalog.id() == id)
    }

    /// Check a batch without recording it.
    pub fn check(&self, messages: &[Value]) -> Report {
        self.run(messages).0
    }

    /// Check a batch and, when it has no errors, record its effect on the
    /// surfaces.
    pub fn apply(&mut self, messages: &[Value]) -> Report {
        let (report, surfaces) = self.run(messages);
        if report.is_ok() {
            self.surfaces = surfaces;
        }
        report
    }

    fn run(&self, messages: &[Value]) -> (Report, BTreeMap<String, Surface>) {
        let mut state = self.surfaces.clone();
        let mut report = Report::default();
        // Where each component of a touched surface was last defined in this
        // batch, so tree problems can point at it.
        let mut origins: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut touched = BTreeSet::new();

        for (index, raw) in messages.iter().enumerate() {
            let prefix = format!("/{index}");
            let before = report.errors.len();
            self.check_message(
                raw,
                &prefix,
                &mut state,
                &mut report,
                &mut origins,
                &mut touched,
            );
            if report.errors.len() == before {
                match serde_json::from_value::<AgentMessage>(raw.clone()) {
                    Ok(message) => report.messages.push(message),
                    Err(error) => report.errors.push(problem(
                        Code::ValidationFailed,
                        None,
                        prefix,
                        format!("message does not parse: {error}"),
                    )),
                }
            }
        }

        for surface_id in &touched {
            if let Some(surface) = state.get(surface_id) {
                self.check_tree(surface_id, surface, &origins, &mut report);
            }
        }
        if !report.is_ok() {
            report.messages.clear();
        }
        (report, state)
    }

    #[allow(clippy::too_many_arguments)]
    fn check_message(
        &self,
        raw: &Value,
        prefix: &str,
        state: &mut BTreeMap<String, Surface>,
        report: &mut Report,
        origins: &mut BTreeMap<(String, String), String>,
        touched: &mut BTreeSet<String>,
    ) {
        let present: Vec<&str> = KINDS
            .iter()
            .copied()
            .filter(|kind| raw.get(*kind).is_some())
            .collect();
        let [kind] = present.as_slice() else {
            let found = if present.is_empty() {
                "none".to_string()
            } else {
                present.join(" and ")
            };
            report.errors.push(problem(
                Code::ValidationFailed,
                None,
                prefix.to_string(),
                format!(
                    "a message must hold exactly one of {} (found {found})",
                    KINDS.join(", ")
                ),
            ));
            return;
        };
        let body = &raw[*kind];
        let surface_id = body.get("surfaceId").and_then(Value::as_str);
        let before = report.errors.len();

        match *kind {
            "createSurface" => {
                let catalog_id = body.get("catalogId").and_then(Value::as_str);
                if let Some(id) = surface_id {
                    if state.contains_key(id) {
                        report.errors.push(problem(
                            Code::IntegrityError,
                            Some(id),
                            format!("{prefix}/createSurface/surfaceId"),
                            format!(
                                "surface {id:?} already exists; delete it first or use a new surfaceId"
                            ),
                        ));
                    }
                }
                if let Some(id) = catalog_id {
                    if self.entry(id).is_none() {
                        report.errors.push(problem(
                            Code::ValidationFailed,
                            surface_id,
                            format!("{prefix}/createSurface/catalogId"),
                            format!(
                                "catalog {id:?} is not supported here; use one of: {}",
                                self.supported_catalog_ids().join(", ")
                            ),
                        ));
                    }
                }
                self.check_components(raw, kind, catalog_id, prefix, surface_id, report);
                if report.errors.len() == before {
                    if let Some(id) = surface_id {
                        let mut surface = Surface {
                            catalog_id: catalog_id.map(String::from),
                            ..Surface::default()
                        };
                        record(&mut surface, body, id, kind, prefix, origins);
                        state.insert(id.to_string(), surface);
                        touched.insert(id.to_string());
                    }
                }
            }
            "updateComponents" | "updateDataModel" | "deleteSurface" => {
                let known = surface_id.and_then(|id| state.get(id));
                if let (Some(id), None) = (surface_id, known) {
                    report.errors.push(problem(
                        Code::IntegrityError,
                        Some(id),
                        format!("{prefix}/{kind}/surfaceId"),
                        format!(
                            "surface {id:?} does not exist; create it with createSurface first"
                        ),
                    ));
                }
                let default = known.and_then(|surface| surface.catalog_id.clone());
                if surface_id.is_some() && known.is_none() {
                    // Nothing to resolve the catalog against: the integrity error says it.
                } else if *kind == "updateComponents" {
                    self.check_components(
                        raw,
                        kind,
                        default.as_deref(),
                        prefix,
                        surface_id,
                        report,
                    );
                } else {
                    let entry = default
                        .as_deref()
                        .and_then(|id| self.entry(id))
                        .or(self.catalogs.first());
                    if let Some(entry) = entry {
                        push_schema(
                            report,
                            surface_id,
                            prefix,
                            entry.schemas.check_agent_message_as(kind, raw),
                        );
                    }
                }
                if *kind == "updateDataModel" {
                    if let Some(path) = body.get("path").and_then(Value::as_str) {
                        if !path.is_empty() && !path.starts_with('/') {
                            report.errors.push(problem(
                                Code::ValidationFailed,
                                surface_id,
                                format!("{prefix}/updateDataModel/path"),
                                format!(
                                    "path {path:?} is not a JSON Pointer; it must start with \"/\""
                                ),
                            ));
                        }
                    }
                }
                if report.errors.len() == before {
                    if let Some(id) = surface_id {
                        match *kind {
                            "updateComponents" => {
                                if let Some(surface) = state.get_mut(id) {
                                    record(surface, body, id, kind, prefix, origins);
                                }
                                touched.insert(id.to_string());
                            }
                            "deleteSurface" => {
                                state.remove(id);
                                touched.remove(id);
                            }
                            _ => {}
                        }
                    }
                }
            }
            "callRendererFunction" => self.check_call(raw, prefix, state, report),
            _ => {
                if let Some(entry) = self.catalogs.first() {
                    push_schema(
                        report,
                        None,
                        prefix,
                        entry.schemas.check_agent_message_as(kind, raw),
                    );
                }
            }
        }
    }

    /// Schema-check a message that carries components, one catalog at a time:
    /// each component against the catalog it resolves to (its `@catalogId`,
    /// else the surface's default), so a surface can mix catalogs.
    fn check_components(
        &self,
        raw: &Value,
        kind: &str,
        default: Option<&str>,
        prefix: &str,
        surface: Option<&str>,
        report: &mut Report,
    ) {
        let components = raw[kind].get("components").and_then(Value::as_array);
        let Some(components) = components else {
            let entry = default
                .and_then(|id| self.entry(id))
                .or(self.catalogs.first());
            if let Some(entry) = entry {
                push_schema(
                    report,
                    surface,
                    prefix,
                    entry.schemas.check_agent_message_as(kind, raw),
                );
            }
            return;
        };

        let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        let mut ids = BTreeMap::new();
        for (index, component) in components.iter().enumerate() {
            let at = format!("{prefix}/{kind}/components/{index}");
            let own = component.get("@catalogId").and_then(Value::as_str);
            match own.or(default) {
                None => report.errors.push(problem(
                    Code::ValidationFailed,
                    surface,
                    at.clone(),
                    "this component has no catalog: set catalogId on createSurface or @catalogId on the component",
                )),
                Some(id) if self.entry(id).is_none() => {
                    // An unsupported surface default is reported once, on the surface.
                    if own.is_some() {
                        report.errors.push(problem(
                            Code::ValidationFailed,
                            surface,
                            format!("{at}/@catalogId"),
                            format!(
                                "catalog {id:?} is not supported here; use one of: {}",
                                self.supported_catalog_ids().join(", ")
                            ),
                        ));
                    }
                }
                Some(id) => groups.entry(id).or_default().push(index),
            }
            if let Some(id) = component.get("id").and_then(Value::as_str) {
                if !is_identifier(id) {
                    report.errors.push(problem(
                        Code::ValidationFailed,
                        surface,
                        format!("{at}/id"),
                        format!(
                            "component id {id:?} is not an identifier: start with a letter or _, then letters, digits, or _"
                        ),
                    ));
                }
                if let Some(first) = ids.insert(id.to_string(), index) {
                    report.errors.push(problem(
                        Code::ValidationFailed,
                        surface,
                        format!("{at}/id"),
                        format!("component id {id:?} is used twice in this message (also at index {first})"),
                    ));
                }
            }
            self.check_functions(component, own.or(default), &at, surface, report);
        }

        if groups.is_empty() {
            return;
        }
        let mut seen = BTreeSet::new();
        for (catalog_id, indices) in groups {
            let entry = self
                .entry(catalog_id)
                .expect("grouped by supported catalogs");
            let mut part = raw.clone();
            part[kind]["components"] =
                Value::Array(indices.iter().map(|&i| components[i].clone()).collect());
            if let Err(errors) = entry.schemas.check_agent_message_as(kind, &part) {
                for error in errors {
                    let path = remap(&error.path, kind, &indices);
                    if seen.insert((path.clone(), error.message.clone())) {
                        report.errors.push(problem(
                            Code::ValidationFailed,
                            surface,
                            format!("{prefix}{path}"),
                            error.message,
                        ));
                    }
                }
            }
        }
    }

    /// Function calls inside a component: one that needs a user gesture may
    /// only be an action's `functionCall`, and an `agentOnly` function cannot be
    /// used by a component at all (the specification's "Security Boundaries").
    /// Unknown functions are the schema's to report.
    fn check_functions(
        &self,
        component: &Value,
        catalog: Option<&str>,
        at: &str,
        surface: Option<&str>,
        report: &mut Report,
    ) {
        let mut calls = Vec::new();
        find_calls(component, at.to_string(), None, &mut calls);
        for (path, parent_key, call) in calls {
            let name = call
                .get("@call")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let catalog_id = call.get("catalogId").and_then(Value::as_str).or(catalog);
            let Some(def) = catalog_id
                .and_then(|id| self.entry(id))
                .and_then(|entry| entry.catalog.function(name))
            else {
                continue;
            };
            if def.requires_user_activation && parent_key.as_deref() != Some("functionCall") {
                report.errors.push(problem(
                    Code::ValidationFailed,
                    surface,
                    path.clone(),
                    format!(
                        "{name} needs a user gesture, so it can only be an action's functionCall, not a binding or a check"
                    ),
                ));
            }
            if def.callers == Callers::AgentOnly {
                report.errors.push(problem(
                    Code::ValidationFailed,
                    surface,
                    path,
                    format!("{name} is agentOnly: only the agent can call it, never a component"),
                ));
            }
        }
    }

    fn check_call(
        &self,
        raw: &Value,
        prefix: &str,
        state: &BTreeMap<String, Surface>,
        report: &mut Report,
    ) {
        let body = &raw["callRendererFunction"];
        let call = &body["callFunction"];
        let name = call
            .get("@call")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let catalog_id = call.get("catalogId").and_then(Value::as_str);
        let entry = catalog_id.and_then(|id| self.entry(id));
        match (catalog_id, entry) {
            (Some(id), None) => report.errors.push(problem(
                Code::InvalidFunctionCall,
                None,
                format!("{prefix}/callRendererFunction/callFunction/catalogId"),
                format!("catalog {id:?} is not supported here"),
            )),
            _ => {
                let entry = entry.or(self.catalogs.first());
                if let Some(entry) = entry {
                    push_schema(
                        report,
                        None,
                        prefix,
                        entry
                            .schemas
                            .check_agent_message_as("callRendererFunction", raw),
                    );
                }
            }
        }
        if let Some(def) = entry.and_then(|entry| entry.catalog.function(name)) {
            if def.callers == Callers::RendererOnly {
                report.errors.push(problem(
                    Code::InvalidFunctionCall,
                    None,
                    format!("{prefix}/callRendererFunction/callFunction/@call"),
                    format!(
                        "{name} cannot be called by the agent (allowedCallers is rendererOnly)"
                    ),
                ));
            }
        }
        if let Some(id) = body.get("surfaceId").and_then(Value::as_str) {
            if !state.contains_key(id) {
                report.errors.push(problem(
                    Code::InvalidFunctionCall,
                    Some(id),
                    format!("{prefix}/callRendererFunction/surfaceId"),
                    format!("surface {id:?} does not exist"),
                ));
            }
        }
    }

    /// The rules over a surface's whole component tree, after the batch:
    /// composition constraints, cycles, and (as warnings) missing children, a
    /// missing root, and components nothing shows.
    fn check_tree(
        &self,
        surface_id: &str,
        surface: &Surface,
        origins: &BTreeMap<(String, String), String>,
        report: &mut Report,
    ) {
        let at = |id: &str| {
            origins
                .get(&(surface_id.to_string(), id.to_string()))
                .cloned()
                .unwrap_or_default()
        };
        let def_of = |component: &Value| -> Option<&ComponentDef> {
            let type_name = component.get("component")?.as_str()?;
            let catalog = component
                .get("@catalogId")
                .and_then(Value::as_str)
                .or(surface.catalog_id.as_deref())?;
            self.entry(catalog)?.catalog.component(type_name)
        };

        let mut edges: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (id, component) in &surface.components {
            let Some(def) = def_of(component) else {
                continue;
            };
            let children = child_ids(component, def);
            for child in &children {
                match surface.components.get(child) {
                    None => report.warnings.push(problem(
                        Code::ValidationFailed,
                        Some(surface_id),
                        at(id),
                        format!(
                            "{id} refers to {child:?}, which is not defined yet; it shows as a placeholder until it is"
                        ),
                    )),
                    Some(child_component) => {
                        if let Some(allowed) = &def.allowed_children {
                            if !allowed.iter().any(|t| t == type_of(child_component)) {
                                report.errors.push(problem(
                                    Code::UnallowedChild,
                                    Some(surface_id),
                                    at(id),
                                    format!(
                                        "{} {id} may only contain {}, not {} {child}",
                                        type_of(component),
                                        allowed.join(", "),
                                        type_of(child_component)
                                    ),
                                ));
                            }
                        }
                        if let Some(allowed) = def_of(child_component).and_then(|d| d.allowed_parents.as_ref()) {
                            if !allowed.iter().any(|t| t == type_of(component)) {
                                report.errors.push(problem(
                                    Code::UnallowedParent,
                                    Some(surface_id),
                                    at(child),
                                    format!(
                                        "{} {child} may only be placed under {}, not {} {id}",
                                        type_of(child_component),
                                        allowed.join(", "),
                                        type_of(component)
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
            edges.insert(id.as_str(), children);
        }

        match surface.components.get("root") {
            None if !surface.components.is_empty() => report.warnings.push(problem(
                Code::ValidationFailed,
                Some(surface_id),
                String::new(),
                format!("surface {surface_id:?} has no component with id \"root\", so nothing is shown yet"),
            )),
            None => {}
            Some(root) => {
                if let Some(allowed) = def_of(root).and_then(|d| d.allowed_parents.as_ref()) {
                    if !allowed.iter().any(|t| t == "Surface") {
                        report.errors.push(problem(
                            Code::UnallowedParent,
                            Some(surface_id),
                            at("root"),
                            format!(
                                "{} cannot be the root: it may only be placed under {}",
                                type_of(root),
                                allowed.join(", ")
                            ),
                        ));
                    }
                }
                if let Some(cycle) = find_cycle(&edges) {
                    report.errors.push(problem(
                        Code::ValidationFailed,
                        Some(surface_id),
                        at(&cycle[0]),
                        format!("components form a cycle: {}", cycle.join(" -> ")),
                    ));
                } else {
                    let reachable = reach(&edges, "root");
                    let unseen: Vec<&str> = surface
                        .components
                        .keys()
                        .map(String::as_str)
                        .filter(|id| !reachable.contains(id))
                        .collect();
                    if !unseen.is_empty() {
                        report.warnings.push(problem(
                            Code::ValidationFailed,
                            Some(surface_id),
                            String::new(),
                            format!(
                                "not reachable from root, so not shown: {}",
                                unseen.join(", ")
                            ),
                        ));
                    }
                }
            }
        }
    }
}

/// A component's type name (`""` when it has none).
fn type_of(component: &Value) -> &str {
    component
        .get("component")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Add a message's components to a surface, noting where each came from.
fn record(
    surface: &mut Surface,
    body: &Value,
    surface_id: &str,
    kind: &str,
    prefix: &str,
    origins: &mut BTreeMap<(String, String), String>,
) {
    if let Some(components) = body.get("components").and_then(Value::as_array) {
        for (index, component) in components.iter().enumerate() {
            if let Some(id) = component.get("id").and_then(Value::as_str) {
                surface.components.insert(id.to_string(), component.clone());
                origins.insert(
                    (surface_id.to_string(), id.to_string()),
                    format!("{prefix}/{kind}/components/{index}"),
                );
            }
        }
    }
}

fn push_schema(
    report: &mut Report,
    surface: Option<&str>,
    prefix: &str,
    result: Result<(), Vec<SchemaError>>,
) {
    if let Err(errors) = result {
        for error in errors {
            report.errors.push(problem(
                Code::ValidationFailed,
                surface,
                format!("{prefix}{}", error.path),
                error.message,
            ));
        }
    }
}

/// Map a path into a partial message's components back to the full message's.
fn remap(path: &str, kind: &str, indices: &[usize]) -> String {
    let head = format!("/{kind}/components/");
    let Some(rest) = path.strip_prefix(&head) else {
        return path.to_string();
    };
    let (number, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    match number.parse::<usize>().ok().and_then(|n| indices.get(n)) {
        Some(original) => format!("{head}{original}{tail}"),
        None => path.to_string(),
    }
}

/// The ids a component refers to through its child-reference properties.
/// A template's `componentId` counts as a child.
fn child_ids(component: &Value, def: &ComponentDef) -> Vec<String> {
    let mut ids = Vec::new();
    for (property, kind) in &def.children {
        let Some(value) = component.get(property) else {
            continue;
        };
        match (kind, value) {
            (ChildRef::Single, Value::String(id)) => ids.push(id.clone()),
            (ChildRef::List, Value::Array(items)) => {
                ids.extend(items.iter().filter_map(Value::as_str).map(String::from));
            }
            (ChildRef::List, Value::Object(template)) => {
                if let Some(id) = template.get("componentId").and_then(Value::as_str) {
                    ids.push(id.to_string());
                }
            }
            _ => {}
        }
    }
    ids
}

/// Every function call (an object with a string `@call`) inside a value, with
/// its path and the key it sits under.
fn find_calls<'a>(
    value: &'a Value,
    path: String,
    key: Option<String>,
    calls: &mut Vec<(String, Option<String>, &'a Map<String, Value>)>,
) {
    match value {
        Value::Object(object) => {
            if object.get("@call").is_some_and(Value::is_string) {
                calls.push((path.clone(), key, object));
            }
            for (name, child) in object {
                find_calls(
                    child,
                    format!("{path}/{}", segment(name)),
                    Some(name.clone()),
                    calls,
                );
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                find_calls(item, format!("{path}/{index}"), key.clone(), calls);
            }
        }
        _ => {}
    }
}

fn find_cycle(edges: &BTreeMap<&str, Vec<String>>) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Open,
        Done,
    }
    fn visit(
        node: &str,
        edges: &BTreeMap<&str, Vec<String>>,
        marks: &mut BTreeMap<String, Mark>,
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        match marks.get(node) {
            Some(Mark::Done) => return None,
            Some(Mark::Open) => {
                let start = stack.iter().position(|n| n == node).unwrap_or(0);
                let mut cycle = stack[start..].to_vec();
                cycle.push(node.to_string());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(node.to_string(), Mark::Open);
        stack.push(node.to_string());
        for child in edges.get(node).into_iter().flatten() {
            if let Some(cycle) = visit(child, edges, marks, stack) {
                return Some(cycle);
            }
        }
        stack.pop();
        marks.insert(node.to_string(), Mark::Done);
        None
    }
    let mut marks = BTreeMap::new();
    for node in edges.keys() {
        if let Some(cycle) = visit(node, edges, &mut marks, &mut Vec::new()) {
            return Some(cycle);
        }
    }
    None
}

fn reach<'a>(edges: &'a BTreeMap<&str, Vec<String>>, start: &'a str) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![start];
    while let Some(node) = queue.pop() {
        if seen.insert(node) {
            for child in edges.get(node).into_iter().flatten() {
                queue.push(child.as_str());
            }
        }
    }
    seen
}
