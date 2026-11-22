//! The headless runtime through its public API: what a renderer built on it
//! sees, beyond what the shared conformance suites check.

use agent86_ui::runtime::{Effect, Gesture, Runtime};
use agent86_ui::{Code, BASIC_CATALOG_ID};
use serde_json::{json, Value};

fn create(components: Value, data: Value) -> Value {
    json!({"version": "1.0", "createSurface": {
        "surfaceId": "s", "catalogId": BASIC_CATALOG_ID,
        "components": components, "dataModel": data
    }})
}

fn form() -> Runtime {
    let mut runtime = Runtime::basic();
    let report = runtime.apply(&[create(
        json!([
            {"id": "root", "component": "Column", "children": ["name", "list", "send"]},
            {"id": "name", "component": "TextField", "label": "Name", "value": {"@path": "/form/name"},
             "@checks": [{"condition": {"@call": "required", "args": {"value": {"@path": "/form/name"}}},
                          "message": "Name, please."}]},
            {"id": "list", "component": "List", "children": {"path": "/items", "componentId": "item"}},
            {"id": "item", "component": "Button", "child": "item_label",
             "action": {"event": {"name": "pick", "context": {"title": {"@path": "title"},
                                                             "n": {"@call": "@index", "args": {"offset": 1}}}}}},
            {"id": "item_label", "component": "Text",
             "text": {"@call": "formatString", "args": {"value": "${@index(offset: 1)}. ${title}"}}},
            {"id": "send", "component": "Button", "child": "send_label",
             "action": {"event": {"name": "send", "context": {"name": {"@path": "/form/name"}}}},
             "@checks": [{"condition": {"@call": "required", "args": {"value": {"@path": "/form/name"}}}}]},
            {"id": "send_label", "component": "Text", "text": "Send"}
        ]),
        json!({"form": {"name": ""}, "items": [{"title": "Tea"}, {"title": "Coffee"}]}),
    )]);
    assert!(report.is_ok(), "{:#?}", report.errors);
    runtime
}

#[test]
fn the_tree_resolves_bindings_templates_and_checks() {
    let runtime = form();
    let tree = runtime.tree("s").unwrap();
    assert_eq!(tree.component, "Column");
    let (property, children) = &tree.children[0];
    assert_eq!(property, "children");
    assert_eq!(
        children.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        ["name", "list", "send"]
    );

    let name = &children[0];
    assert_eq!(name.properties["value"], json!(""));
    assert_eq!(name.properties["label"], json!("Name"));
    assert!(!name.is_valid());
    assert_eq!(name.validation_errors(), ["Name, please."]);

    // One Button per item, each with its own data context and label.
    let items = &children[1].children[0].1;
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].data_path, "/items/1");
    assert_eq!(items[1].index, Some(1));
    let label = &items[1].children[0].1[0];
    assert_eq!(label.properties["text"], json!("2. Coffee"));
    // The action is not resolved with the properties; it runs on a gesture.
    assert!(!items[1].properties.contains_key("action"));

    // The send button's check fails, so a renderer disables it.
    assert!(!children[2].is_valid());
}

#[test]
fn input_writes_back_and_checks_follow() {
    let mut runtime = form();
    runtime.input("s", "/form/name", json!("Ada")).unwrap();
    let tree = runtime.tree("s").unwrap();
    let children = &tree.children[0].1;
    assert_eq!(children[0].properties["value"], json!("Ada"));
    assert!(children[0].is_valid());
    assert!(children[2].is_valid());
}

#[test]
fn a_gesture_on_a_template_instance_sends_its_context() {
    let mut runtime = form();
    let tree = runtime.tree("s").unwrap();
    let coffee = tree.children[0].1[1].children[0].1[1].clone();
    let action = runtime
        .components("s")
        .unwrap()
        .find(|c| c["id"] == "item")
        .unwrap()["action"]
        .clone();
    let mut gesture = Gesture::on(&coffee, "s");
    gesture.timestamp = Some("2026-11-22T10:00:00Z".into());
    let dispatch = runtime.dispatch(&action, &gesture).unwrap();
    let sent = dispatch.action.unwrap();
    assert_eq!(sent.name, "pick");
    assert_eq!(sent.source_component_id, "item");
    assert_eq!(sent.context["title"], json!("Coffee"));
    assert_eq!(sent.context["n"], json!(2));
}

#[test]
fn open_url_needs_a_real_gesture_and_comes_back_as_an_effect() {
    let mut runtime = Runtime::basic();
    runtime.apply(&[create(
        json!([{"id": "root", "component": "Text", "text": "x"}]),
        json!({}),
    )]);
    let action =
        json!({"functionCall": {"@call": "openUrl", "args": {"url": "https://example.com/a"}}});
    let mut gesture = Gesture::new("s", "link");
    let dispatch = runtime.dispatch(&action, &gesture).unwrap();
    assert_eq!(
        dispatch.effects,
        [Effect::OpenUrl("https://example.com/a".into())]
    );
    assert!(dispatch.action.is_none());

    gesture.activation = false;
    let error = runtime.dispatch(&action, &gesture).unwrap_err();
    assert_eq!(error.code, "EXPRESSION_ERROR");

    let script =
        json!({"functionCall": {"@call": "openUrl", "args": {"url": "javascript:alert(1)"}}});
    assert!(runtime
        .dispatch(&script, &Gesture::new("s", "link"))
        .is_err());
}

