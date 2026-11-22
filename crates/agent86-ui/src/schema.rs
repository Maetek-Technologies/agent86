//! The A2UI 1.0 JSON Schemas, vendored from the specification (`spec/`), and the
//! compiled validators built from them.
//!
//! A2UI's envelope schema is catalog-agnostic: a component is checked against
//! `catalog.json#/$defs/anyComponent`, and `catalog.json` is a placeholder the
//! validator maps to whichever catalog is in use (the specification's "Swappable
//! Catalogs & Validation"). [`SchemaSet::for_catalog`] does that mapping, the same
//! way the specification's own test runner does: the catalog document is served
//! under the placeholder's URI with its `$id` rewritten to match, so its relative
//! `common_types.json` references resolve to the protocol's common types.
//!
//! Nothing is fetched. Every document a reference can reach is served from memory
//! by [`MemoryDocs`]; a reference to anything else fails to compile.

use crate::catalog::Catalog;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

/// The base URI the 1.0 schemas' `$id`s share.
pub const SPEC_BASE: &str = "https://a2ui.org/specification/v1_0/";

const COMMON_TYPES: &str = include_str!("../spec/v1_0/json/common_types.json");
const AGENT_TO_RENDERER: &str = include_str!("../spec/v1_0/json/agent_to_renderer.json");
const RENDERER_TO_AGENT: &str = include_str!("../spec/v1_0/json/renderer_to_agent.json");
const CATALOG_DEFINITION: &str = include_str!("../spec/v1_0/json/catalog_definition.json");
const AGENT_CAPABILITIES: &str = include_str!("../spec/v1_0/json/agent_capabilities.json");
const RENDERER_CAPABILITIES: &str = include_str!("../spec/v1_0/json/renderer_capabilities.json");
const RENDERER_DATA_MODEL: &str = include_str!("../spec/v1_0/json/renderer_data_model.json");

/// One failure from a schema check: where in the checked value, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    /// A JSON Pointer into the checked value (`""` for the value itself).
    pub path: String,
    /// The validator's description of the failure.
    pub message: String,
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Why a schema could not be compiled: a catalog that refers outside the
/// documents served, or that is not a valid schema.
#[derive(Debug, Clone, thiserror::Error)]
#[error("schema does not compile: {0}")]
pub struct CompileError(pub String);

fn parse(text: &str) -> Value {
    // The vendored schemas are checked by this crate's tests; a parse failure is a
    // packaging fault, not an input error.
    serde_json::from_str(text).expect("vendored A2UI schema is valid JSON")
}

/// The vendored schema documents, parsed once.
struct Docs {
    common_types: Value,
    agent_to_renderer: Value,
    renderer_to_agent: Value,
    catalog_definition: Value,
    agent_capabilities: Value,
    renderer_capabilities: Value,
    renderer_data_model: Value,
}

fn docs() -> &'static Docs {
    static DOCS: OnceLock<Docs> = OnceLock::new();
    DOCS.get_or_init(|| Docs {
        common_types: parse(COMMON_TYPES),
        agent_to_renderer: parse(AGENT_TO_RENDERER),
        renderer_to_agent: parse(RENDERER_TO_AGENT),
        catalog_definition: parse(CATALOG_DEFINITION),
        agent_capabilities: parse(AGENT_CAPABILITIES),
        renderer_capabilities: parse(RENDERER_CAPABILITIES),
        renderer_data_model: parse(RENDERER_DATA_MODEL),
    })
}

/// Serves schema documents from memory, by URI. The only retriever the
/// validators ever get, so a reference outside these documents cannot reach the
/// network or the file system.
struct MemoryDocs(HashMap<String, Value>);

impl jsonschema::Retrieve for MemoryDocs {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let key = uri.as_str();
        let key = key.split('#').next().unwrap_or(key);
        self.0
            .get(key)
            .cloned()
            .ok_or_else(|| format!("no A2UI schema is served at {key}").into())
    }
}

fn uri(name: &str) -> String {
    format!("{SPEC_BASE}{name}")
}

