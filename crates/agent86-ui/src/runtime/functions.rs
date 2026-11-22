//! Evaluating dynamic values: data bindings, function calls, and the basic
//! catalog's functions, with the specification's coercion rules
//! ("`DynamicBoolean` and `CheckRule` Coercion") and catalog resolution
//! ("Mixable catalogs and component resolution logic").
//!
//! A call resolves in its own `catalogId`, else the scope's default catalog
//! (the surface's, or, inside a `formatString` template, the template's), else
//! it fails. The function must be declared by that catalog and implemented
//! here; the basic catalog's functions are implemented natively, whatever
//! catalog declares them. A function that needs a user gesture runs only in a
//! scope that has one, and an `agentOnly` function never runs on the renderer.

use super::data::{self, DataError, DataModel};
use super::expr::{self, Part};
use super::format;
use super::regex::Pattern;
use crate::catalog::{Callers, Catalog};
use serde_json::{json, Map, Value};
use std::cell::RefCell;

/// A failure while evaluating, in the specification's error categories.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// A function failed, was refused, or a template did not parse
    /// (`EXPRESSION_ERROR`).
    #[error("{0}")]
    Expression(String),
    /// A call's catalog could not be resolved.
    #[error("{0}")]
    Catalog(String),
    /// A data binding's path is not allowed.
    #[error(transparent)]
    Data(#[from] DataError),
    /// A local function ran and failed (`EXECUTION_ERROR`).
    #[error("{0}")]
    Execution(String),
}

impl EvalError {
    /// The category name the shared suites use.
    pub fn category(&self) -> &'static str {
        match self {
            EvalError::Expression(_) => "ExpressionError",
            EvalError::Catalog(_) => "CatalogError",
            EvalError::Data(_) => "DataError",
            EvalError::Execution(_) => "ExecutionError",
        }
    }
}

impl From<expr::ExpressionError> for EvalError {
    fn from(error: expr::ExpressionError) -> Self {
        EvalError::Expression(error.0)
    }
}

fn expression<T>(message: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError::Expression(message.into()))
}

/// Something a function asks the renderer to do (only `openUrl`, so far).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Effect {
    /// Open this `http` or `https` URL, in response to the user's gesture.
    OpenUrl(String),
    /// Set this data model path (a local function's write, applied when the
    /// action finishes).
    SetData {
        /// The absolute JSON Pointer.
        path: String,
        /// The value.
        value: Value,
    },
}

/// A function the embedder implements for its own catalog, run on the
/// renderer: given the resolved arguments and its context, it returns a value
/// or fails with a message (an `EXECUTION_ERROR`).
pub type LocalFunction =
    std::sync::Arc<dyn Fn(&Map<String, Value>, &Scope<'_>) -> Result<Value, String> + Send + Sync>;

/// Where an expression is evaluated.
pub struct Scope<'a> {
    /// The surface's data model.
    pub data: &'a DataModel,
    /// The data context a relative path resolves against (`/` at the root, the
    /// item's path inside a template).
    pub path: String,
    /// The innermost template's item index, for `@index`.
    pub index: Option<usize>,
    /// The catalog a call without its own `catalogId` resolves in.
    pub catalog: Option<String>,
    /// Whether this runs from a user gesture (an action), which functions
    /// that need one require.
    pub activation: bool,
    effects: RefCell<Vec<Effect>>,
}

impl<'a> Scope<'a> {
    /// The root scope of a surface whose default catalog is `catalog`.
    pub fn root(data: &'a DataModel, catalog: Option<&str>) -> Self {
        Self {
            data,
            path: "/".into(),
            index: None,
            catalog: catalog.map(String::from),
            activation: false,
            effects: RefCell::new(Vec::new()),
        }
    }

    /// This scope inside a template item.
    pub fn item(&self, path: String, index: usize) -> Scope<'a> {
        Scope {
            data: self.data,
            path,
            index: Some(index),
            catalog: self.catalog.clone(),
            activation: self.activation,
            effects: RefCell::new(Vec::new()),
        }
    }

    fn with_catalog(&self, catalog: Option<String>) -> Scope<'a> {
        Scope {
            data: self.data,
            path: self.path.clone(),
            index: self.index,
            catalog,
            activation: self.activation,
            effects: RefCell::new(Vec::new()),
        }
    }

    /// The effects functions asked for, in order, taken out of the scope.
    pub fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut self.effects.borrow_mut())
    }

    fn absorb(&self, inner: &Scope<'_>) {
        self.effects.borrow_mut().extend(inner.take_effects());
    }

    /// Read the data model at a path relative to this scope (`None` when
    /// absent or not allowed).
    pub fn get(&self, path: &str) -> Option<Value> {
        let resolved = data::resolve(&self.path, path);
        self.data.get(&resolved).ok().flatten().cloned()
    }

    /// Ask for a data model write, for a local function: applied when the
    /// action that ran it finishes, and dropped when it ran from a binding.
    pub fn set(&self, path: &str, value: Value) {
        let path = data::resolve(&self.path, path);
        self.effects
            .borrow_mut()
            .push(Effect::SetData { path, value });
    }
}

