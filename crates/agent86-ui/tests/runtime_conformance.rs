//! The A2UI specification's shared conformance suites for the runtime, run
//! against `agent86_ui::runtime`.
//!
//! `tests/fixtures/conformance/core/*.json` are the upstream YAML suites
//! (`conformance/core/` at the commit in `spec/UPSTREAM.md`), converted to JSON
//! unchanged so the tests need no YAML parser. Each case names an `action`; a
//! handler per action drives the runtime and checks the expectation.
//!
//! The upstream harness guide's strict rule applies: an unknown action, step
//! op, or key fails the case. The only escape is `UNSUPPORTED`, which records
//! why a case is skipped; an entry that matches no case fails the run, so the
//! list cannot go stale.

use agent86_ui::runtime::{data, expr, DataModel, EvalError, Evaluator, Scope};
use agent86_ui::Catalog;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

fn suite(name: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/conformance/core")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let cases: Vec<Value> = serde_json::from_str(&text).unwrap();
    assert!(!cases.is_empty(), "{name} holds no cases");
    cases
}

/// Fail on any key outside `allowed` (plus the metadata every case may carry).
fn known_keys(object: &Value, allowed: &[&str], at: &str) -> Result<(), String> {
    const META: &[&str] = &["name", "description", "features"];
    for key in object.as_object().into_iter().flat_map(|o| o.keys()) {
        if !allowed.contains(&key.as_str()) && !META.contains(&key.as_str()) {
            return Err(format!("Unknown key '{key}' in {at}"));
        }
    }
    Ok(())
}

fn expect_error(object: &Value) -> Option<&Value> {
    object
        .get("expect_error")
        .or_else(|| object.get("expectError"))
}

/// Check an error against `expect_error`: the category the handler mapped it
/// to, and `message` as a regular expression.
fn check_error(expected: &Value, category: &str, message: &str) -> Result<(), String> {
    let want = expected["category"].as_str().unwrap_or_default();
    if want != category {
        return Err(format!("expected a {want}, got a {category}: {message}"));
    }
    if let Some(pattern) = expected.get("message").and_then(Value::as_str) {
        let re = regex::Regex::new(pattern).unwrap();
        if !re.is_match(message) {
            return Err(format!(
                "error message {message:?} does not match {pattern:?}"
            ));
        }
    }
    Ok(())
}

/// Whether a case declares a protocol version before 1.0 (top level first,
/// then its `catalog` block, as the harness guide says).
fn pre_1_0(case: &Value) -> bool {
    let version = case
        .get("protocolVersion")
        .or_else(|| case.get("catalog").and_then(|c| c.get("protocolVersion")))
        .and_then(Value::as_str);
    version.is_some_and(|v| v.starts_with('v'))
}

fn type_name(value: Option<&Value>) -> &'static str {
    match value {
        None => "absent",
        Some(Value::Object(_)) => "object",
        Some(Value::Array(_)) => "list",
        Some(Value::String(_)) => "string",
        Some(Value::Number(_)) => "number",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Null) => "null",
    }
}

/// Which cases a suite runs.
#[derive(Clone, Copy, PartialEq)]
enum Versions {
    /// Every case: the behavior does not change between protocol versions
    /// (the data model, paths, the template grammar), whatever a case's label.
    All,
    /// Only cases for 1.0 or with no version: results differ by version, and
    /// this implementation reads 1.0 only.
    From1_0,
    /// Only cases marked 1.0: the version-less ones use 0.9's syntax (a
    /// binding written `{"path": ...}`, which 1.0 reads as a literal).
    Only1_0,
}

