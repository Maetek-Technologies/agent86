//! Catalogs: the components and functions a renderer draws and runs, which are
//! the only ones an agent may use. A catalog is a JSON Schema document
//! (`catalog_definition.json`); [`Catalog`] holds it with what validation needs
//! read out of it: each component's child-reference properties and composition
//! constraints, and each function's return type and allowed callers.

use crate::ident::is_identifier;
use crate::schema::{check_catalog_definition, CompileError, SchemaError, SchemaSet};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The basic catalog's id, as the specification publishes it.
pub const BASIC_CATALOG_ID: &str =
    "https://a2ui.org/specification/v1_0/catalogs/basic/catalog.json";

const BASIC_CATALOG: &str = include_str!("../spec/v1_0/catalogs/basic/catalog.json");

/// The schemas in `common_types.json` a catalog's external `$ref` may target
/// (the specification's catalog schema rule 3, "Restricted `$ref` Targets").
pub const ALLOWED_EXTERNAL_REF_TARGETS: &[&str] = &[
    "ComponentId",
    "Child",
    "ChildList",
    "DynamicString",
    "DynamicNumber",
    "DynamicBoolean",
    "DynamicStringList",
    "DynamicValue",
    "AccessibilityAttributes",
    "CheckRule",
    "Checkable",
    "Action",
    "DataBinding",
    "FunctionCall",
];

/// How a component property refers to other components, read from its schema's
/// `$ref`, which is how the specification says validators find structural links
/// ("Validator compliance when defining catalogs").
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChildRef {
    /// One component id (`ComponentId` or `Child`).
    Single,
    /// A `ChildList`: an array of ids, or a template (`componentId` and `path`).
    List,
    /// An array of objects each holding one component id in this field (the
    /// basic catalog's `Tabs.tabs[].child`).
    Items(String),
}

/// What validation needs to know about one component type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentDef {
    /// The properties that hold child references, by name.
    pub children: BTreeMap<String, ChildRef>,
    /// The parent types this component may appear under (`None`: any).
    pub allowed_parents: Option<Vec<String>>,
    /// The child types this component may contain (`None`: any).
    pub allowed_children: Option<Vec<String>>,
    /// The properties that hold an `Action`, which a renderer runs on a user
    /// gesture rather than resolving with the other properties.
    pub actions: std::collections::BTreeSet<String>,
}

/// Who may call a function (`allowedCallers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callers {
    /// Only the renderer, from bindings, checks, and actions (the default).
    RendererOnly,
    /// Only the agent, through `callRendererFunction`.
    AgentOnly,
    /// Either.
    RendererOrAgent,
}

/// What validation needs to know about one function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    /// The declared `returnType` (`string`, `validationResult`, `void`, ...).
    pub return_type: String,
    /// The declared `allowedCallers`.
    pub callers: Callers,
    /// Whether it may only run from a user gesture (`requiresUserActivation`).
    pub requires_user_activation: bool,
}

/// Why a catalog was refused when loaded.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// The document does not match `catalog_definition.json`.
    #[error("catalog does not match catalog_definition.json: {}", join(.0))]
    Schema(Vec<SchemaError>),
    /// A rule the schema cannot express: the protocol version, a name, a
    /// `$ref` target, a component's discriminator.
    #[error("invalid catalog: {0}")]
    Rule(String),
    /// The catalog does not compile as the envelope schema's `catalog.json`.
    #[error(transparent)]
    Compile(#[from] CompileError),
}

