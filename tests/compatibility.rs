use agent86::types::{
    AgentPhase, CompletionDelta, Message, ModelRef, Role, StopReason, ToolCall, ToolResult,
    ToolSpec, Usage, INVALID_TOOL_ARGS_KEY,
};
use serde_json::{json, Map, Value};

#[test]
fn enums_use_the_python_wire_vocabulary() {
    assert_eq!(
        serde_json::to_value(Role::Assistant).unwrap(),
        json!("assistant")
    );
    assert_eq!(
        serde_json::to_value(AgentPhase::Execute).unwrap(),
        json!("execute")
    );
    assert_eq!(
        serde_json::to_value(StopReason::EndTurn).unwrap(),
        json!("end_turn")
    );
    assert_eq!(
        serde_json::to_value(StopReason::ToolUse).unwrap(),
        json!("tool_use")
    );
    assert_eq!(
        serde_json::to_value(StopReason::MaxTokens).unwrap(),
        json!("max_tokens")
    );
}

#[test]
fn model_refs_split_only_on_the_first_colon() {
    let model = ModelRef::parse(" openrouter:anthropic/claude-3.7-sonnet ").unwrap();
    assert_eq!(model.provider, "openrouter");
    assert_eq!(model.model, "anthropic/claude-3.7-sonnet");
    assert_eq!(model.to_string(), "openrouter:anthropic/claude-3.7-sonnet");

    let with_port = ModelRef::parse("local:model:variant").unwrap();
    assert_eq!(with_port.model, "model:variant");
}

#[test]
fn malformed_model_refs_are_rejected() {
    for reference in ["gpt-4o", ":model", "provider:", " : ", ""] {
        assert!(
            ModelRef::parse(reference).is_err(),
            "accepted {reference:?}"
        );
    }
}

#[test]
fn usage_addition_includes_cached_tokens_and_cost() {
    let first = Usage {
        input_tokens: 100,
        output_tokens: 20,
        cost_usd: 0.01,
        cache_read_tokens: 40,
        cache_creation_tokens: 10,
    };
    let second = Usage {
        input_tokens: 50,
        output_tokens: 5,
        cost_usd: 0.005,
        cache_read_tokens: 20,
        cache_creation_tokens: 0,
    };
    let total = first + second;

    assert_eq!(total.input_tokens, 150);
    assert_eq!(total.output_tokens, 25);
    assert_eq!(total.cache_read_tokens, 60);
    assert_eq!(total.cache_creation_tokens, 10);
    assert!((total.cost_usd - 0.015).abs() < f64::EPSILON);
}

#[test]
fn invalid_tool_arguments_remain_available_for_structured_errors() {
    let mut arguments = Map::new();
    arguments.insert(INVALID_TOOL_ARGS_KEY.to_owned(), json!("{not json"));
    let call = ToolCall {
        id: "call-1".to_owned(),
        name: "read_file".to_owned(),
        arguments,
    };

    assert_eq!(call.invalid_arguments(), Some("{not json"));
}

#[test]
fn tool_specs_start_with_an_empty_object_schema() {
    let spec = ToolSpec::new("list_dir", "List workspace entries");
    assert_eq!(spec.parameters, json!({"type": "object", "properties": {}}));
    assert!(!spec.side_effecting);
}

#[test]
fn messages_preserve_provider_neutral_tool_call_json() {
    let message = Message {
        role: Role::Assistant,
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: "call-1".to_owned(),
            name: "read_file".to_owned(),
            arguments: Map::from_iter([("path".to_owned(), json!("README.md"))]),
        }],
        tool_call_id: None,
        name: None,
    };

    let encoded = serde_json::to_value(&message).unwrap();
    assert_eq!(
        encoded,
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call-1",
                "name": "read_file",
                "arguments": {"path": "README.md"}
            }]
        })
    );

    let decoded: Message = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, message);
}

#[test]
fn tool_result_defaults_match_the_python_contract() {
    let result: ToolResult = serde_json::from_value(json!({
        "call_id": "call-1",
        "name": "list_dir"
    }))
    .unwrap();

    assert!(result.ok);
    assert_eq!(result.content, "");
    assert_eq!(result.error, None);
}

#[test]
fn final_completion_delta_round_trips() {
    let delta = CompletionDelta {
        text: String::new(),
        done: true,
        completion: Some(agent86::types::Completion {
            text: "Done".to_owned(),
            stop_reason: Some("end_turn".to_owned()),
            ..Default::default()
        }),
    };

    let encoded = serde_json::to_string(&delta).unwrap();
    let decoded: CompletionDelta = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, delta);
    assert_eq!(decoded.completion.unwrap().usage, Usage::default());
}

#[test]
fn usage_deserializes_missing_counters_as_zero() {
    let usage: Usage = serde_json::from_value(Value::Null).unwrap_or_default();
    assert_eq!(usage, Usage::default());

    let usage: Usage = serde_json::from_value(json!({"input_tokens": 12})).unwrap();
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 0);
}