/// The documents a validator may reach: the common types, the message schemas,
/// the catalog definition, and `catalog.json`. `common_types.json` itself refers
/// to `catalog.json` (a `FunctionCall` is checked against the catalog's
/// `anyFunction`), so the schemas that take no catalog (the catalog definition,
/// the capabilities) are given a stand-in that matches nothing; they never reach
/// it, since none of them checks a function call.
fn memory(catalog: Option<&Value>) -> MemoryDocs {
    let d = docs();
    let catalog = catalog.cloned().unwrap_or_else(|| {
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": uri("catalog.json"),
            "$defs": {"anyComponent": false, "anyFunction": false}
        })
    });
    MemoryDocs(HashMap::from([
        (uri("common_types.json"), d.common_types.clone()),
        (uri("agent_to_renderer.json"), d.agent_to_renderer.clone()),
        (uri("renderer_to_agent.json"), d.renderer_to_agent.clone()),
        (uri("catalog_definition.json"), d.catalog_definition.clone()),
        (uri("catalog.json"), catalog),
    ]))
}

fn compile(schema: &Value, catalog: Option<&Value>) -> Result<jsonschema::Validator, CompileError> {
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        // The specification's runner validates with ajv-formats, so `format`
        // (the action's `date-time` timestamp) is asserted, not just annotated.
        .should_validate_formats(true)
        .with_retriever(memory(catalog))
        .build(schema)
        .map_err(|error| CompileError(error.to_string()))
}

fn errors(validator: &jsonschema::Validator, value: &Value) -> Result<(), Vec<SchemaError>> {
    let mut found = Vec::new();
    let mut unevaluated = Vec::new();
    for error in validator.iter_errors(value) {
        if matches!(
            error.kind(),
            jsonschema::error::ValidationErrorKind::UnevaluatedProperties { .. }
        ) {
            unevaluated.push(SchemaError {
                path: error.instance_path().to_string(),
                message: render(&error),
            });
        } else {
            describe(&error, &mut found);
        }
    }
    // When part of an object fails, the properties that part would have
    // evaluated are reported as unexpected too, valid or not ("label" and
    // "value" of a TextField whose check is wrong). That complaint only stands
    // when nothing else failed inside the same object.
    for error in unevaluated {
        let inside = |other: &SchemaError| {
            other.path == error.path || other.path.starts_with(&format!("{}/", error.path))
        };
        if !found.iter().any(inside) {
            found.push(error);
        }
    }
    let mut seen = std::collections::HashSet::new();
    found.retain(|error| seen.insert((error.path.clone(), error.message.clone())));
    if found.is_empty() {
        Ok(())
    } else {
        Err(found)
    }
}

/// The longest instance a message quotes; a longer one is written as `value`.
const QUOTE_LIMIT: usize = 60;

fn render(error: &jsonschema::ValidationError<'_>) -> String {
    use jsonschema::error::ValidationErrorKind;
    let short = error.instance().to_string().chars().count() <= QUOTE_LIMIT;
    let text = if short {
        error.to_string()
    } else {
        error.masked().to_string()
    };
    // The field a value sits in, when the path ends in a property name, so
    // the message says which one is wrong.
    let path = error.instance_path().to_string();
    let field = path
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty() && segment.parse::<usize>().is_err());
    match (error.kind(), field) {
        (ValidationErrorKind::Type { .. }, Some(field)) => {
            match text.split_once(" is not of type ") {
                Some((value, expected)) => {
                    let expected = expected.trim_matches('"');
                    let article = if expected.starts_with(['a', 'e', 'i', 'o', 'u']) {
                        "an"
                    } else {
                        "a"
                    };
                    format!("{field} must be {article} {expected}, not {value}")
                }
                None => format!("{field}: {text}"),
            }
        }
        (ValidationErrorKind::Enum { .. }, Some(field)) => format!("{field}: {text}"),
        // A component missing a property: name its type.
        (ValidationErrorKind::Required { .. }, _) => {
            match error.instance().get("component").and_then(Value::as_str) {
                Some(component) => format!("{text} of {component}"),
                None => text,
            }
        }
        _ => text,
    }
}

/// Whether an error shows a value plainly is not a `oneOf` branch's form:
/// - a discriminator mismatch, the `const` on a component's `component` or a
///   function call's `@call` (how a `oneOf` over a catalog's components or
///   functions rules out the other types);
/// - at the `oneOf`'s own location, a type mismatch (an object is not the
///   literal-string branch), a missing `@` key (a `{"@call": ...}` is not the
///   `{"@path": ...}` branch), or a missing `event` or `functionCall` (an
///   action's two forms).
fn rules_out(error: &jsonschema::ValidationError<'_>, at: &str) -> bool {
    use jsonschema::error::ValidationErrorKind;
    let path = error.instance_path().to_string();
    match error.kind() {
        ValidationErrorKind::Constant { .. } => {
            path.ends_with("/component") || path.ends_with("/@call")
        }
        ValidationErrorKind::Type { .. } => path == at,
        ValidationErrorKind::Required { property } => {
            path == at
                && property
                    .as_str()
                    .is_some_and(|name| name.starts_with('@') || ACTION_FORMS.contains(&name))
        }
        _ => false,
    }
}

