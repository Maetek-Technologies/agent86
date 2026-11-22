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
    /// A dynamic value cannot be evaluated: a template that does not parse,
    /// `@index` outside a list template, a function that cannot be resolved
    /// (found by [`crate::runtime::Runtime`], which evaluates the surface).
    ExpressionError,
    /// An `updateDataModel` write the path does not allow (through a string,
    /// a non-numeric segment on an array). Sent on the wire as
    /// `VALIDATION_FAILED`, the code for a message field that fails.
    DataError,
    /// A limit the reference SDKs share was passed: values nested more than
    /// [`MAX_DEPTH`] deep, components more than [`MAX_DEPTH`] below `root`,
    /// function calls nested more than [`MAX_CALL_DEPTH`] deep, or components
    /// that refer to each other in a cycle. Sent as `RECURSION_LIMIT_EXCEEDED`,
    /// the reference SDKs' code; the specification leaves codes other than its
    /// three validation ones open.
    RecursionLimitExceeded,
    /// A component or a function call resolves to no catalog this renderer
    /// has: the one it names is not supported, or it names none and its surface
    /// has no default. Sent as `CATALOG_NOT_FOUND`, the reference SDKs' code.
    CatalogNotFound,
}

/// How deep values may nest in a message, and components below `root`, as in
/// the reference SDKs.
pub const MAX_DEPTH: usize = 50;

/// How deep function calls may nest, as in the reference SDKs.
pub const MAX_CALL_DEPTH: usize = 5;

impl Code {
    /// The code as the specification writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::ValidationFailed => "VALIDATION_FAILED",
            Code::UnallowedParent => "UNALLOWED_PARENT",
            Code::UnallowedChild => "UNALLOWED_CHILD",
            Code::IntegrityError => "INTEGRITY_ERROR",
            Code::InvalidFunctionCall => "INVALID_FUNCTION_CALL",
            Code::ExpressionError => "EXPRESSION_ERROR",
            Code::DataError => "VALIDATION_FAILED",
            Code::RecursionLimitExceeded => "RECURSION_LIMIT_EXCEEDED",
            Code::CatalogNotFound => "CATALOG_NOT_FOUND",
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

/// How strictly a surface's component tree must be complete once a batch is
/// applied: the reference SDKs' `ValidationConfig`, without its version and
/// message filters.
///
/// [`Rules::STRICT`] (the default, as in the reference SDKs) makes each problem
/// an error, which suits a batch that is meant to be whole (one tool call).
/// [`Rules::RELAXED`] makes each a warning, for a renderer drawing a stream that
/// is completed later. A cycle or a broken composition rule is an error either
/// way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Rules {
    /// A surface with components but none with id `root` is accepted.
    pub allow_missing_root: bool,
    /// A component may refer to a child that is not defined.
    pub allow_dangling_references: bool,
    /// A component nothing reachable from `root` shows is accepted.
    pub allow_orphans: bool,
}

impl Rules {
    /// Every problem is an error.
    pub const STRICT: Rules = Rules {
        allow_missing_root: false,
        allow_dangling_references: false,
        allow_orphans: false,
    };
    /// Every problem is a warning.
    pub const RELAXED: Rules = Rules {
        allow_missing_root: true,
        allow_dangling_references: true,
        allow_orphans: true,
    };
    /// These rules with a missing root allowed or not.
    pub const fn allowing_missing_root(mut self, allow: bool) -> Self {
        self.allow_missing_root = allow;
        self
    }

    /// These rules with dangling references allowed or not.
    pub const fn allowing_dangling_references(mut self, allow: bool) -> Self {
        self.allow_dangling_references = allow;
        self
    }

    /// These rules with orphans allowed or not.
    pub const fn allowing_orphans(mut self, allow: bool) -> Self {
        self.allow_orphans = allow;
        self
    }
}

impl Default for Rules {
    fn default() -> Self {
        Rules::STRICT
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
    rules: Rules,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator")
            .field("catalogs", &self.supported_catalog_ids())
            .field("surfaces", &self.surfaces.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Message kinds of A2UI 0.8 and 0.9 that 1.0 renamed or dropped, named in
/// the error so a model trained on them sees why.
const LEGACY_KINDS: &[&str] = &[
    "beginRendering",
    "surfaceUpdate",
    "dataModelUpdate",
    "updateSurface",
];

const KINDS: &[&str] = &[
    "createSurface",
    "updateComponents",
    "updateDataModel",
    "deleteSurface",
    "callRendererFunction",
    "agentFunctionResponse",
];

pub(crate) fn problem(
    code: Code,
    surface: Option<&str>,
    path: String,
    message: impl Into<String>,
) -> Problem {
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
            rules: Rules::default(),
        })
    }

