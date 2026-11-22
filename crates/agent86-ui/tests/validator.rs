//! `Validator`: the schema check plus the rules a schema cannot express, over
//! batches of messages and the surfaces they build.

use agent86_ui::{
    AgentMessageKind, Catalog, Code, Problem, Report, Rules, Validator, BASIC_CATALOG_ID,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn create(surface: &str, components: Value) -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": surface,
            "catalogId": BASIC_CATALOG_ID,
            "components": components
        }
    })
}

fn update(surface: &str, components: Value) -> Value {
    json!({"version": "1.0", "updateComponents": {"surfaceId": surface, "components": components}})
}

fn text(id: &str, value: &str) -> Value {
    json!({"id": id, "component": "Text", "text": value})
}

fn codes(problems: &[Problem]) -> Vec<(Code, String)> {
    problems.iter().map(|p| (p.code, p.path.clone())).collect()
}

fn only_error(report: &Report) -> &Problem {
    assert_eq!(report.errors.len(), 1, "{:#?}", report.errors);
    &report.errors[0]
}

#[test]
fn every_basic_catalog_example_passes_the_rules_too() {
    // Each upstream example is one batch to a fresh renderer. They are the
    // specification's own idea of valid UI, so none may fail a rule here,
    // strict but for orphans: 1.0 has no message that removes a component, so
    // replacing a placeholder (`incremental-dashboard`) leaves it orphaned.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1_0/examples");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    let mut failures = Vec::new();
    for path in paths {
        let example: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let messages = example["messages"].as_array().unwrap();
        let mut validator = Validator::basic();
        validator.set_rules(Rules::STRICT.allowing_orphans(true));
        let report = validator.apply(messages);
        if !report.is_ok() {
            failures.push(format!("{}: {:#?}", path.display(), report.errors));
        }
        assert_eq!(report.is_ok(), report.messages.len() == messages.len());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_valid_batch_is_typed_and_recorded() {
    let mut validator = Validator::basic();
    let report = validator.apply(&[create(
        "s1",
        json!([
            {"id": "root", "component": "Column", "children": ["title"]},
            text("title", "Hello")
        ]),
    )]);
    assert!(report.is_ok(), "{:#?}", report.errors);
    assert!(report.warnings.is_empty(), "{:#?}", report.warnings);
    assert!(matches!(
        report.messages[0].kind,
        AgentMessageKind::CreateSurface(_)
    ));
    assert_eq!(validator.surface_ids().collect::<Vec<_>>(), ["s1"]);

    // The next batch is checked against what the first created.
    let report = validator.apply(&[update("s1", json!([text("title", "Hi")]))]);
    assert!(report.is_ok(), "{:#?}", report.errors);
}

#[test]
fn a_surface_cannot_be_created_twice_or_used_before_it_exists() {
    let mut validator = Validator::basic();
    validator.apply(&[create("s1", json!([text("root", "a")]))]);

    let report = validator.check(&[create("s1", json!([text("root", "b")]))]);
    let error = only_error(&report);
    assert_eq!(error.code, Code::IntegrityError);
    assert_eq!(error.path, "/0/createSurface/surfaceId");

    let report = validator.check(&[update("nope", json!([text("root", "b")]))]);
    assert_eq!(only_error(&report).code, Code::IntegrityError);

    let report = validator.check(&[json!({
        "version": "1.0", "updateDataModel": {"surfaceId": "nope", "value": {}}
    })]);
    assert_eq!(only_error(&report).code, Code::IntegrityError);

    // Deleting frees the id; a later message in the same batch sees the delete.
    let report = validator.apply(&[
        json!({"version": "1.0", "deleteSurface": {"surfaceId": "s1"}}),
        create("s1", json!([text("root", "again")])),
    ]);
    assert!(report.is_ok(), "{:#?}", report.errors);
}

#[test]
fn a_batch_with_an_error_changes_nothing() {
    let mut validator = Validator::basic();
    let report = validator.apply(&[
        create("s1", json!([text("root", "fine")])),
        update("missing", json!([text("root", "x")])),
    ]);
    assert!(!report.is_ok());
    assert!(report.messages.is_empty());
    assert_eq!(validator.surface_ids().count(), 0);

    // `check` never records, even when the batch is valid.
    assert!(validator
        .check(&[create("s2", json!([text("root", "x")]))])
        .is_ok());
    assert_eq!(validator.surface_ids().count(), 0);
}

#[test]
fn an_unsupported_catalog_is_named_with_the_supported_ones() {
    let report = Validator::basic().check(&[json!({
        "version": "1.0",
        "createSurface": {"surfaceId": "s", "catalogId": "example.com:other",
                          "components": [text("root", "x")]}
    })]);
    let error = only_error(&report);
    assert_eq!(error.path, "/0/createSurface/catalogId");
    assert!(
        error.message.contains(BASIC_CATALOG_ID),
        "{}",
        error.message
    );
}

#[test]
fn a_component_with_no_catalog_is_refused() {
    let report = Validator::basic().check(&[json!({
        "version": "1.0",
        "createSurface": {"surfaceId": "s", "components": [text("root", "x")]}
    })]);
    let error = only_error(&report);
    assert_eq!(error.path, "/0/createSurface/components/0");
    assert!(error.message.contains("no catalog"));
}

#[test]
fn component_ids_must_be_identifiers_and_unique_in_a_message() {
    let report = Validator::basic().check(&[create(
        "s",
        json!([
            {"id": "root", "component": "Column", "children": ["user-card", "a"]},
            text("user-card", "x"),
            text("a", "1"),
            text("a", "2")
        ]),
    )]);
    assert_eq!(
        codes(&report.errors),
        [
            (
                Code::ValidationFailed,
                "/0/createSurface/components/1/id".to_string()
            ),
            // A duplicate id is an integrity error, as the reference SDKs
            // report it.
            (
                Code::IntegrityError,
                "/0/createSurface/components/3/id".to_string()
            ),
        ]
    );
    assert!(report.errors[0]
        .message
        .starts_with("Component id 'user-card' must be a valid UAX #31 identifier"));
    assert!(report.errors[1]
        .message
        .starts_with("Duplicate component ID 'a'"));
}

#[test]
fn a_schema_error_points_into_the_batch() {
    // A Text with no `text` fails the catalog's schema for the second component.
    let report = Validator::basic().check(&[
        create("ok", json!([text("root", "x")])),
        create(
            "s",
            json!([text("root", "x"), {"id": "bad", "component": "Text"}]),
        ),
    ]);
    // The catalog's oneOf over component types is narrowed to Text's schema, so
    // the model hears what Text is missing, not that nothing matched.
    let error = only_error(&report);
    assert_eq!(error.path, "/1/createSurface/components/1");
    assert_eq!(error.message, "\"text\" is a required property of Text");

    // The same for a function call: the catalog's oneOf over functions is
    // narrowed to `required`'s schema.
    let report = Validator::basic().check(&[create(
        "s",
        json!([{"id": "root", "component": "TextField", "label": "Name",
                "value": {"@path": "/name"},
                "@checks": [{"condition": {"@call": "required"}}]}]),
    )]);
    let error = only_error(&report);
    assert_eq!(
        error.path,
        "/0/createSurface/components/0/@checks/0/condition"
    );
    assert_eq!(error.message, "\"args\" is a required property");

    // A property the type does not have is still named.
    let report = Validator::basic().check(&[create(
        "s",
        json!([{"id": "root", "component": "Text", "text": "hi", "colour": "red"}]),
    )]);
    let error = only_error(&report);
    assert_eq!(error.path, "/0/createSurface/components/0");
    assert!(error.message.contains("'colour'"), "{}", error.message);

    // A value of the wrong shape lists the forms the field allows.
    let report = Validator::basic().check(&[create(
        "s",
        json!([{"id": "root", "component": "Text", "text": {"path": "/name"}}]),
    )]);
    let error = only_error(&report);
    assert_eq!(error.path, "/0/createSurface/components/0/text");
    assert!(
        error
            .message
            .starts_with("matches none of the forms allowed here"),
        "{}",
        error.message
    );
    assert!(error.message.contains("\"@path\""), "{}", error.message);
}

#[test]
fn missing_children_a_missing_root_and_hidden_components_are_errors_when_strict() {
    let batch = [create(
        "s",
        json!([
            {"id": "root", "component": "Column", "children": ["later"]},
            text("orphan", "nobody shows me")
        ]),
    )];
    let report = Validator::basic().check(&batch);
    assert_eq!(Validator::basic().rules(), Rules::STRICT);
    let messages: Vec<&str> = report.errors.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(report.errors.len(), 2, "{messages:#?}");
    assert!(messages[0]
        .starts_with("Dangling reference: root references non-existent component 'later'"));
    assert_eq!(report.errors[0].path, "/0/createSurface/components/0");
    assert!(messages[1].starts_with("Component 'orphan' is not reachable from 'root'"));
    assert_eq!(report.errors[1].path, "/0/createSurface/components/1");

    let report = Validator::basic().check(&[create("t", json!([text("title", "no root")]))]);
    let error = only_error(&report);
    assert!(
        error.message.starts_with("Missing root component"),
        "{}",
        error.message
    );

    // A surface created with no components yet is fine either way.
    let report = Validator::basic().check(&[create("u", json!([]))]);
    assert!(report.is_ok() && report.warnings.is_empty(), "{report:#?}");
}

#[test]
fn relaxed_rules_make_them_warnings() {
    let mut validator = Validator::basic();
    validator.set_rules(Rules::RELAXED);
    let report = validator.apply(&[create(
        "s",
        json!([
            {"id": "root", "component": "Column", "children": ["later"]},
            text("orphan", "nobody shows me")
        ]),
    )]);
    assert!(report.is_ok(), "{:#?}", report.errors);
    let messages: Vec<&str> = report.warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(report.warnings.len(), 2, "{messages:#?}");
    assert!(messages
        .iter()
        .any(|m| m.contains("'later'") && m.contains("placeholder")));
    assert!(messages.iter().any(|m| m.contains("orphan")));
    assert_eq!(report.warnings[0].path, "/0/createSurface/components/0");

    let report = validator.check(&[create("t", json!([text("title", "no root")]))]);
    assert!(report.is_ok());
    assert!(report.warnings[0].message.contains("none with id \"root\""));

    // Each rule on its own: only orphans allowed leaves the others errors.
    let mut validator = Validator::basic();
    validator.set_rules(Rules::STRICT.allowing_orphans(true));
    let report = validator.check(&[create(
        "s",
        json!([
            {"id": "root", "component": "Column", "children": ["later"]},
            text("orphan", "nobody shows me")
        ]),
    )]);
    assert!(only_error(&report)
        .message
        .starts_with("Dangling reference"));
    assert_eq!(report.warnings.len(), 1);
}

#[test]
fn a_cycle_is_an_error() {
    let report = Validator::basic().check(&[create(
        "s",
        json!([
            {"id": "root", "component": "Column", "children": ["a"]},
            {"id": "a", "component": "Card", "child": "b"},
            {"id": "b", "component": "Card", "child": "a"}
        ]),
    )]);
    let error = only_error(&report);
    assert_eq!(error.code, Code::ValidationFailed);
    assert!(error.message.contains("a -> b -> a"), "{}", error.message);
}

#[test]
fn a_function_that_needs_a_gesture_may_only_be_an_actions_function_call() {
    let button = |action: Value| {
        json!([
            {"id": "root", "component": "Button", "child": "label", "action": action},
            {"id": "label", "component": "Text",
             "text": {"@call": "openUrl", "args": {"url": "https://example.com"}}}
        ])
    };
    // openUrl in a binding (the label's text): refused.
    let report = Validator::basic().check(&[create("s", button(json!({"event": {"name": "go"}})))]);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.path == "/0/createSurface/components/1/text"
                && e.message.contains("user gesture")),
        "{:#?}",
        report.errors
    );

    // openUrl as the button's action: fine.
    let report = Validator::basic().check(&[create(
        "s",
        json!([
            {"id": "root", "component": "Button", "child": "label",
             "action": {"functionCall": {"@call": "openUrl", "args": {"url": "https://example.com"}}}},
            text("label", "Open")
        ]),
    )]);
    assert!(report.is_ok(), "{:#?}", report.errors);
}