fn join(errors: &[SchemaError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// A loaded, checked catalog.
#[derive(Debug, Clone)]
pub struct Catalog {
    id: String,
    document: Value,
    components: BTreeMap<String, ComponentDef>,
    functions: BTreeMap<String, FunctionDef>,
}

impl Catalog {
    /// The specification's basic catalog (`catalogs/basic/v1`), vendored.
    pub fn basic() -> &'static Catalog {
        static BASIC: OnceLock<Catalog> = OnceLock::new();
        BASIC.get_or_init(|| {
            let document = serde_json::from_str(BASIC_CATALOG).expect("basic catalog is JSON");
            Catalog::from_value(document).expect("the vendored basic catalog is valid")
        })
    }

    /// Load a catalog document, checking it against `catalog_definition.json`
    /// and the rules the schema cannot express: `protocolVersion` must be
    /// `1.0`; component, function, property, and `$defs` names must be UAX #31
    /// identifiers; every `$ref` must target this catalog's own components,
    /// functions, or leaf `$defs`, or one of [`ALLOWED_EXTERNAL_REF_TARGETS`];
    /// and each component must declare `component` as the constant of its own
    /// name. Nothing the document refers to is fetched.
    pub fn from_value(document: Value) -> Result<Catalog, CatalogError> {
        check_catalog_definition(&document).map_err(CatalogError::Schema)?;
        let object = document
            .as_object()
            .ok_or_else(|| CatalogError::Rule("a catalog is a JSON object".into()))?;
        match object.get("protocolVersion").and_then(Value::as_str) {
            Some("1.0") => {}
            Some(other) => {
                return Err(CatalogError::Rule(format!(
                    "protocolVersion is {other:?}; this implementation reads 1.0 catalogs only"
                )))
            }
            None => {
                return Err(CatalogError::Rule(
                    "protocolVersion is missing, which means 0.9; this implementation reads 1.0 catalogs only".into(),
                ))
            }
        }
        let id = object
            .get("catalogId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let empty = serde_json::Map::new();
        let component_docs = object
            .get("components")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let function_docs = object
            .get("functions")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let defs = object
            .get("$defs")
            .and_then(Value::as_object)
            .unwrap_or(&empty);

        let mut problems = Vec::new();
        for (name, schema) in component_docs {
            if !is_identifier(name) {
                problems.push(format!("component name {name:?} is not an identifier"));
            }
            if discriminator(schema) != Some(name.as_str()) {
                problems.push(format!(
                    "component {name:?} must declare \"component\": {{\"const\": {name:?}}}"
                ));
            }
            check_names(schema, &mut problems);
        }
        for (name, schema) in function_docs {
            let bare = name.strip_prefix('@').unwrap_or(name);
            if !is_identifier(bare) {
                problems.push(format!("function name {name:?} is not an identifier"));
            }
            check_names(schema, &mut problems);
        }
        for (name, schema) in defs {
            if name == "anyComponent" || name == "anyFunction" {
                continue;
            }
            if !is_identifier(name) {
                problems.push(format!("$defs name {name:?} is not an identifier"));
            }
            check_names(schema, &mut problems);
        }
        let mut refs = Vec::new();
        for section in [component_docs, function_docs, defs] {
            for schema in section.values() {
                collect_refs(schema, &mut refs);
            }
        }
        for target in refs {
            if let Some(problem) = ref_problem(&target, component_docs, function_docs, defs) {
                problems.push(problem);
            }
        }
        if !problems.is_empty() {
            return Err(CatalogError::Rule(problems.join("; ")));
        }

        let components = component_docs
            .iter()
            .map(|(name, schema)| (name.clone(), component_def(schema)))
            .collect();
        let functions = function_docs
            .iter()
            .map(|(name, schema)| (name.clone(), function_def(schema)))
            .collect();
        let catalog = Catalog {
            id,
            document,
            components,
            functions,
        };
        // Compiling proves every reference resolves within the served documents.
        SchemaSet::for_catalog(&catalog)?;
        Ok(catalog)
    }

    /// The catalog's id (`catalogId`).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The catalog's JSON Schema document, as loaded.
    pub fn document(&self) -> &Value {
        &self.document
    }

    /// The catalog's `instructions` for the model, if it has any.
    pub fn instructions(&self) -> Option<&str> {
        self.document.get("instructions").and_then(Value::as_str)
    }

    /// The component types, by name.
    pub fn components(&self) -> &BTreeMap<String, ComponentDef> {
        &self.components
    }

    /// The functions, by name.
    pub fn functions(&self) -> &BTreeMap<String, FunctionDef> {
        &self.functions
    }

    /// One component type, if the catalog has it.
    pub fn component(&self, name: &str) -> Option<&ComponentDef> {
        self.components.get(name)
    }

    /// One function, if the catalog has it.
    pub fn function(&self, name: &str) -> Option<&FunctionDef> {
        self.functions.get(name)
    }

    /// Whether this catalog's schema for component `type_name` declares a
    /// property `name`: in its own `properties`, or those of any `allOf`,
    /// `oneOf`, or `anyOf` branch, or of a `$defs` entry it refers to. A
    /// component may declare `catalogId`, `metadata`, `accessibility`, or
    /// `checks` as ordinary properties, which then are not the legacy spellings
    /// of the `@` envelope keys.
    pub fn declares(&self, type_name: &str, name: &str) -> bool {
        let Some(schema) = self.document.pointer(&format!(
            "/components/{}",
            type_name.replace('~', "~0").replace('/', "~1")
        )) else {
            return false;
        };
        declared(schema, name, &self.document, 0)
    }

    /// Whether `name` is component `type_name`'s own property here, not the
    /// envelope's: [`Catalog::declares`], except that a `catalogId` or
    /// `metadata` declared in a `properties` block that also declares `id`, in
    /// the standard shape (a string `catalogId`, or a `metadata` object whose
    /// only property is `extensions`), is the envelope's, as early 1.0 drafts
    /// inlined it.
    pub fn declares_own(&self, type_name: &str, name: &str) -> bool {
        let Some(schema) = self.document.pointer(&format!(
            "/components/{}",
            type_name.replace('~', "~0").replace('/', "~1")
        )) else {
            return false;
        };
        own(schema, name, &self.document, 0)
    }
}

/// A component schema's `properties.component.const`, at its root or in one of
/// its `allOf` entries.
fn discriminator(schema: &Value) -> Option<&str> {
    property_maps(schema)
        .into_iter()
        .find_map(|properties| properties.get("component")?.get("const")?.as_str())
}

/// The `properties` objects of a component schema: its own and those of its
/// `allOf` entries (where catalogs put local properties beside a `Checkable`).
fn property_maps(schema: &Value) -> Vec<&serde_json::Map<String, Value>> {
    let mut maps = Vec::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        maps.push(properties);
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        for entry in all {
            if let Some(properties) = entry.get("properties").and_then(Value::as_object) {
                maps.push(properties);
            }
        }
    }
    maps
}

fn child_ref(property: &Value) -> Option<ChildRef> {
    match property.get("$ref")?.as_str()? {
        "common_types.json#/$defs/ComponentId" | "common_types.json#/$defs/Child" => {
            Some(ChildRef::Single)
        }
        "common_types.json#/$defs/ChildList" => Some(ChildRef::List),
        _ => None,
    }
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect(),
    )
}