/// Run every case of `suite` with an `action` in `actions` through `handle`,
/// collecting failures. `unsupported` lists case names to skip, with reasons.
fn run(
    suite_name: &str,
    versions: Versions,
    actions: &[&str],
    unsupported: &[(&str, &str)],
    handle: impl Fn(&Value) -> Result<(), String>,
) {
    let cases = suite(suite_name);
    let mut failures = Vec::new();
    let mut matched = BTreeSet::new();
    let mut ran = 0;
    for case in &cases {
        let name = case["name"].as_str().unwrap_or("?");
        let action = case["action"].as_str().unwrap_or("?");
        if let Some((skip, _)) = unsupported.iter().find(|(skip, _)| *skip == name) {
            matched.insert(*skip);
            continue;
        }
        let marked_1_0 = case
            .get("protocolVersion")
            .or_else(|| case.get("catalog").and_then(|c| c.get("protocolVersion")))
            .and_then(Value::as_str)
            == Some("1.0");
        if (versions == Versions::From1_0 && pre_1_0(case))
            || (versions == Versions::Only1_0 && !marked_1_0)
        {
            // This implementation reads A2UI 1.0 only.
            continue;
        }
        if !actions.contains(&action) {
            failures.push(format!("{suite_name}#{name}: Unknown action '{action}'"));
            continue;
        }
        ran += 1;
        if let Err(problem) = handle(case) {
            failures.push(format!("{suite_name}#{name}: {problem}"));
        }
    }
    for (skip, _) in unsupported {
        if !matched.contains(skip) {
            failures.push(format!(
                "{suite_name}: UNSUPPORTED entry {skip} matches no case"
            ));
        }
    }
    assert!(ran > 0, "{suite_name}: no case ran");
    assert!(
        failures.is_empty(),
        "{} of {} cases in {suite_name} failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

fn data_model_case(case: &Value) -> Result<(), String> {
    known_keys(
        case,
        &["action", "catalog", "initial", "watch", "steps"],
        "case",
    )?;
    let mut model = DataModel::new(case.get("initial").cloned().unwrap_or_else(|| json!({})));
    let mut watched = Vec::new();
    for path in case
        .get("watch")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = path.as_str().unwrap();
        watched.push((model.watch(path).map_err(|e| e.message)?, path.to_string()));
    }
    for (number, step) in case["steps"].as_array().unwrap().iter().enumerate() {
        let at = format!("step {number}");
        known_keys(
            step,
            &[
                "op",
                "path",
                "value",
                "expect",
                "expect_absent",
                "expect_type",
                "expect_notified",
                "expect_values",
                "expect_error",
            ],
            &at,
        )?;
        let path = step.get("path").and_then(Value::as_str).unwrap_or("/");
        let op = step["op"].as_str().unwrap();
        let outcome = match op {
            "get" => model
                .get(path)
                .map(|value| (Vec::new(), Some(value.cloned()))),
            "set" => model
                .set(path, step["value"].clone())
                .map(|ids| (ids, None)),
            "delete" => model.delete(path).map(|ids| (ids, None)),
            "dispose" => {
                model.dispose();
                Ok((Vec::new(), None))
            }
            // Mutating what the caller passed to `set`: Rust moves the value
            // in, so there is nothing to mutate and nothing can change.
            "mutate_input" => Ok((Vec::new(), None)),
            other => return Err(format!("Unknown op '{other}' at {at}")),
        };
        match (outcome, expect_error(step)) {
            (Err(error), Some(expected)) => check_error(expected, "DataError", &error.message)
                .map_err(|e| format!("{at}: {e}"))?,
            (Err(error), None) => return Err(format!("{at}: unexpected error: {}", error.message)),
            (Ok(_), Some(expected)) => {
                return Err(format!("{at}: expected {expected}, got success"))
            }
            (Ok((notified, got)), None) => {
                let got = got.flatten();
                if let Some(expected) = step.get("expect") {
                    if got.as_ref() != Some(expected) {
                        return Err(format!(
                            "{at}: get {path} gave {got:?}, expected {expected}"
                        ));
                    }
                }
                if step.get("expect_absent").and_then(Value::as_bool) == Some(true) && got.is_some()
                {
                    return Err(format!("{at}: get {path} gave {got:?}, expected absent"));
                }
                if let Some(expected) = step.get("expect_type").and_then(Value::as_str) {
                    let found = type_name(got.as_ref());
                    if found != expected {
                        return Err(format!("{at}: {path} is {found}, expected {expected}"));
                    }
                }
                if let Some(expected) = step.get("expect_notified").and_then(Value::as_array) {
                    let mut found: Vec<&str> = watched
                        .iter()
                        .filter(|(id, _)| notified.contains(id))
                        .map(|(_, path)| path.as_str())
                        .collect();
                    let mut expected: Vec<&str> =
                        expected.iter().filter_map(Value::as_str).collect();
                    found.sort_unstable();
                    expected.sort_unstable();
                    if found != expected {
                        return Err(format!("{at}: notified {found:?}, expected {expected:?}"));
                    }
                }
                if let Some(values) = step.get("expect_values").and_then(Value::as_object) {
                    for (watch_path, expected) in values {
                        let value = model.get(watch_path).map_err(|e| e.message)?;
                        if value != Some(expected) {
                            return Err(format!(
                                "{at}: {watch_path} is {value:?}, expected {expected}"
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn data_model() {
    run(
        "data_model",
        Versions::All,
        &["data_model"],
        &[],
        data_model_case,
    );
}

#[test]
fn data_context() {
    run(
        "data_context",
        Versions::All,
        &["resolve_path"],
        &[],
        |case| {
            known_keys(case, &["action", "catalog", "args", "expect"], "case")?;
            let args = &case["args"];
            known_keys(args, &["path", "contextPath"], "args")?;
            let got = data::resolve(
                args["contextPath"].as_str().unwrap_or("/"),
                args["path"].as_str().unwrap(),
            );
            let expected = case["expect"].as_str().unwrap();
            if got != expected {
                return Err(format!("resolved to {got:?}, expected {expected:?}"));
            }
            Ok(())
        },
    );
}

/// Join adjacent text literals and drop empty ones, as the harness guide says
/// to before comparing: a template fixes the values, not how text is split.
fn joined(parts: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for part in parts {
        if let Value::String(text) = part {
            if text.is_empty() {
                continue;
            }
            if let Some(Value::String(previous)) = out.last_mut() {
                previous.push_str(text);
                continue;
            }
        }
        out.push(part.clone());
    }
    out
}

#[test]
fn expressions() {
    run(
        "expressions",
        Versions::All,
        &["parse_expression_template", "validate"],
        &[],
        |case| {
            if case["action"] == "validate" {
                return process_case(case);
            }
            known_keys(
                case,
                &["action", "catalog", "input", "expect", "expect_error"],
                "case",
            )?;
            let input = case["input"].as_str().unwrap();
            match (expr::parse(input), expect_error(case)) {
                (Err(error), Some(expected)) => check_error(expected, "ParseError", &error.0),
                (Err(error), None) => Err(format!("unexpected error: {}", error.0)),
                (Ok(parts), Some(expected)) => Err(format!(
                    "expected {expected}, got {:?}",
                    parts.iter().map(expr::Part::to_json).collect::<Vec<_>>()
                )),
                (Ok(parts), None) => {
                    let got: Vec<Value> = parts.iter().map(expr::Part::to_json).collect();
                    let expected = joined(case["expect"].as_array().unwrap());
                    if joined(&got) == expected {
                        Ok(())
                    } else {
                        Err(format!("parsed to {got:?}, expected {expected:?}"))
                    }
                }
            }
        },
    );
}

/// A catalog a case names by its repository path: the basic catalog is the
/// crate's own; a test catalog is read from the fixtures; a catalog for an
/// earlier protocol version is not available to a 1.0 surface, so `None`.
fn catalog_at(path: &str) -> Option<Catalog> {
    match path {
        "catalogs/basic/v1/catalog.json" => Some(Catalog::basic().clone()),
        p if p.starts_with("catalogs/basic/v0_") => None,
        p => {
            let name = p.strip_prefix("conformance/").unwrap_or(p);
            let file = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/conformance")
                .join(name);
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            Some(Catalog::from_value(serde_json::from_str(&text).unwrap()).unwrap())
        }
    }
}

fn evaluate_function_case(case: &Value) -> Result<(), String> {
    if case["action"] == "validate" {
        return process_case(case);
    }
    known_keys(
        case,
        &[
            "action",
            "catalog",
            "protocolVersion",
            "function",
            "args",
            "expect",
            "expectError",
            "surface",
            "catalogId",
            "catalogPaths",
            "dataModel",
        ],
        "case",
    )?;
    let function = case["function"].as_str().unwrap();
    let data = DataModel::new(match case.get("dataModel") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        _ => json!({}),
    });
    let mut call = serde_json::Map::new();
    call.insert("@call".into(), json!(function));
    call.insert(
        "args".into(),
        case.get("args").cloned().unwrap_or_else(|| json!({})),
    );
    let (evaluator, scope) = match case.get("surface") {
        // On a surface: its catalogs, its default, and the call's own catalogId.
        Some(surface) => {
            known_keys(surface, &["catalogId"], "surface")?;
            let catalogs: Vec<Catalog> = case["catalogPaths"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|p| catalog_at(p.as_str().unwrap()))
                .collect();
            if let Some(id) = case.get("catalogId").and_then(Value::as_str) {
                call.insert("catalogId".into(), json!(id));
            }
            let scope = Scope::root(&data, surface.get("catalogId").and_then(Value::as_str));
            (Evaluator::new(catalogs), scope)
        }
        // Directly, in the basic catalog, from a gesture (as the reference
        // harnesses call functions directly).
        None => {
            let mut scope = Scope::root(&data, Some(agent86_ui::BASIC_CATALOG_ID));
            scope.activation = true;
            (Evaluator::basic(), scope)
        }
    };
    let outcome: Result<Value, EvalError> = evaluator.call(&call, &scope);
    match (outcome, expect_error(case)) {
        (Err(error), Some(expected)) => check_error(expected, error.category(), &error.to_string()),
        (Err(error), None) => Err(format!("unexpected {}: {error}", error.category())),
        (Ok(value), Some(expected)) => Err(format!("expected {expected}, got {value}")),
        (Ok(value), None) => {
            let expected = &case["expect"];
            let same = match (expected, &value) {
                // A validation result against a boolean: the version-less
                // cases were written for 0.9, where validators returned
                // booleans; in 1.0 they return results, compared by `valid`.
                (Value::Bool(want), Value::Object(got)) => got.get("valid") == Some(&json!(want)),
                // `{"valid": x}` alone checks only `valid`.
                (Value::Object(want), Value::Object(got))
                    if want.len() == 1 && want.contains_key("valid") =>
                {
                    got.get("valid") == want.get("valid")
                }
                _ => *expected == value,
            };
            if same {
                Ok(())
            } else {
                Err(format!("got {value}, expected {expected}"))
            }
        }
    }
}

#[test]
fn functions() {
    run(
        "functions",
        Versions::From1_0,
        &["evaluate_function", "validate"],
        &[(
            "test_function_format_date_tr35_tokens",
            "validate: the surface processor is not built yet",
        )],
        evaluate_function_case,
    );
}

/// The basic catalog under the id `basic`, which the suites' messages use for
/// it.
fn basic_alias() -> Catalog {
    let mut document = Catalog::basic().document().clone();
    document["catalogId"] = json!("basic");
    Catalog::from_value(document).unwrap()
}

/// An inline test catalog, completed into a valid one: each component gets its
/// discriminator, and the catalog its `anyComponent` and `anyFunction` (the
/// harness guide's advice for a stricter runner, in place of relaxing the
/// crate's own catalog checks).
fn inline_catalog(spec: &Value) -> Catalog {
    let id = spec
        .get("catalogId")
        .and_then(Value::as_str)
        .unwrap_or("basic");
    let Some(components) = spec.get("components").and_then(Value::as_object) else {
        let mut document = Catalog::basic().document().clone();
        document["catalogId"] = json!(id);
        return Catalog::from_value(document).unwrap();
    };
    let mut defs = Vec::new();
    let mut filled = serde_json::Map::new();
    for (name, schema) in components {
        let mut schema = schema.clone();
        schema["type"] = json!("object");
        schema["properties"]["component"] = json!({"const": name});
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        if !required.contains(&json!("component")) {
            let mut required = required;
            required.push(json!("component"));
            schema["required"] = Value::Array(required);
        }
        defs.push(json!({"$ref": format!("#/components/{name}")}));
        filled.insert(name.clone(), schema);
    }
    Catalog::from_value(json!({
        "protocolVersion": "1.0",
        "catalogId": id,
        "components": filled,
        "$defs": {"anyComponent": {"oneOf": defs}, "anyFunction": false}
    }))
    .unwrap()
}

fn runtime_for(case: &Value) -> agent86_ui::runtime::Runtime {
    let catalog = match case.get("catalog") {
        Some(spec) if spec.get("components").is_some() => inline_catalog(spec),
        _ => basic_alias(),
    };
    agent86_ui::runtime::Runtime::new([catalog]).unwrap()
}

/// The categories a report's errors fall in, as the shared suites name them.
fn categories(report: &agent86_ui::Report) -> Vec<(&'static str, String)> {
    use agent86_ui::Code;
    report
        .errors
        .iter()
        .map(|problem| {
            let category = match problem.code {
                Code::IntegrityError => "IntegrityError",
                Code::DataError => "DataError",
                _ => "ValidationError",
            };
            (category, problem.message.clone())
        })
        .collect()
}

/// Whether a report has an error of the expected category and message. An
/// integrity error is a validation error too, as in the reference SDKs.
fn report_matches(expected: &Value, report: &agent86_ui::Report) -> Result<(), String> {
    let found = categories(report);
    let want = expected["category"].as_str().unwrap_or_default();
    let pattern = expected
        .get("message")
        .and_then(Value::as_str)
        .map(|p| regex::Regex::new(p).unwrap());
    let ok = found.iter().any(|(category, message)| {
        (*category == want || (want == "ValidationError" && *category == "IntegrityError"))
            && pattern.as_ref().is_none_or(|re| re.is_match(message))
    });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "expected a {want} {:?}, got {found:?}",
            expected.get("message")
        ))
    }
}

/// Scalars compare as strings (a suite writes `5` or `'5'`); structured values
/// structurally, as the reference harness does.
fn property_matches(actual: Option<&Value>, expected: &Value) -> bool {
    let structured = |v: &Value| v.is_object() || v.is_array();
    match actual {
        None => expected.is_null(),
        Some(actual) if structured(actual) || structured(expected) => actual == expected,
        Some(actual) => {
            agent86_ui::runtime::format::to_string(actual)
                == agent86_ui::runtime::format::to_string(expected)
                || (actual.is_null() && expected.is_null())
        }
    }
}

/// Every node of a surface by the names the suites use: its id, and
/// `<id>_<n>` for an instance a template made for item `n`.
fn nodes_by_name(
    runtime: &agent86_ui::runtime::Runtime,
    surface: &str,
) -> std::collections::BTreeMap<String, agent86_ui::runtime::Node> {
    let mut map = std::collections::BTreeMap::new();
    if let Some(tree) = runtime.tree(surface) {
        for node in tree.walk() {
            if node.index.is_some() {
                let ordinal = node.data_path.rsplit('/').next().unwrap_or("");
                if ordinal.parse::<usize>().is_ok() {
                    map.insert(format!("{}_{ordinal}", node.id), node.clone());
                }
            }
            map.entry(node.id.clone()).or_insert_with(|| node.clone());
        }
    }
    // Components not reachable from root resolve at the root context.
    for component in runtime.components(surface).into_iter().flatten() {
        let id = component["id"].as_str().unwrap_or_default().to_string();
        if let std::collections::btree_map::Entry::Vacant(slot) = map.entry(id) {
            if let Some(node) = runtime.node(surface, slot.key()) {
                slot.insert(node);
            }
        }
    }
    map
}

fn check_node(node: &agent86_ui::runtime::Node, expected: &Value, at: &str) -> Result<(), String> {
    for (key, want) in expected.as_object().into_iter().flatten() {
        let got = match key.as_str() {
            "id" => Some(json!(node.id)),
            "component" => Some(json!(node.component)),
            _ => node.properties.get(key).cloned(),
        };
        if !property_matches(got.as_ref(), want) {
            return Err(format!("{at}.{key} is {got:?}, expected {want}"));
        }
    }
    Ok(())
}

fn check_surfaces(runtime: &agent86_ui::runtime::Runtime, expected: &Value) -> Result<(), String> {
    for (surface, want) in expected["surfaces"].as_object().into_iter().flatten() {
        known_keys(
            want,
            &[
                "components",
                "dataModel",
                "exists",
                "isValid",
                "validationErrors",
                "validationResults",
                "validationResult",
            ],
            "expect.surfaces",
        )?;
        let nodes = nodes_by_name(runtime, surface);
        match want.get("components") {
            Some(Value::Object(components)) => {
                for (id, component) in components {
                    let node = nodes
                        .get(id)
                        .ok_or_else(|| format!("{surface}: no component {id}"))?;
                    check_node(node, component, &format!("{surface}.{id}"))?;
                }
            }
            Some(Value::Array(components)) => {
                let count = runtime.components(surface).map_or(0, Iterator::count);
                if count != components.len() {
                    return Err(format!(
                        "{surface}: {count} components, expected {}",
                        components.len()
                    ));
                }
                for component in components {
                    let id = component["id"].as_str().unwrap_or_default();
                    let node = nodes
                        .get(id)
                        .ok_or_else(|| format!("{surface}: no component {id}"))?;
                    check_node(node, component, &format!("{surface}.{id}"))?;
                }
            }
            _ => {}
        }
        if let Some(model) = want.get("dataModel") {
            let got = runtime.data_model(surface);
            if got != Some(model) {
                return Err(format!("{surface}: data model {got:?}, expected {model}"));
            }
        }
        let per_node = |field: &str,
                        get: &dyn Fn(&agent86_ui::runtime::Node) -> Value|
         -> Result<(), String> {
            for (id, want) in want
                .get(field)
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let node = nodes
                    .get(id)
                    .ok_or_else(|| format!("{surface}: no component {id}"))?;
                let got = get(node);
                let same = match (want, &got) {
                    // `validationResult` is a subset match.
                    (Value::Object(w), Value::Object(g)) if field == "validationResult" => {
                        w.iter().all(|(k, v)| g.get(k) == Some(v))
                    }
                    _ => *want == got,
                };
                if !same {
                    return Err(format!("{surface}.{id}.{field} is {got}, expected {want}"));
                }
            }
            Ok(())
        };
        per_node("isValid", &|node| json!(node.is_valid()))?;
        per_node("validationErrors", &|node| json!(node.validation_errors()))?;
        per_node("validationResults", &|node| json!(node.failures))?;
        per_node("validationResult", &|node| node.validation_result())?;
    }
    Ok(())
}

fn messages_of(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::Object(map) if map.contains_key("messages") => {
            map["messages"].as_array().cloned().unwrap_or_default()
        }
        Value::Object(_) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn process_case(case: &Value) -> Result<(), String> {
    known_keys(
        case,
        &[
            "action",
            "catalog",
            "steps",
            "payload",
            "messages",
            "expect",
            "expectError",
            "expect_error",
        ],
        "case",
    )?;
    let mut runtime = runtime_for(case);
    let steps: Vec<Value> = match case.get("steps").and_then(Value::as_array) {
        Some(steps) => steps.clone(),
        None => vec![json!({
            "payload": case.get("payload").or_else(|| case.get("messages")).cloned().unwrap_or(json!([])),
        })],
    };
    let last = steps.len().saturating_sub(1);
    for (number, step) in steps.iter().enumerate() {
        known_keys(
            step,
            &[
                "payload",
                "messages",
                "expect",
                "expectError",
                "expect_error",
            ],
            "step",
        )?;
        let messages = messages_of(
            step.get("payload")
                .or_else(|| step.get("messages"))
                .unwrap_or(&json!([])),
        );
        let report = runtime.apply(&messages);
        // A case-level expectError belongs to the last step.
        let expected = expect_error(step).or(if number == last {
            expect_error(case)
        } else {
            None
        });
        match expected {
            Some(expected) => {
                report_matches(expected, &report).map_err(|e| format!("step {number}: {e}"))?
            }
            None => {
                if !report.is_ok() {
                    return Err(format!(
                        "step {number}: unexpected errors {:?}",
                        categories(&report)
                    ));
                }
                if let Some(expect) = step.get("expect").filter(|e| !e.is_null()) {
                    check_surfaces(&runtime, expect).map_err(|e| format!("step {number}: {e}"))?;
                }
            }
        }
    }
    if let Some(expect) = case.get("expect").filter(|e| !e.is_null()) {
        check_surfaces(&runtime, expect)?;
    }
    Ok(())
}

const PROCESSOR: &[&str] = &["validate", "process_messages"];

#[test]
fn index_function() {
    run(
        "index_function",
        Versions::From1_0,
        PROCESSOR,
        &[],
        process_case,
    );
}

#[test]
fn validation_result() {
    run(
        "validation_result",
        Versions::From1_0,
        PROCESSOR,
        &[],
        process_case,
    );
}

#[test]
fn data_deletion() {
    run(
        "data_deletion",
        Versions::From1_0,
        PROCESSOR,
        &[],
        process_case,
    );
}

#[test]
fn data_model_pointers() {
    run(
        "data_model_pointers",
        Versions::From1_0,
        PROCESSOR,
        &[],
        process_case,
    );
}

#[test]
fn reserved_keys() {
    run(
        "reserved_keys",
        Versions::From1_0,
        PROCESSOR,
        &[],
        process_case,
    );
}

/// The local functions the reference harness registers for the action cases:
/// `incrementCounter` adds `step` (default 1) to the number at `path` (default
/// `/counter`); `failingAction` always fails.
fn action_runtime() -> agent86_ui::runtime::Runtime {
    use agent86_ui::{Callers, FunctionDef};
    let mut runtime = agent86_ui::runtime::Runtime::new([basic_alias()]).unwrap();
    let def = FunctionDef {
        return_type: "void".into(),
        callers: Callers::RendererOnly,
        requires_user_activation: false,
    };
    runtime.register(
        "incrementCounter",
        def.clone(),
        std::sync::Arc::new(|args, scope| {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("/counter");
            let step = args.get("step").and_then(Value::as_f64).unwrap_or(1.0);
            let current = scope
                .get(path)
                .as_ref()
                .map_or(0.0, agent86_ui::runtime::format::to_number);
            scope.set(
                path,
                agent86_ui::runtime::expr::number_value(current + step),
            );
            Ok(Value::Null)
        }),
    );
    runtime.register(
        "failingAction",
        def,
        std::sync::Arc::new(|_, _| Err("Action function failed".into())),
    );
    runtime
}

fn dispatch_case(case: &Value) -> Result<(), String> {
    known_keys(
        case,
        &[
            "action",
            "protocolVersion",
            "actionPayload",
            "dataModel",
            "scope",
            "surfaceId",
            "sourceComponentId",
            "userActivation",
            "metadata",
            "expectDispatched",
            "expectDataModel",
            "expectError",
        ],
        "case",
    )?;
    let mut runtime = action_runtime();
    let surface = case
        .get("surfaceId")
        .and_then(Value::as_str)
        .unwrap_or("main");
    let report = runtime.apply(&[json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": surface,
            "catalogId": "basic",
            "dataModel": case.get("dataModel").cloned().unwrap_or_else(|| json!({}))
        }
    })]);
    assert!(report.is_ok(), "{report:?}");
    let scope = case.get("scope").and_then(Value::as_str).unwrap_or("/");
    let index = scope
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<usize>().ok());
    let mut gesture = agent86_ui::runtime::Gesture::new(
        surface,
        case.get("sourceComponentId")
            .and_then(Value::as_str)
            .unwrap_or("button"),
    );
    gesture.data_path = scope.into();
    gesture.index = index;
    gesture.activation = case
        .get("userActivation")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    gesture.metadata = case.get("metadata").cloned();
    gesture.timestamp = Some("2026-11-22T10:00:00Z".into());

    let outcome = runtime.dispatch(&case["actionPayload"], &gesture);
    match (outcome, case.get("expectError")) {
        (Err(error), Some(expected)) => {
            known_keys(
                expected,
                &["code", "surfaceId", "message", "category"],
                "expectError",
            )?;
            if let Some(code) = expected.get("code").and_then(Value::as_str) {
                if error.code != code {
                    return Err(format!("error code {}, expected {code}", error.code));
                }
            }
            if let Some(category) = expected.get("category").and_then(Value::as_str) {
                let mapped = if error.code == "VALIDATION_FAILED" {
                    "ValidationError"
                } else {
                    "?"
                };
                if mapped != category {
                    return Err(format!("error {} is not a {category}", error.code));
                }
            }
            if let Some(id) = expected.get("surfaceId").and_then(Value::as_str) {
                if error.surface_id != id {
                    return Err(format!("error on {}, expected {id}", error.surface_id));
                }
            }
            if let Some(message) = expected.get("message").and_then(Value::as_str) {
                if !error.message.contains(message) {
                    return Err(format!(
                        "error {:?} does not mention {message:?}",
                        error.message
                    ));
                }
            }
        }
        (Err(error), None) => return Err(format!("unexpected error {error}")),
        (Ok(dispatch), Some(expected)) => {
            return Err(format!("expected {expected}, got {dispatch:?}"))
        }
        (Ok(dispatch), None) => {
            let sent = dispatch.action.map(|a| serde_json::to_value(a).unwrap());
            match (case.get("expectDispatched"), &sent) {
                (Some(Value::Null), Some(sent)) => {
                    return Err(format!("expected nothing sent, got {sent}"))
                }
                (Some(Value::Object(want)), None) => {
                    return Err(format!("expected {want:?}, nothing sent"))
                }
                (Some(Value::Object(want)), Some(sent)) => {
                    for (key, expected) in want {
                        if key == "timestampIso8601" {
                            let ok = sent["timestamp"]
                                .as_str()
                                .is_some_and(|t| t.parse::<jiff::Timestamp>().is_ok());
                            if !ok {
                                return Err(format!(
                                    "timestamp {} is not ISO 8601",
                                    sent["timestamp"]
                                ));
                            }
                            continue;
                        }
                        let got = sent.get(key).cloned().unwrap_or(Value::Null);
                        if got != *expected {
                            return Err(format!("dispatched {key} is {got}, expected {expected}"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(model) = case.get("expectDataModel") {
        let got = runtime.data_model(surface);
        if got != Some(model) {
            return Err(format!("data model {got:?}, expected {model}"));
        }
    }
    Ok(())
}

#[test]
fn actions() {
    run(
        "actions",
        Versions::Only1_0,
        &["dispatch_action"],
        &[],
        dispatch_case,
    );
}