#[test]
fn tabs_children_are_resolved() {
    let mut runtime = Runtime::basic();
    let report = runtime.apply(&[create(
        json!([
            {"id": "root", "component": "Tabs", "tabs": [
                {"title": "One", "child": "first"}, {"title": "Two", "child": "second"}]},
            {"id": "first", "component": "Text", "text": "1"},
            {"id": "second", "component": "Text", "text": "2"}
        ]),
        json!({}),
    )]);
    assert!(report.is_ok(), "{:#?}", report.errors);
    // Tabs' children are reachable, so nothing is reported unreachable.
    assert!(report.warnings.is_empty(), "{:#?}", report.warnings);
    let tree = runtime.tree("s").unwrap();
    let tabs = &tree.children[0];
    assert_eq!(tabs.0, "tabs");
    assert_eq!(
        tabs.1.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    // The titles stay in the property, resolved.
    assert_eq!(tree.properties["tabs"][1]["title"], json!("Two"));
}

#[test]
fn a_template_that_does_not_parse_rejects_the_batch() {
    let mut runtime = Runtime::basic();
    let report = runtime.apply(&[create(
        json!([{"id": "root", "component": "Text",
                "text": {"@call": "formatString", "args": {"value": "Hi ${/name"}}}]),
        json!({"name": "Ada"}),
    )]);
    assert!(!report.is_ok());
    assert_eq!(report.errors[0].code, Code::ExpressionError);
    assert!(
        report.errors[0].message.contains("Unclosed interpolation"),
        "{}",
        report.errors[0].message
    );
    // Nothing was applied.
    assert_eq!(runtime.surface_ids().count(), 0);
}

#[test]
fn a_bad_data_write_rejects_the_batch_and_leaves_the_model() {
    let mut runtime = Runtime::basic();
    runtime.apply(&[create(
        json!([{"id": "root", "component": "Text", "text": "x"}]),
        json!({"name": "Ada"}),
    )]);
    let report = runtime.apply(&[
        json!({"version": "1.0", "updateDataModel": {"surfaceId": "s", "path": "/age", "value": 36}}),
        json!({"version": "1.0", "updateDataModel": {"surfaceId": "s", "path": "/name/first", "value": "A"}}),
    ]);
    assert!(!report.is_ok());
    assert_eq!(report.errors[0].code, Code::DataError);
    assert_eq!(report.errors[0].path, "/1/updateDataModel/path");
    assert_eq!(runtime.data_model("s"), Some(&json!({"name": "Ada"})));
}

#[test]
fn null_deletes_and_send_data_model_is_reported() {
    let mut runtime = Runtime::basic();
    runtime.apply(&[json!({"version": "1.0", "createSurface": {
        "surfaceId": "s", "catalogId": BASIC_CATALOG_ID, "sendDataModel": true,
        "components": [{"id": "root", "component": "Text", "text": "x"}],
        "dataModel": {"a": 1, "b": 2}}})]);
    runtime.apply(&[json!({"version": "1.0", "updateDataModel": {"surfaceId": "s", "path": "/a", "value": null}})]);
    assert_eq!(
        runtime.renderer_data_model(),
        Some(json!({"version": "1.0", "surfaces": {"s": {"b": 2}}}))
    );
    agent86_ui::check_renderer_data_model(&runtime.renderer_data_model().unwrap()).unwrap();
}

#[test]
fn a_snapshot_redraws_every_surface_as_it_is() {
    let mut runtime = form();
    // Change it after creating it: a component replaced, the data written.
    let report = runtime.apply(&[
        json!({"version": "1.0", "updateComponents": {"surfaceId": "s", "components": [
            {"id": "send_label", "component": "Text", "text": "Send it"}
        ]}}),
        json!({"version": "1.0", "updateDataModel": {"surfaceId": "s", "path": "/form/name", "value": "Ada"}}),
    ]);
    assert!(report.is_ok(), "{:#?}", report.errors);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.len(), 1);
    let mut fresh = Runtime::basic();
    let report = fresh.apply(&snapshot);
    assert!(report.is_ok(), "{:#?}", report.errors);
    assert_eq!(fresh.tree("s"), runtime.tree("s"));
    assert_eq!(fresh.data_model("s"), runtime.data_model("s"));
    assert_eq!(
        snapshot[0]["createSurface"]["catalogId"],
        json!(BASIC_CATALOG_ID)
    );
}