/// An array property of child references: its items each one id (read as a
/// [`ChildRef::List`] of ids), or objects with a child-reference field.
fn items_ref(property: &Value) -> Option<ChildRef> {
    let items = property.get("items")?;
    if child_ref(items) == Some(ChildRef::Single) {
        return Some(ChildRef::List);
    }
    let fields = items.get("properties")?.as_object()?;
    fields
        .iter()
        .find(|(_, field)| child_ref(field) == Some(ChildRef::Single))
        .map(|(name, _)| ChildRef::Items(name.clone()))
}

fn component_def(schema: &Value) -> ComponentDef {
    let mut children = BTreeMap::new();
    let mut actions = std::collections::BTreeSet::new();
    for properties in property_maps(schema) {
        for (name, property) in properties {
            if let Some(kind) = child_ref(property).or_else(|| items_ref(property)) {
                children.insert(name.clone(), kind);
            }
            if property.get("$ref").and_then(Value::as_str)
                == Some("common_types.json#/$defs/Action")
            {
                actions.insert(name.clone());
            }
        }
    }
    ComponentDef {
        children,
        allowed_parents: strings(schema.get("allowedParents")),
        allowed_children: strings(schema.get("allowedChildren")),
        actions,
    }
}

fn function_def(schema: &Value) -> FunctionDef {
    let callers = match schema.get("allowedCallers").and_then(Value::as_str) {
        Some("agentOnly") => Callers::AgentOnly,
        Some("rendererOrAgent") => Callers::RendererOrAgent,
        _ => Callers::RendererOnly,
    };
    FunctionDef {
        return_type: schema
            .get("returnType")
            .and_then(Value::as_str)
            .unwrap_or("any")
            .to_string(),
        callers,
        requires_user_activation: schema
            .get("requiresUserActivation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Keys whose values are annotations or literal data, not subschemas.
const NON_SCHEMA_KEYS: &[&str] = &[
    "metadata",
    "examples",
    "const",
    "default",
    "enum",
    "description",
];

/// Property names inside a schema must be identifiers too (except the
/// protocol's own `@path` and `@call`).
fn check_names(schema: &Value, problems: &mut Vec<String>) {
    match schema {
        Value::Object(object) => {
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                for (name, property) in properties {
                    if name != "@path" && name != "@call" && !is_identifier(name) {
                        problems.push(format!("property name {name:?} is not an identifier"));
                    }
                    check_names(property, problems);
                }
            }
            for (key, value) in object {
                if key != "properties" && !NON_SCHEMA_KEYS.contains(&key.as_str()) {
                    check_names(value, problems);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                check_names(item, problems);
            }
        }
        _ => {}
    }
}

fn collect_refs(schema: &Value, refs: &mut Vec<String>) {
    match schema {
        Value::Object(object) => {
            if let Some(target) = object.get("$ref").and_then(Value::as_str) {
                refs.push(target.to_string());
            }
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                for property in properties.values() {
                    collect_refs(property, refs);
                }
            }
            for (key, value) in object {
                if key != "properties" && !NON_SCHEMA_KEYS.contains(&key.as_str()) {
                    collect_refs(value, refs);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, refs);
            }
        }
        _ => {}
    }
}

/// A `$ref` fragment's last segment as a key: percent-decoded (a fragment is a
/// URI part, so a non-ASCII name arrives encoded) and JSON Pointer unescaped.
fn fragment_key(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied(),
            bytes.get(i + 2).copied(),
        ) {
            (b'%', Some(h), Some(l)) if hex(h).is_some() && hex(l).is_some() => {
                decoded.push((hex(h).unwrap() * 16 + hex(l).unwrap()) as u8);
                i += 3;
            }
            (b, _, _) => {
                decoded.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded)
        .replace("~1", "/")
        .replace("~0", "~")
}

/// [`declared`], with an inline envelope mixin not counted (see
/// [`Catalog::declares_own`]).
fn own(schema: &Value, name: &str, document: &Value, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        if let Some(property) = properties.get(name) {
            let standard = match name {
                "catalogId" => property.get("type").and_then(Value::as_str) == Some("string"),
                "metadata" => {
                    property.get("type").and_then(Value::as_str) == Some("object")
                        && property
                            .get("properties")
                            .and_then(Value::as_object)
                            .is_some_and(|inner| {
                                inner.len() == 1 && inner.contains_key("extensions")
                            })
                }
                _ => false,
            };
            if !(standard && properties.contains_key("id")) {
                return true;
            }
        }
    }
    if let Some(local) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|target| target.strip_prefix('#'))
    {
        if let Some(target) = document.pointer(local) {
            if own(target, name, document, depth + 1) {
                return true;
            }
        }
    }
    ["allOf", "oneOf", "anyOf"].iter().any(|key| {
        schema
            .get(*key)
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| own(part, name, document, depth + 1))
            })
    })
}