    /// The completeness rules batches are checked under from now on.
    pub fn set_rules(&mut self, rules: Rules) {
        self.rules = rules;
    }

    /// The completeness rules in force.
    pub fn rules(&self) -> Rules {
        self.rules
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

    /// [`Validator::check`], with the batch as checked: each component's
    /// legacy envelope keys (`catalogId`, `metadata`, `accessibility`, `checks`
    /// where its schema does not declare them) moved to their `@` spellings.
    pub(crate) fn check_normalized(&self, messages: &[Value]) -> (Report, Vec<Value>) {
        let (report, _, normalized) = self.run(messages);
        (report, normalized)
    }

    /// Check a batch and, when it has no errors, record its effect on the
    /// surfaces.
    pub fn apply(&mut self, messages: &[Value]) -> Report {
        let (report, surfaces, _) = self.run(messages);
        if report.is_ok() {
            self.surfaces = surfaces;
        }
        report
    }

    fn run(&self, messages: &[Value]) -> (Report, BTreeMap<String, Surface>, Vec<Value>) {
        let mut state = self.surfaces.clone();
        let mut report = Report::default();
        // Where each component of a touched surface was last defined in this
        // batch, so tree problems can point at it.
        let mut origins: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut touched = BTreeSet::new();
        let mut normalized = Vec::with_capacity(messages.len());

        for (index, raw) in messages.iter().enumerate() {
            let prefix = format!("/{index}");
            let before = report.errors.len();
            // Limits and path syntax first, over the raw message: a value nested
            // too deep is not worth a schema check.
            if let Err(problem) = limits_and_paths(raw, &prefix, 1, 0) {
                report.errors.push(problem);
                normalized.push(raw.clone());
                continue;
            }
            let message = self.normalize(raw, &prefix, &state, &mut report);
            let raw = &message;
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
            normalized.push(message);
        }

        for surface_id in &touched {
            if let Some(surface) = state.get(surface_id) {
                self.check_tree(surface_id, surface, &origins, &mut report);
            }
        }
        if !report.is_ok() {
            report.messages.clear();
        }
        (report, state, normalized)
    }

    /// A message with each component's legacy envelope keys moved to their `@`
    /// spellings, as renderers accept them during the 1.0 transition (the
    /// specification's evolution guide): a bare `catalogId` the component's
    /// schema in the surface's default catalog does not declare routes it like
    /// `@catalogId`, and a bare `metadata`, `accessibility`, or `checks` its
    /// schema in the catalog it resolves to does not declare is that `@` key. A
    /// component carrying both spellings of one is refused. A declared property
    /// is the component's own and stays as it is.
    fn normalize(
        &self,
        raw: &Value,
        prefix: &str,
        state: &BTreeMap<String, Surface>,
        report: &mut Report,
    ) -> Value {
        let mut message = raw.clone();
        for kind in ["createSurface", "updateComponents"] {
            let Some(body) = message.get_mut(kind) else {
                continue;
            };
            let default = match kind {
                "createSurface" => body
                    .get("catalogId")
                    .and_then(Value::as_str)
                    .map(String::from),
                _ => body
                    .get("surfaceId")
                    .and_then(Value::as_str)
                    .and_then(|id| state.get(id))
                    .and_then(|surface| surface.catalog_id.clone()),
            };
            let surface = body
                .get("surfaceId")
                .and_then(Value::as_str)
                .map(String::from);
            let Some(components) = body.get_mut("components").and_then(Value::as_array_mut) else {
                continue;
            };
            for (index, component) in components.iter_mut().enumerate() {
                let Some(object) = component.as_object_mut() else {
                    continue;
                };
                let at = format!("{prefix}/{kind}/components/{index}");
                let type_name = object
                    .get("component")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let declares = |catalog: Option<&str>, key: &str| {
                    catalog
                        .and_then(|id| self.entry(id))
                        .is_some_and(|entry| entry.catalog.declares_own(&type_name, key))
                };
                let legacy = |object: &mut Map<String, Value>,
                              key: &str,
                              declared: bool,
                              report: &mut Report| {
                    if declared || !object.contains_key(key) {
                        return;
                    }
                    let prefixed = format!("@{key}");
                    if object.contains_key(&prefixed) {
                        report.errors.push(problem(
                            Code::ValidationFailed,
                            surface.as_deref(),
                            format!("{at}/{key}"),
                            format!(
                                "this component has both {key} and {prefixed}; write only {prefixed}"
                            ),
                        ));
                        // Said once: the bare key's schema complaint would only
                        // repeat it.
                        object.remove(key);
                    } else if let Some(value) = object.remove(key) {
                        object.insert(prefixed, value);
                    }
                };
                let declared = declares(default.as_deref(), "catalogId");
                legacy(object, "catalogId", declared, report);
                let resolved = object
                    .get("@catalogId")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| default.clone());
                for key in ["metadata", "accessibility", "checks"] {
                    let declared = declares(resolved.as_deref(), key);
                    legacy(object, key, declared, report);
                }
            }
        }
        message
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
        // The version first: a message for another protocol version is not
        // worth reading further, and this check needs no catalog.
        match raw.get("version") {
            Some(Value::String(version)) if version == crate::PROTOCOL_VERSION => {}
            None => {
                report.errors.push(problem(
                    Code::ValidationFailed,
                    None,
                    prefix.to_string(),
                    "Missing required version field: every message needs \"version\": \"1.0\"",
                ));
                return;
            }
            Some(version) => {
                let shown = version
                    .as_str()
                    .map_or_else(|| version.to_string(), String::from);
                report.errors.push(problem(
                    Code::ValidationFailed,
                    None,
                    format!("{prefix}/version"),
                    format!(
                        "Unsupported protocol version '{shown}': this renderer speaks A2UI 1.0, so set \"version\": \"1.0\""
                    ),
                ));
                return;
            }
        }
        let present: Vec<&str> = KINDS
            .iter()
            .copied()
            .filter(|kind| raw.get(*kind).is_some())
            .collect();
        let [kind] = present.as_slice() else {
            let message = if let Some(old) = LEGACY_KINDS.iter().find(|k| raw.get(**k).is_some()) {
                format!(
                    "action '{old}' is not supported in protocol version 1.0 (it is from A2UI 0.8 or 0.9); use one of {}",
                    KINDS.join(", ")
                )
            } else if present.is_empty() {
                format!("a message must hold exactly one of {}", KINDS.join(", "))
            } else {
                format!(
                    "Message contains multiple conflicting update actions ({}); send each in a message of its own",
                    present.join(" and ")
                )
            };
            report.errors.push(problem(
                Code::ValidationFailed,
                None,
                prefix.to_string(),
                message,
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
                            Code::CatalogNotFound,
                            surface_id,
                            format!("{prefix}/createSurface/catalogId"),
                            self.not_found(id),
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
            // Deleting a surface that does not exist changes nothing, as in the
            // reference SDKs; the model hears about it as a warning.
            "deleteSurface"
                if surface_id.is_some_and(|id| !state.contains_key(id))
                    && body.as_object().is_some_and(|b| b.len() == 1) =>
            {
                let id = surface_id.unwrap_or_default();
                report.warnings.push(problem(
                    Code::IntegrityError,
                    Some(id),
                    format!("{prefix}/deleteSurface/surfaceId"),
                    format!("surface {id:?} does not exist, so there was nothing to delete"),
                ));
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
                    Code::CatalogNotFound,
                    surface,
                    at.clone(),
                    format!(
                        "this component has no catalog: surface {:?} has no default catalogId, so set @catalogId on the component or catalogId on createSurface",
                        surface.unwrap_or_default()
                    ),
                )),
                Some(id) if self.entry(id).is_none() => {
                    // An unsupported surface default is reported once, on the surface.
                    if own.is_some() {
                        report.errors.push(problem(
                            Code::CatalogNotFound,
                            surface,
                            format!("{at}/@catalogId"),
                            self.not_found(id),
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
                            "Component id '{id}' must be a valid UAX #31 identifier: start with a letter or _, then letters, digits, or _"
                        ),
                    ));
                }
                if let Some(first) = ids.insert(id.to_string(), index) {
                    report.errors.push(problem(
                        Code::IntegrityError,
                        surface,
                        format!("{at}/id"),
                        format!("Duplicate component ID '{id}': it is used twice in this message (also at index {first}); give each component its own id"),
                    ));
                }
            }
            self.check_functions(component, default, &at, surface, report);
        }
        // A call resolves to its own catalogId, else the surface's default, which
        // need not be its component's catalog. The component's schema knows only
        // its own catalog's functions, so such a call is checked against the
        // catalog it resolves to, and a binding stands in for it in the
        // component's check.
        let mut checked: Vec<Value> = Vec::with_capacity(components.len());
        let mut detached = Vec::new();
        for (index, component) in components.iter().enumerate() {
            let at = format!("{prefix}/{kind}/components/{index}");
            let own = component
                .get("@catalogId")
                .and_then(Value::as_str)
                .or(default);
            checked.push(match own.filter(|id| self.entry(id).is_some()) {
                Some(context) => self.detach(
                    component,
                    context,
                    default,
                    &at,
                    None,
                    surface,
                    report,
                    &mut detached,
                ),
                None => component.clone(),
            });
        }
        let mut seen = BTreeSet::new();
        for (catalog_id, indices) in groups {
            let entry = self
                .entry(catalog_id)
                .expect("grouped by supported catalogs");
            let mut part = raw.clone();
            part[kind]["components"] =
                Value::Array(indices.iter().map(|&i| checked[i].clone()).collect());
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
        // Then every call against the catalog it resolves to, as the reference
        // SDKs check a call wherever it sits (a component's schema need not
        // type the place, a list of objects say). What the component's check
        // already said about a call is not said twice.
        for (path, call, catalog_id) in detached {
            let Some(entry) = self.entry(&catalog_id) else {
                continue;
            };
            let covers = |p: &Problem| {
                p.path.starts_with(&path) || (!p.path.is_empty() && path.starts_with(&p.path))
            };
            // A generic "matches none of the forms" over the call (with the
            // unevaluated `args` a failed `oneOf` leaves behind) says less than
            // the call's own check, which then replaces it.
            let generic = |p: &Problem| {
                p.message.starts_with("matches none of the forms")
                    || p.message
                        .starts_with("value is not valid under any of the schemas")
                    || p.message
                        .starts_with("Unevaluated properties are not allowed ('args'")
            };
            let covering: Vec<&Problem> = report.errors.iter().filter(|p| covers(p)).collect();
            if !covering.is_empty() && !covering.iter().all(|p| generic(p)) {
                continue;
            }
            let mut call = call;
            if let Some(object) = call.as_object_mut() {
                object.remove("catalogId");
            }
            if let Err(errors) = entry.schemas.check_function_call(&call) {
                report.errors.retain(|p| !(covers(p) && generic(p)));
                for error in errors {
                    report.errors.push(problem(
                        Code::ValidationFailed,
                        surface,
                        format!("{path}{}", error.path),
                        error.message,
                    ));
                }
            }
        }
    }

    /// "Catalog not found", with the catalogs this renderer has.
    fn not_found(&self, id: &str) -> String {
        format!(
            "Catalog not found: {id}; this renderer supports: {}",
            self.supported_catalog_ids().join(", ")
        )
    }

    /// `value` with each function call that resolves to a catalog other than
    /// `context` (its own `catalogId`, else the surface's `default`) replaced
    /// by a binding, which every dynamic value accepts, and the call pushed to
    /// `out` with its path and catalog, its own arguments treated the same way
    /// against that catalog. A call that resolves to no catalog the renderer
    /// has is reported here. System functions (`@index`) belong to no catalog,
    /// and an action's `functionCall`, where a binding cannot stand, is left to
    /// the component's check.
    #[allow(clippy::too_many_arguments)]
    fn detach(
        &self,
        value: &Value,
        context: &str,
        default: Option<&str>,
        at: &str,
        parent: Option<&str>,
        surface: Option<&str>,
        report: &mut Report,
        out: &mut Vec<(String, Value, String)>,
    ) -> Value {
        let descend = |value: &Value,
                       context: &str,
                       report: &mut Report,
                       out: &mut Vec<(String, Value, String)>|
         -> Value {
            match value {
                Value::Object(map) => Value::Object(
                    map.iter()
                        .map(|(key, item)| {
                            let path = format!("{at}/{}", segment(key));
                            let item = self.detach(
                                item,
                                context,
                                default,
                                &path,
                                Some(key),
                                surface,
                                report,
                                out,
                            );
                            (key.clone(), item)
                        })
                        .collect(),
                ),
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            let path = format!("{at}/{index}");
                            self.detach(item, context, default, &path, parent, surface, report, out)
                        })
                        .collect(),
                ),
                other => other.clone(),
            }
        };
        let Some(name) = value.get("@call").and_then(Value::as_str) else {
            return descend(value, context, report, out);
        };
        if name.starts_with('@') || parent == Some("functionCall") {
            return descend(value, context, report, out);
        }
        let placeholder = serde_json::json!({"@path": "/"});
        let named = value.get("catalogId").and_then(Value::as_str);
        match named.or(default) {
            None => {
                report.errors.push(problem(
                    Code::CatalogNotFound,
                    surface,
                    at.to_string(),
                    format!(
                        "{name} is called with no catalogId, and surface {:?} has no default catalogId; give the call a catalogId",
                        surface.unwrap_or_default()
                    ),
                ));
                placeholder
            }
            Some(id) if self.entry(id).is_none() => {
                report.errors.push(problem(
                    Code::CatalogNotFound,
                    surface,
                    format!("{at}/catalogId"),
                    self.not_found(id),
                ));
                placeholder
            }
            Some(id) if id == context => {
                let call = descend(value, context, report, out);
                out.push((at.to_string(), call.clone(), id.to_string()));
                call
            }
            Some(id) => {
                let call = descend(value, id, report, out);
                out.push((at.to_string(), call, id.to_string()));
                placeholder
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
            // System functions (`@index`) are named by the specification.
            if !name.starts_with('@') && !is_identifier(name) {
                report.errors.push(problem(
                    Code::ValidationFailed,
                    surface,
                    format!("{path}/@call"),
                    format!(
                        "Function name '{name}' must be a valid UAX #31 identifier: start with a letter or _, then letters, digits, or _"
                    ),
                ));
                continue;
            }
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
    /// composition constraints, cycles, and, as errors or warnings by
    /// [`Rules`], missing children, a missing root, and components nothing
    /// shows.
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
            for (child, field) in &children {
                match surface.components.get(child) {
                    None if self.rules.allow_dangling_references => report.warnings.push(problem(
                        Code::ValidationFailed,
                        Some(surface_id),
                        at(id),
                        format!(
                            "Dangling reference: {id} references component '{child}' in field '{field}', which is not defined yet; it shows as a placeholder until it is"
                        ),
                    )),
                    None => report.errors.push(problem(
                        Code::ValidationFailed,
                        Some(surface_id),
                        at(id),
                        format!(
                            "Dangling reference: {id} references non-existent component '{child}' in field '{field}'; define it in the same call or remove the reference"
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
            edges.insert(
                id.as_str(),
                children.into_iter().map(|(child, _)| child).collect(),
            );
        }

        if let Some(cycle) = find_cycle(&edges) {
            let message = if cycle.len() == 2 {
                format!("Self-reference detected: {} references itself", cycle[0])
            } else {
                format!("Circular reference detected: {}", cycle.join(" -> "))
            };
            report.errors.push(problem(
                Code::RecursionLimitExceeded,
                Some(surface_id),
                at(&cycle[0]),
                message,
            ));
            return;
        }
        let Some(root) = surface.components.get("root") else {
            if !surface.components.is_empty() {
                let (list, verdict) = if self.rules.allow_missing_root {
                    (&mut report.warnings, "so nothing is shown yet")
                } else {
                    (&mut report.errors, "so nothing can be shown; add one")
                };
                list.push(problem(
                    Code::ValidationFailed,
                    Some(surface_id),
                    String::new(),
                    format!(
                        "Missing root component: surface {surface_id:?} has components but none with id \"root\", {verdict}"
                    ),
                ));
            }
            return;
        };
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
        let depth = longest_chain(&edges, "root", &mut BTreeMap::new());
        if depth > MAX_DEPTH {
            report.errors.push(problem(
                Code::RecursionLimitExceeded,
                Some(surface_id),
                String::new(),
                format!(
                    "Global recursion limit exceeded: logical depth > {MAX_DEPTH} (components nest {depth} deep below root); flatten the layout"
                ),
            ));
            return;
        }
        let reachable = reach(&edges, "root");
        for id in surface
            .components
            .keys()
            .filter(|id| !reachable.contains(id.as_str()))
        {
            if self.rules.allow_orphans {
                report.warnings.push(problem(
                    Code::ValidationFailed,
                    Some(surface_id),
                    at(id),
                    format!("Component '{id}' is not reachable from 'root', so it is not shown"),
                ));
            } else {
                report.errors.push(problem(
                    Code::ValidationFailed,
                    Some(surface_id),
                    at(id),
                    format!(
                        "Component '{id}' is not reachable from 'root', so it would never be shown; make it a child of a shown component or remove it"
                    ),
                ));
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
/// A component's child references, each with the field it is in, written
/// as the reference SDKs write it (`children[1]`, `tabs[0].child`,
/// `children.componentId` for a template).
fn child_ids(component: &Value, def: &ComponentDef) -> Vec<(String, String)> {
    let mut ids = Vec::new();
    for (property, kind) in &def.children {
        let Some(value) = component.get(property) else {
            continue;
        };
        match (kind, value) {
            (ChildRef::Single, Value::String(id)) => ids.push((id.clone(), property.clone())),
            (ChildRef::List, Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    if let Some(id) = item.as_str() {
                        ids.push((id.to_string(), format!("{property}[{index}]")));
                    }
                }
            }
            (ChildRef::List, Value::Object(template)) => {
                if let Some(id) = template.get("componentId").and_then(Value::as_str) {
                    ids.push((id.to_string(), format!("{property}.componentId")));
                }
            }
            (ChildRef::Items(field), Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    if let Some(id) = item.get(field).and_then(Value::as_str) {
                        ids.push((id.to_string(), format!("{property}[{index}].{field}")));
                    }
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

/// How many levels of children lie below `node` at most (0 for a leaf),
/// memoized; the graph has no cycles by the time this runs.
fn longest_chain(
    edges: &BTreeMap<&str, Vec<String>>,
    node: &str,
    memo: &mut BTreeMap<String, usize>,
) -> usize {
    if let Some(depth) = memo.get(node) {
        return *depth;
    }
    let depth = edges
        .get(node)
        .into_iter()
        .flatten()
        .filter(|child| edges.contains_key(child.as_str()))
        .map(|child| 1 + longest_chain(edges, child, memo))
        .max()
        .unwrap_or(0);
    memo.insert(node.to_string(), depth);
    depth
}

/// The reference SDKs' limits and path syntax over a raw message: values
/// nested more than [`MAX_DEPTH`] deep, function calls (an object with `@call`
/// and `args`) nested more than [`MAX_CALL_DEPTH`] deep in each other's
/// arguments, and `@path` or `path` strings that are not JSON Pointers (relative
/// or absolute; a `~` must be `~0` or `~1`). `depth` counts the message itself
/// as 1, as the SDKs count it inside the batch.
fn limits_and_paths(value: &Value, at: &str, depth: usize, calls: usize) -> Result<(), Problem> {
    if depth > MAX_DEPTH {
        return Err(problem(
            Code::RecursionLimitExceeded,
            None,
            at.to_string(),
            format!(
                "Global recursion limit exceeded: Depth > {MAX_DEPTH}; nest values less deeply"
            ),
        ));
    }
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                limits_and_paths(item, &format!("{at}/{index}"), depth + 1, calls)?;
            }
        }
        Value::Object(map) => {
            for key in ["path", "@path"] {
                if let Some(Value::String(path)) = map.get(key) {
                    if !pointer_syntax(path) {
                        return Err(problem(
                            Code::ValidationFailed,
                            None,
                            format!("{at}/{}", segment(key)),
                            format!(
                                "Invalid path syntax: '{path}' is not a JSON Pointer (a ~ must be written ~0, and a / inside a key ~1)"
                            ),
                        ));
                    }
                }
            }
            let is_call = map.contains_key("@call") && map.contains_key("args");
            if is_call && calls >= MAX_CALL_DEPTH {
                return Err(problem(
                    Code::RecursionLimitExceeded,
                    None,
                    at.to_string(),
                    format!(
                        "Recursion limit exceeded: functionCall depth > {MAX_CALL_DEPTH}; nest fewer function calls"
                    ),
                ));
            }
            for (key, item) in map {
                let nested = if is_call && key == "args" {
                    calls + 1
                } else {
                    calls
                };
                limits_and_paths(item, &format!("{at}/{}", segment(key)), depth + 1, nested)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether `path` is a JSON Pointer, absolute or relative: every `~` escapes
/// `0` or `1`.
fn pointer_syntax(path: &str) -> bool {
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
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