/// The `DynamicBoolean` coercion: booleans as themselves, a validation result
/// as its `valid`, `"true"`/`"false"` in any case, a nonzero number as true,
/// and everything else (other strings, `null`, objects, arrays) as false.
pub fn coerce_bool(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Object(map) => map.get("valid").and_then(Value::as_bool).unwrap_or(false),
        Value::String(s) => s.eq_ignore_ascii_case("true"),
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        _ => false,
    }
}

/// A validation result's `valid`, if the value is one.
pub fn validity(value: &Value) -> Option<bool> {
    value.as_object()?.get("valid")?.as_bool()
}

fn result(valid: bool, message: Option<String>) -> Value {
    let mut map = Map::new();
    map.insert("valid".into(), Value::Bool(valid));
    if let Some(message) = message.filter(|_| !valid) {
        map.insert("message".into(), Value::String(message));
    }
    Value::Object(map)
}

/// Evaluates dynamic values against a set of catalogs.
#[derive(Clone)]
pub struct Evaluator {
    catalogs: Vec<Catalog>,
    locals: Vec<(String, crate::catalog::FunctionDef, LocalFunction)>,
}

impl std::fmt::Debug for Evaluator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Evaluator")
            .field(
                "catalogs",
                &self.catalogs.iter().map(Catalog::id).collect::<Vec<_>>(),
            )
            .field(
                "locals",
                &self
                    .locals
                    .iter()
                    .map(|(name, _, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

const NATIVE: &[&str] = &[
    "required",
    "regex",
    "length",
    "numeric",
    "email",
    "formatString",
    "formatNumber",
    "formatCurrency",
    "formatDate",
    "pluralize",
    "openUrl",
    "and",
    "or",
    "not",
];

impl Evaluator {
    /// An evaluator over the catalogs a surface may use.
    pub fn new(catalogs: impl IntoIterator<Item = Catalog>) -> Self {
        Self {
            catalogs: catalogs.into_iter().collect(),
            locals: Vec::new(),
        }
    }

    /// Implement a function on the renderer: `def` says who may call it and
    /// whether it needs a gesture, as a catalog entry would. A registered
    /// function can be called from any of the evaluator's catalogs.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        def: crate::catalog::FunctionDef,
        function: LocalFunction,
    ) {
        let name = name.into();
        self.locals.retain(|(existing, _, _)| *existing != name);
        self.locals.push((name, def, function));
    }

    /// An evaluator over the basic catalog alone.
    pub fn basic() -> Self {
        Self::new([Catalog::basic().clone()])
    }

    fn catalog(&self, id: &str) -> Option<&Catalog> {
        self.catalogs.iter().find(|catalog| catalog.id() == id)
    }

    /// Resolve a dynamic value: a `{"@path": ...}` reads the data model (absent
    /// is `null`), a `{"@call": ...}` calls the function, an array resolves
    /// element by element, and any other object is a literal (its `@@` keys
    /// unescaped to `@`). A literal object with a single-`@` key is refused,
    /// as the specification reserves that namespace.
    pub fn evaluate(&self, value: &Value, scope: &Scope<'_>) -> Result<Value, EvalError> {
        match value {
            Value::Object(map) => {
                if let Some(path) = map.get("@path").and_then(Value::as_str) {
                    let resolved = data::resolve(&scope.path, path);
                    return Ok(scope.data.get(&resolved)?.cloned().unwrap_or(Value::Null));
                }
                if map.get("@call").is_some_and(Value::is_string) {
                    return self.call(map, scope);
                }
                let mut literal = Map::new();
                for (key, inner) in map {
                    let key = if let Some(rest) = key.strip_prefix("@@") {
                        format!("@{rest}")
                    } else if key.starts_with('@') {
                        return expression(format!("'{key}' is a reserved key"));
                    } else {
                        key.clone()
                    };
                    literal.insert(key, inner.clone());
                }
                Ok(Value::Object(literal))
            }
            Value::Array(items) => items
                .iter()
                .map(|item| self.evaluate(item, scope))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            other => Ok(other.clone()),
        }
    }

    /// Call a function: `{"@call": name, "catalogId"?: ..., "args"?: {...}}`.
    pub fn call(&self, call: &Map<String, Value>, scope: &Scope<'_>) -> Result<Value, EvalError> {
        let name = call
            .get("@call")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let empty = Map::new();
        let raw_args = call
            .get("args")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        if name == "@index" {
            let Some(index) = scope.index else {
                return expression("@index function can only be evaluated inside a collection template iteration scope.");
            };
            let offset = match raw_args.get("offset") {
                Some(value) => format::to_number(&self.evaluate(value, scope)?),
                None => 0.0,
            };
            return Ok(expr::number_value(index as f64 + offset));
        }
        if name.starts_with('@') {
            return expression(format!("Unknown system function '{name}'"));
        }
        let catalog_id = match call.get("catalogId").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => scope.catalog.clone().ok_or_else(|| {
                EvalError::Catalog(format!(
                    "Cannot resolve function '{name}': the call names no catalogId and the surface has no default catalogId"
                ))
            })?,
        };
        let catalog = self
            .catalog(&catalog_id)
            .ok_or_else(|| EvalError::Catalog(format!("Catalog not found: {catalog_id}")))?;
        let local = self.locals.iter().find(|(local, _, _)| local == name);
        let Some(def) = catalog.function(name).or(local.map(|(_, def, _)| def)) else {
            return expression(format!(
                "Function '{name}' is not defined in catalog '{catalog_id}'"
            ));
        };
        if def.callers == Callers::AgentOnly {
            return expression(format!(
                "Function '{name}' is agentOnly and cannot be called by renderer"
            ));
        }
        if def.requires_user_activation && !scope.activation {
            return expression(format!(
                "Function '{name}' requires user activation: it can only run from an action"
            ));
        }
        if local.is_none() && !NATIVE.contains(&name) {
            return expression(format!(
                "Function '{name}' of catalog '{catalog_id}' has no implementation in this renderer"
            ));
        }
        // Nested calls resolve in their own catalog or the scope's default;
        // only a formatString template passes its catalog on.
        let mut args = Map::new();
        for (key, value) in raw_args {
            args.insert(key.clone(), self.evaluate(value, scope)?);
        }
        match local {
            Some((_, _, function)) => function(&args, scope).map_err(EvalError::Execution),
            None => self.native(name, &args, &catalog_id, scope),
        }
    }

    fn native(
        &self,
        name: &str,
        args: &Map<String, Value>,
        catalog_id: &str,
        scope: &Scope<'_>,
    ) -> Result<Value, EvalError> {
        let arg = |key: &str| args.get(key).unwrap_or(&Value::Null);
        let number = |key: &str| args.get(key).map(format::to_number).filter(|n| !n.is_nan());
        let decimals =
            || -> Option<usize> { number("decimals").map(|d| d.clamp(0.0, 20.0).round() as usize) };
        let grouping = || args.get("grouping").is_none_or(coerce_bool);
        Ok(match name {
            "required" => {
                let present = match arg("value") {
                    Value::Null => false,
                    Value::String(s) => !s.is_empty(),
                    Value::Array(items) => !items.is_empty(),
                    _ => true,
                };
                result(present, Some("This field is required.".into()))
            }
            "regex" => {
                let pattern = arg("pattern").as_str().unwrap_or_default();
                let compiled = Pattern::compile(pattern).map_err(|refusal| {
                    EvalError::Expression(format!(
                        "Invalid or unsafe regex pattern ({}): {pattern}",
                        refusal.as_str()
                    ))
                })?;
                let value = format::to_string(arg("value"));
                if value.chars().count() > super::regex::MAX_VALUE_CODE_POINTS {
                    result(
                        false,
                        Some("Value exceeds the maximum length for pattern matching.".into()),
                    )
                } else {
                    result(
                        compiled.test(&value),
                        Some("Value does not match required pattern.".into()),
                    )
                }
            }
            "length" => {
                let length = match arg("value") {
                    Value::String(s) => s.encode_utf16().count() as f64,
                    Value::Array(items) => items.len() as f64,
                    _ => 0.0,
                };
                match (number("min"), number("max")) {
                    (Some(min), _) if length < min => result(
                        false,
                        Some(format!("Minimum length is {}.", format::number_string(min))),
                    ),
                    (_, Some(max)) if length > max => result(
                        false,
                        Some(format!("Maximum length is {}.", format::number_string(max))),
                    ),
                    _ => result(true, None),
                }
            }
            "numeric" => {
                let value = format::to_number(arg("value"));
                if value.is_nan() {
                    result(false, Some("Value must be a valid number.".into()))
                } else {
                    match (number("min"), number("max")) {
                        (Some(min), _) if value < min => result(
                            false,
                            Some(format!("Minimum value is {}.", format::number_string(min))),
                        ),
                        (_, Some(max)) if value > max => result(
                            false,
                            Some(format!("Maximum value is {}.", format::number_string(max))),
                        ),
                        _ => result(true, None),
                    }
                }
            }
            "email" => {
                let valid = arg("value").as_str().is_some_and(is_email);
                if valid {
                    result(true, None)
                } else {
                    json!({
                        "valid": false,
                        "code": "INVALID_EMAIL",
                        "severity": "error",
                        "message": "Please enter a valid email address (e.g. user@example.com)."
                    })
                }
            }
            "formatString" => match arg("value") {
                Value::String(template) => {
                    Value::String(self.template(template, catalog_id, scope)?)
                }
                other => Value::String(format::to_string(other)),
            },
            "formatNumber" => Value::String(format::format_number(
                format::to_number(arg("value")),
                decimals(),
                grouping(),
            )),
            "formatCurrency" => Value::String(format::format_currency(
                format::to_number(arg("value")),
                &format::to_string(arg("currency")),
                decimals(),
                grouping(),
            )),
            "formatDate" => Value::String(format::format_date(
                arg("value"),
                &format::to_string(arg("format")),
            )),
            "pluralize" => {
                let category = format::plural_category(format::to_number(arg("value")), |key| {
                    args.contains_key(key)
                });
                let chosen = args.get(category).or_else(|| args.get("other"));
                Value::String(chosen.map(format::to_string).unwrap_or_default())
            }
            "openUrl" => {
                let url = format::to_string(arg("url"));
                let trimmed = url.trim();
                let lower = trimmed.to_ascii_lowercase();
                if !(lower.starts_with("https://") || lower.starts_with("http://")) {
                    return expression(format!(
                        "openUrl only opens http and https URLs, not {trimmed:?}"
                    ));
                }
                scope
                    .effects
                    .borrow_mut()
                    .push(Effect::OpenUrl(trimmed.to_string()));
                Value::Null
            }
            "and" | "or" => {
                let values = arg("values")
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if values.len() < 2 {
                    return expression(format!("{name} requires at least 2 values"));
                }
                Value::Bool(if name == "and" {
                    values.iter().all(coerce_bool)
                } else {
                    values.iter().any(coerce_bool)
                })
            }
            "not" => Value::Bool(!coerce_bool(arg("value"))),
            _ => return expression(format!("Function '{name}' has no implementation")),
        })
    }

    /// Evaluate a `formatString` template: its calls resolve in the catalog the
    /// `formatString` call resolved to, and every part becomes text.
    fn template(
        &self,
        template: &str,
        catalog_id: &str,
        scope: &Scope<'_>,
    ) -> Result<String, EvalError> {
        let parts = expr::parse(template)?;
        let inner = scope.with_catalog(Some(catalog_id.to_string()));
        let mut out = String::new();
        for part in &parts {
            let value = self.evaluate(&part_value(part, catalog_id), &inner)?;
            out.push_str(&format::to_string(&value));
        }
        scope.absorb(&inner);
        Ok(out)
    }
}

/// A template part as a dynamic value: a binding as `{"@path"}`, a call as
/// `{"@call"}` in the template's catalog (system functions stay catalogless).
fn part_value(part: &Part, catalog_id: &str) -> Value {
    match part {
        Part::Literal(value) => value.clone(),
        Part::Path(path) => json!({ "@path": path }),
        Part::Call { name, args } => {
            let args: Map<String, Value> = args
                .iter()
                .map(|(key, value)| (key.clone(), part_value(value, catalog_id)))
                .collect();
            let mut call = Map::new();
            call.insert("@call".into(), Value::String(name.clone()));
            call.insert("args".into(), Value::Object(args));
            if !name.starts_with('@') {
                call.insert("catalogId".into(), Value::String(catalog_id.into()));
            }
            Value::Object(call)
        }
    }
}

/// The email check every reference SDK uses:
/// `^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$`.
fn is_email(value: &str) -> bool {
    static EMAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    EMAIL
        .get_or_init(|| {
            regex::Regex::new(r"^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$")
                .expect("email pattern")
        })
        .is_match(value)
}