#[test]
fn the_agent_may_not_call_a_renderer_only_function() {
    let mut validator = Validator::basic();
    validator.apply(&[create("s", json!([text("root", "x")]))]);
    let report = validator.check(&[json!({
        "version": "1.0",
        "callRendererFunction": {
            "functionCallId": "c1",
            "callFunction": {"@call": "required", "catalogId": BASIC_CATALOG_ID,
                             "args": {"value": "x"}}
        }
    })]);
    let error = only_error(&report);
    assert_eq!(error.code, Code::InvalidFunctionCall);
    assert!(error.message.contains("rendererOnly"));
}

#[test]
fn a_message_must_be_exactly_one_kind() {
    let report = Validator::basic().check(&[json!({"version": "1.0", "hello": {}})]);
    assert!(!report.is_ok());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.message.contains("exactly one of")),
        "{:#?}",
        report.errors
    );
}

#[test]
fn a_data_model_path_must_be_a_json_pointer() {
    let mut validator = Validator::basic();
    validator.apply(&[create("s", json!([text("root", "x")]))]);
    let report = validator.check(&[json!({
        "version": "1.0", "updateDataModel": {"surfaceId": "s", "path": "user/name", "value": "x"}
    })]);
    assert_eq!(only_error(&report).path, "/0/updateDataModel/path");
    let report = validator.check(&[json!({
        "version": "1.0", "updateDataModel": {"surfaceId": "s", "path": "/user/name", "value": null}
    })]);
    assert!(report.is_ok(), "{:#?}", report.errors);
}

