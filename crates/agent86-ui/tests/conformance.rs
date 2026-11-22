//! The A2UI 1.0 specification's own tests, run against this crate.
//!
//! `tests/fixtures/v1_0/` holds the specification's test cases, its testing
//! catalog, the example stream, and the basic catalog's examples, copied from the
//! upstream repository at the commit in `spec/UPSTREAM.md`. The upstream runner
//! (`specification/v1_0/test/run_tests.py`) checks each case with ajv
//! (draft 2020-12, formats asserted); this file replays the same cases through
//! [`SchemaSet`] and the crate's other checks, and every expected verdict must
//! match. Refresh the fixtures with the schemas, never one without the other.

use agent86_ui::{
    check_agent_capabilities, check_catalog_definition, check_renderer_capabilities,
    check_renderer_data_model, Catalog, SchemaError, SchemaSet, BASIC_CATALOG_ID,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1_0")
}

fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn testing_catalog() -> Value {
    read(&fixtures().join("testing_catalog.json"))
}

/// The catalog a suite names (`"catalog"`, relative to the upstream test
/// directory), or the basic catalog.
fn suite_catalog(suite: &Value) -> Value {
    match suite.get("catalog").and_then(Value::as_str) {
        None => Catalog::basic().document().clone(),
        Some("testing_catalog.json") => testing_catalog(),
        Some(other) => panic!("suite names a catalog this harness does not carry: {other}"),
    }
}

fn verdict(result: Result<(), Vec<SchemaError>>) -> (bool, String) {
    match result {
        Ok(()) => (true, String::new()),
        Err(errors) => (
            false,
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ),
    }
}

#[test]
fn every_specification_test_case_gets_the_expected_verdict() {
    let mut suites: Vec<PathBuf> = std::fs::read_dir(fixtures().join("cases"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    suites.sort();
    assert_eq!(suites.len(), 17, "the specification ships 17 suites");

    let mut mismatches = Vec::new();
    let mut total = 0;
    for path in &suites {
        let suite = read(path);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let schema = suite
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or("agent_to_renderer.json")
            .to_string();
        let set = SchemaSet::for_document(&suite_catalog(&suite)).unwrap();
        for (index, case) in suite["tests"].as_array().unwrap().iter().enumerate() {
            total += 1;
            let data = &case["data"];
            let expected = case.get("valid").and_then(Value::as_bool).unwrap_or(true);
            let (valid, detail) = verdict(match schema.as_str() {
                "agent_to_renderer.json" => set.check_agent_message(data),
                "renderer_to_agent.json" => set.check_renderer_message(data),
                "catalog_definition.json" => check_catalog_definition(data),
                "agent_capabilities.json" => check_agent_capabilities(data),
                other => panic!("{name}: unknown schema {other}"),
            });
            if valid != expected {
                mismatches.push(format!(
                    "{name} #{index} {:?}: expected valid={expected}, got valid={valid} {detail}",
                    case.get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                ));
            }
        }
    }
    assert_eq!(total, 178, "the specification's suites hold 178 cases");
    assert!(
        mismatches.is_empty(),
        "{} of {total} cases disagree with the specification:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[test]
fn the_specifications_example_stream_is_valid() {
    let text =
        std::fs::read_to_string(fixtures().join("cases/contact_form_example.jsonl")).unwrap();
    let set = SchemaSet::for_catalog(Catalog::basic()).unwrap();
    let mut lines = 0;
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let message: Value = serde_json::from_str(line).unwrap();
        if let Err(errors) = set.check_agent_message(&message) {
            panic!("line {}: {errors:?}", number + 1);
        }
    }
    assert!(lines > 0);
}

#[test]
fn every_basic_catalog_example_is_valid() {
    let set = SchemaSet::for_catalog(Catalog::basic()).unwrap();
    let mut examples: Vec<PathBuf> = std::fs::read_dir(fixtures().join("examples"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    examples.sort();
    assert_eq!(examples.len(), 43, "the basic catalog ships 43 examples");
    let mut failures = Vec::new();
    for path in &examples {
        let example = read(path);
        for (index, message) in example["messages"].as_array().unwrap().iter().enumerate() {
            if let Err(errors) = set.check_agent_message(message) {
                failures.push(format!(
                    "{} message {index}: {errors:?}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_specifications_catalogs_load() {
    // The upstream runner checks each catalog against `catalog_definition.json`,
    // its names against UAX #31, and its `$ref` targets; `Catalog::from_value`
    // does all three.
    assert_eq!(Catalog::basic().id(), BASIC_CATALOG_ID);
    let testing = Catalog::from_value(testing_catalog()).unwrap();
    assert_eq!(
        testing.id(),
        "https://a2ui.org/specification/v1_0/testing_catalog.json"
    );
}

#[test]
fn the_a2a_payloads_the_specification_checks_are_valid() {
    // The payloads of the upstream runner's `validate_a2a_schemas`.
    check_renderer_capabilities(&json!({
        "1.0": {"supportedCatalogIds": [BASIC_CATALOG_ID], "inlineCatalogs": []}
    }))
    .unwrap();
    check_agent_capabilities(&json!({
        "1.0": {"supportedCatalogIds": [BASIC_CATALOG_ID], "acceptsInlineCatalogs": true}
    }))
    .unwrap();
    check_renderer_data_model(&json!({
        "version": "1.0",
        "surfaces": {"surface_123": {"user": {"name": "Alice"}}}
    }))
    .unwrap();
    let set = SchemaSet::for_catalog(Catalog::basic()).unwrap();
    set.check_agent_message(&json!({
        "version": "1.0",
        "createSurface": {"surfaceId": "test_surface", "catalogId": BASIC_CATALOG_ID}
    }))
    .unwrap();
    set.check_renderer_message(&json!({
        "version": "1.0",
        "action": {
            "name": "click_button",
            "surfaceId": "test_surface",
            "sourceComponentId": "btn_1",
            "timestamp": "2026-06-22T17:00:00Z",
            "context": {}
        }
    }))
    .unwrap();
}

#[test]
fn capabilities_without_the_version_key_are_refused() {
    assert!(check_renderer_capabilities(&json!({"supportedCatalogIds": []})).is_err());
    assert!(check_renderer_data_model(&json!({"surfaces": {}})).is_err());
}