/// The keys that tell an action's two forms apart.
const ACTION_FORMS: &[&str] = &["event", "functionCall"];

/// A value whose discriminator (`component`, `@call`) names no branch of a
/// `oneOf`: the name it gave, and the names the catalog defines, so the model
/// can pick one.
fn unknown_type(context: &[Vec<jsonschema::ValidationError<'static>>]) -> String {
    use jsonschema::error::ValidationErrorKind;
    let mut given = None;
    let mut key = "component";
    let mut known = BTreeSet::new();
    for inner in context.iter().flatten() {
        if let ValidationErrorKind::Constant { expected_value } = inner.kind() {
            let path = inner.instance_path().to_string();
            if path.ends_with("/@call") {
                key = "@call";
            }
            given.get_or_insert_with(|| inner.instance().to_string());
            if let Some(name) = expected_value.as_str() {
                known.insert(name.to_string());
            }
        }
    }
    let given = given.unwrap_or_default();
    let given = given.trim_matches('"');
    let known = known.into_iter().collect::<Vec<_>>().join(", ");
    if key == "@call" {
        format!("Unrecognized function '{given}': the catalog defines no such function; use one of: {known}")
    } else {
        format!("Unknown component type '{given}': the catalog defines no such component; use one of: {known}")
    }
}

/// Turn a validator error into what a model can act on. A failed `oneOf` or
/// `anyOf` says only that nothing matched, quoting the whole value. When every
/// branch but one is plainly not the value's form ([`rules_out`]: a Text checked
/// against Button's schema, a function call against the binding form), the one
/// left is what the value meant to be, so its errors are reported instead.
/// Otherwise the error lists each candidate branch's first complaint, which
/// names the forms the field allows.
fn describe(error: &jsonschema::ValidationError<'_>, out: &mut Vec<SchemaError>) {
    use jsonschema::error::ValidationErrorKind;
    let path = error.instance_path().to_string();
    match error.kind() {
        ValidationErrorKind::OneOfNotValid { context } | ValidationErrorKind::AnyOf { context } => {
            let mut meant: Vec<&Vec<jsonschema::ValidationError<'static>>> = context
                .iter()
                .filter(|branch| !branch.iter().any(|inner| rules_out(inner, &path)))
                .collect();
            if let [branch] = meant.as_slice() {
                for inner in branch.iter() {
                    describe(inner, out);
                }
                return;
            }
            if meant.is_empty() {
                if context.iter().all(|branch| {
                    branch.iter().any(|inner| {
                        matches!(inner.kind(), ValidationErrorKind::Constant { .. })
                            && rules_out(inner, &path)
                    })
                }) {
                    out.push(SchemaError {
                        path,
                        message: unknown_type(context),
                    });
                    return;
                }
                meant = context.iter().collect();
            }
            let forms: Vec<String> = meant
                .iter()
                .filter_map(|branch| branch.first())
                .map(|inner| inner.masked().to_string())
                .collect();
            out.push(SchemaError {
                path,
                message: format!(
                    "matches none of the forms allowed here ({})",
                    forms.join("; ")
                ),
            });
        }
        _ => out.push(SchemaError {
            path,
            message: render(error),
        }),
    }
}

/// A catalog document as the envelope schema expects to find it: its `$id`
/// set to the `catalog.json` placeholder, so the references inside it
/// (`common_types.json#/...` and its own `#/components/...`) resolve.
pub(crate) fn aliased(catalog: &Value) -> Value {
    let mut catalog = catalog.clone();
    if let Some(object) = catalog.as_object_mut() {
        object.insert("$id".into(), Value::String(uri("catalog.json")));
    }
    catalog
}

/// The agent-to-renderer message kinds and their definitions in
/// `agent_to_renderer.json`.
const MESSAGE_DEFS: &[(&str, &str)] = &[
    ("createSurface", "CreateSurfaceMessage"),
    ("updateComponents", "UpdateComponentsMessage"),
    ("updateDataModel", "UpdateDataModelMessage"),
    ("deleteSurface", "DeleteSurfaceMessage"),
    ("callRendererFunction", "CallRendererFunctionMessage"),
    ("agentFunctionResponse", "AgentFunctionResponseMessage"),
];

/// The message schemas compiled against one catalog.
pub struct SchemaSet {
    agent_to_renderer: jsonschema::Validator,
    renderer_to_agent: jsonschema::Validator,
    /// One validator per message kind: the same definitions the envelope's
    /// `oneOf` chooses between, so a message checked against its own kind's
    /// gets that kind's errors rather than "matches none of the kinds".
    kinds: Vec<(&'static str, jsonschema::Validator)>,
}

impl std::fmt::Debug for SchemaSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaSet").finish_non_exhaustive()
    }
}