/// A catalog with composition constraints: `Menu` holds only `MenuItem`s, a
/// `MenuItem` lives only in a `Menu`, and `Shell` may only be the root.
fn layout_catalog() -> Catalog {
    let component = |name: &str, extra: Value| {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "component": {"const": name},
                "children": {"$ref": "common_types.json#/$defs/ChildList"},
                "label": {"$ref": "common_types.json#/$defs/DynamicString"}
            },
            "required": ["component"]
        });
        for (key, value) in extra.as_object().unwrap() {
            schema[key] = value.clone();
        }
        schema
    };
    Catalog::from_value(json!({
        "protocolVersion": "1.0",
        "catalogId": "example.com:layout",
        "components": {
            "Shell": component("Shell", json!({"allowedParents": ["Surface"]})),
            "Menu": component("Menu", json!({"allowedChildren": ["MenuItem"]})),
            "MenuItem": component("MenuItem", json!({"allowedParents": ["Menu"]})),
            "Box": component("Box", json!({}))
        },
        "$defs": {
            "anyComponent": {"oneOf": [
                {"$ref": "#/components/Shell"}, {"$ref": "#/components/Menu"},
                {"$ref": "#/components/MenuItem"}, {"$ref": "#/components/Box"}
            ]},
            "anyFunction": false
        }
    }))
    .unwrap()
}