/// Whether `schema` (or a part of it) declares property `name`; `depth` stops
/// a `$ref` loop.
fn declared(schema: &Value, name: &str, document: &Value, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    if schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(name))
    {
        return true;
    }
    if let Some(local) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|target| target.strip_prefix('#'))
    {
        if let Some(target) = document.pointer(local) {
            if declared(target, name, document, depth + 1) {
                return true;
            }
        }
    }
    ["allOf", "oneOf", "anyOf"].iter().any(|key| {
        schema
            .get(*key)
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| declared(part, name, document, depth + 1))
            })
    })
}

fn ref_problem(
    target: &str,
    components: &serde_json::Map<String, Value>,
    functions: &serde_json::Map<String, Value>,
    defs: &serde_json::Map<String, Value>,
) -> Option<String> {
    if let Some(name) = target.strip_prefix("#/components/") {
        return (!components.contains_key(&fragment_key(name)))
            .then(|| format!("$ref {target:?} names no component of this catalog"));
    }
    if let Some(name) = target.strip_prefix("#/functions/") {
        return (!functions.contains_key(&fragment_key(name)))
            .then(|| format!("$ref {target:?} names no function of this catalog"));
    }
    if let Some(name) = target.strip_prefix("#/$defs/") {
        if name == "anyComponent" || name == "anyFunction" {
            return Some(format!(
                "$ref {target:?} may not be referenced from inside the catalog"
            ));
        }
        return (!defs.contains_key(&fragment_key(name)))
            .then(|| format!("$ref {target:?} names no $defs entry of this catalog"));
    }
    if let Some(name) = target.strip_prefix("common_types.json#/$defs/") {
        return (!ALLOWED_EXTERNAL_REF_TARGETS.contains(&name))
            .then(|| format!("$ref {target:?} is not one of the common types a catalog may use"));
    }
    Some(format!(
        "$ref {target:?} points outside the catalog and the common types, which is not allowed"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_ref_fragment_is_percent_decoded_and_unescaped() {
        assert_eq!(fragment_key("%F0%9D%91%A5Label"), "𝑥Label");
        assert_eq!(fragment_key("a~1b~0c"), "a/b~c");
        assert_eq!(fragment_key("100%"), "100%");
    }

    #[test]
    fn an_array_of_component_ids_is_a_child_list() {
        let catalog = Catalog::from_value(json!({
            "protocolVersion": "1.0",
            "catalogId": "slots",
            "components": {"Slots": {
                "type": "object",
                "properties": {
                    "component": {"const": "Slots"},
                    "slots": {"type": "array", "items": {"$ref": "common_types.json#/$defs/ComponentId"}}
                },
                "required": ["component"]
            }},
            "$defs": {"anyComponent": {"oneOf": [{"$ref": "#/components/Slots"}]}, "anyFunction": false}
        }))
        .unwrap();
        assert_eq!(
            catalog.component("Slots").unwrap().children.get("slots"),
            Some(&ChildRef::List)
        );
    }

    #[test]
    fn the_basic_catalog_loads_with_its_structure() {
        let basic = Catalog::basic();
        assert_eq!(basic.id(), BASIC_CATALOG_ID);
        assert_eq!(basic.components().len(), 18);
        assert_eq!(
            basic.component("Column").unwrap().children.get("children"),
            Some(&ChildRef::List)
        );
        assert_eq!(
            basic.component("Button").unwrap().children.get("child"),
            Some(&ChildRef::Single)
        );
        assert!(basic.component("Text").unwrap().children.is_empty());
        assert_eq!(
            basic.component("Tabs").unwrap().children.get("tabs"),
            Some(&ChildRef::Items("child".into()))
        );
        assert!(basic
            .component("Button")
            .unwrap()
            .actions
            .contains("action"));
        let open = basic.function("openUrl").unwrap();
        assert!(open.requires_user_activation);
        assert_eq!(open.callers, Callers::RendererOnly);
        assert_eq!(
            basic.function("required").unwrap().return_type,
            "validationResult"
        );
        assert!(basic.instructions().unwrap().contains("formatString"));
    }

    fn minimal(extra: Value) -> Value {
        let mut catalog = json!({
            "protocolVersion": "1.0",
            "catalogId": "example.com:test",
            "components": {
                "Label": {
                    "type": "object",
                    "properties": {
                        "component": {"const": "Label"},
                        "text": {"$ref": "common_types.json#/$defs/DynamicString"}
                    },
                    "required": ["component", "text"]
                }
            },
            "functions": {},
            "$defs": {
                "anyComponent": {"oneOf": [{"$ref": "#/components/Label"}]},
                "anyFunction": false
            }
        });
        if let (Some(base), Some(extra)) = (catalog.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        catalog
    }

    #[test]
    fn a_minimal_catalog_loads() {
        let catalog = Catalog::from_value(minimal(json!({}))).unwrap();
        assert_eq!(catalog.id(), "example.com:test");
    }

    #[test]
    fn refuses_a_catalog_for_another_protocol_version() {
        let error = Catalog::from_value(minimal(json!({"protocolVersion": "0.9"}))).unwrap_err();
        assert!(error.to_string().contains("1.0 catalogs only"), "{error}");
        let mut no_version = minimal(json!({}));
        no_version
            .as_object_mut()
            .unwrap()
            .remove("protocolVersion");
        assert!(Catalog::from_value(no_version).is_err());
    }

    #[test]
    fn refuses_a_reference_outside_the_catalog() {
        let catalog = minimal(json!({
            "$defs": {
                "anyComponent": {"oneOf": [{"$ref": "#/components/Label"}]},
                "anyFunction": false,
                "Remote": {"$ref": "https://example.com/elsewhere.json"}
            }
        }));
        let error = Catalog::from_value(catalog).unwrap_err();
        assert!(error.to_string().contains("points outside"), "{error}");
    }

    #[test]
    fn refuses_a_common_type_not_on_the_list() {
        let catalog = minimal(json!({
            "$defs": {
                "anyComponent": {"oneOf": [{"$ref": "#/components/Label"}]},
                "anyFunction": false,
                "Sneaky": {"$ref": "common_types.json#/$defs/FunctionResponse"}
            }
        }));
        let error = Catalog::from_value(catalog).unwrap_err();
        assert!(
            error.to_string().contains("not one of the common types"),
            "{error}"
        );
    }

    #[test]
    fn refuses_a_component_whose_discriminator_does_not_match() {
        let catalog = minimal(json!({
            "components": {
                "Label": {
                    "type": "object",
                    "properties": {"component": {"const": "Other"}},
                    "required": ["component"]
                }
            }
        }));
        let error = Catalog::from_value(catalog).unwrap_err();
        assert!(error.to_string().contains("must declare"), "{error}");
    }

    #[test]
    fn refuses_a_name_that_is_not_an_identifier() {
        let catalog = minimal(json!({
            "components": {
                "Label": {
                    "type": "object",
                    "properties": {
                        "component": {"const": "Label"},
                        "font-size": {"type": "number"}
                    },
                    "required": ["component"]
                }
            }
        }));
        let error = Catalog::from_value(catalog).unwrap_err();
        assert!(error.to_string().contains("\"font-size\""), "{error}");
    }
}