impl SchemaSet {
    /// Compile `agent_to_renderer.json` and `renderer_to_agent.json` with
    /// `catalog.json` mapped to `catalog`.
    pub fn for_catalog(catalog: &Catalog) -> Result<Self, CompileError> {
        Self::for_document(catalog.document())
    }

    /// As [`SchemaSet::for_catalog`], from a catalog's JSON document as it is,
    /// unchecked. For the specification's tests, which pair messages with
    /// catalogs; use [`SchemaSet::for_catalog`] otherwise.
    pub fn for_document(catalog: &Value) -> Result<Self, CompileError> {
        let catalog = aliased(catalog);
        let d = docs();
        let kinds = MESSAGE_DEFS
            .iter()
            .map(|(kind, def)| {
                let schema = serde_json::json!({
                    "$ref": format!("{}#/$defs/{def}", uri("agent_to_renderer.json"))
                });
                Ok((*kind, compile(&schema, Some(&catalog))?))
            })
            .collect::<Result<_, CompileError>>()?;
        Ok(Self {
            agent_to_renderer: compile(&d.agent_to_renderer, Some(&catalog))?,
            renderer_to_agent: compile(&d.renderer_to_agent, Some(&catalog))?,
            kinds,
        })
    }

    /// Check a message against one kind's definition (`"createSurface"`, ...).
    /// Valid exactly when [`SchemaSet::check_agent_message`] is for a message
    /// holding that kind's key, with errors about that kind only.
    pub fn check_agent_message_as(
        &self,
        kind: &str,
        message: &Value,
    ) -> Result<(), Vec<SchemaError>> {
        match self.kinds.iter().find(|(name, _)| *name == kind) {
            Some((_, validator)) => errors(validator, message),
            None => self.check_agent_message(message),
        }
    }

    /// Check one agent-to-renderer message against the envelope schema and the
    /// catalog. This is the schema half of validation only: whether a surface
    /// exists, whether `root` is there, and the composition rules are checked by
    /// [`crate::Validator`].
    pub fn check_agent_message(&self, message: &Value) -> Result<(), Vec<SchemaError>> {
        errors(&self.agent_to_renderer, message)
    }

    /// Check one renderer-to-agent message (an action, a function call or
    /// response, an error).
    pub fn check_renderer_message(&self, message: &Value) -> Result<(), Vec<SchemaError>> {
        errors(&self.renderer_to_agent, message)
    }
}

/// The schemas that need no catalog, compiled once.
struct Fixed {
    catalog_definition: jsonschema::Validator,
    agent_capabilities: jsonschema::Validator,
    renderer_capabilities: jsonschema::Validator,
    renderer_data_model: jsonschema::Validator,
}

fn fixed() -> &'static Fixed {
    static FIXED: OnceLock<Fixed> = OnceLock::new();
    FIXED.get_or_init(|| {
        let d = docs();
        let build = |schema: &Value| compile(schema, None).expect("vendored A2UI schema compiles");
        Fixed {
            catalog_definition: build(&d.catalog_definition),
            agent_capabilities: build(&d.agent_capabilities),
            renderer_capabilities: build(&d.renderer_capabilities),
            renderer_data_model: build(&d.renderer_data_model),
        }
    })
}

/// Check a catalog document against `catalog_definition.json`.
pub fn check_catalog_definition(catalog: &Value) -> Result<(), Vec<SchemaError>> {
    errors(&fixed().catalog_definition, catalog)
}

/// Check an agent capabilities object (`{"1.0": {...}}`).
pub fn check_agent_capabilities(value: &Value) -> Result<(), Vec<SchemaError>> {
    errors(&fixed().agent_capabilities, value)
}

/// Check a renderer capabilities object (`{"1.0": {"supportedCatalogIds": [...]}}`).
pub fn check_renderer_capabilities(value: &Value) -> Result<(), Vec<SchemaError>> {
    errors(&fixed().renderer_capabilities, value)
}

/// Check a renderer data model message (`{"version": "1.0", "surfaces": {...}}`).
pub fn check_renderer_data_model(value: &Value) -> Result<(), Vec<SchemaError>> {
    errors(&fixed().renderer_data_model, value)
}