fn layout_create(components: Value) -> Value {
    json!({"version": "1.0", "createSurface": {
        "surfaceId": "s", "catalogId": "example.com:layout", "components": components
    }})
}

#[test]
fn composition_constraints_are_enforced() {
    let validator = Validator::new([layout_catalog()]).unwrap();

    let fine = validator.check(&[layout_create(json!([
        {"id": "root", "component": "Shell", "children": ["menu"]},
        {"id": "menu", "component": "Menu", "children": ["item"]},
        {"id": "item", "component": "MenuItem"}
    ]))]);
    assert!(fine.is_ok(), "{:#?}", fine.errors);

    // A Box inside a Menu breaks the Menu's allowedChildren.
    let report = validator.check(&[layout_create(json!([
        {"id": "root", "component": "Menu", "children": ["box"]},
        {"id": "box", "component": "Box"}
    ]))]);
    assert_eq!(
        codes(&report.errors),
        [(
            Code::UnallowedChild,
            "/0/createSurface/components/0".to_string()
        )]
    );

    // A MenuItem in a Box breaks the MenuItem's allowedParents.
    let report = validator.check(&[layout_create(json!([
        {"id": "root", "component": "Box", "children": ["item"]},
        {"id": "item", "component": "MenuItem"}
    ]))]);
    assert_eq!(
        codes(&report.errors),
        [(
            Code::UnallowedParent,
            "/0/createSurface/components/1".to_string()
        )]
    );

    // A Shell anywhere but the root, and a MenuItem as the root.
    let report = validator.check(&[layout_create(json!([
        {"id": "root", "component": "Box", "children": ["shell"]},
        {"id": "shell", "component": "Shell"}
    ]))]);
    assert_eq!(only_error(&report).code, Code::UnallowedParent);
    let report = validator.check(&[layout_create(
        json!([{"id": "root", "component": "MenuItem"}]),
    )]);
    let error = only_error(&report);
    assert_eq!(error.code, Code::UnallowedParent);
    assert!(error.message.contains("cannot be the root"));
}

#[test]
fn a_surface_can_mix_catalogs_and_errors_keep_their_indices() {
    let validator = Validator::new([Catalog::basic().clone(), layout_catalog()]).unwrap();
    let report = validator.check(&[create(
        "s",
        json!([
            {"id": "root", "component": "Column", "children": ["menu", "title"]},
            {"id": "menu", "component": "Menu", "@catalogId": "example.com:layout",
             "children": ["item"]},
            {"id": "item", "component": "MenuItem", "@catalogId": "example.com:layout"},
            text("title", "Mixed")
        ]),
    )]);
    assert!(report.is_ok(), "{:#?}", report.errors);

    // A layout component under the basic default with no @catalogId fails the
    // basic catalog's schema, at its own index.
    let report = validator.check(&[create(
        "s",
        json!([
            {"id": "root", "component": "Column", "children": ["menu", "box"]},
            {"id": "menu", "component": "Menu", "@catalogId": "example.com:layout"},
            {"id": "box", "component": "Box"}
        ]),
    )]);
    assert!(!report.is_ok());
    assert!(
        report
            .errors
            .iter()
            .all(|e| e.path.starts_with("/0/createSurface/components/2")),
        "{:#?}",
        report.errors
    );
}
