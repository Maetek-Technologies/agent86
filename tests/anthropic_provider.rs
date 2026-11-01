//! The Anthropic provider against the local mock server: the Messages API event
//! stream, tool use, cache-aware usage, the SDK's retries, and Python's messages.

mod support;

use agent86::config::{load_config, ConfigPaths, Overrides};
use agent86::provider::anthropic::{AnthropicConfig, AnthropicProvider};
use agent86::provider::{provider_for_ref_with_store, Provider, ProviderError};
use agent86::secrets::NoSecretStore;
use agent86::types::{
    CompletionDelta, CompletionRequest, Message, ModelRef, Role, ToolCall, ToolSpec,
};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;
use support::{fixture, MockServer, Reply};

const TEST_KEY: &str = "sk-ant-test-0123456789abcdefghij";

fn config(base_url: &str) -> AnthropicConfig {
    AnthropicConfig {
        base_url: Some(base_url.to_owned()),
        ..AnthropicConfig::default()
    }
}

fn provider(model: &str, config: AnthropicConfig) -> AnthropicProvider {
    AnthropicProvider::new(model, config, Some(TEST_KEY.to_owned())).unwrap()
}

fn message(role: Role, content: &str) -> Message {
    Message {
        role,
        content: content.to_owned(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}

fn request(prompt: &str) -> CompletionRequest {
    CompletionRequest {
        model: "anthropic:claude-sonnet-4-6".to_owned(),
        messages: vec![message(Role::User, prompt)],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: None,
        stream: true,
    }
}

fn status(status: u16, headers: Vec<(&'static str, String)>, body: &str) -> Reply {
    Reply::Status {
        status,
        headers,
        body: body.to_owned(),
    }
}

async fn drain(
    provider: &AnthropicProvider,
    request: CompletionRequest,
) -> (Vec<CompletionDelta>, Option<ProviderError>) {
    let mut stream = provider.stream(request);
    let mut deltas = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(delta) => deltas.push(delta),
            Err(error) => return (deltas, Some(error)),
        }
    }
    (deltas, None)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn thinking_blocks_stream_apart_from_the_text() {
    let server = MockServer::start(vec![Reply::Sse(fixture("anthropic/thinking.sse"))]).await;
    let provider = provider("claude-sonnet-4-6", config(&server.base_url));

    let (deltas, error) = drain(&provider, request("2+2?")).await;
    assert_eq!(error, None);
    let thinking: Vec<&str> = deltas
        .iter()
        .filter(|delta| !delta.thinking.is_empty())
        .map(|delta| delta.thinking.as_str())
        .collect();
    assert_eq!(thinking, ["Two and two", " is four."]);
    let completion = deltas.last().unwrap().completion.clone().unwrap();
    // The signature and the redacted block carry nothing readable.
    assert_eq!(completion.thinking, "Two and two is four.");
    assert_eq!(completion.text, "4");
}

#[tokio::test]
async fn streams_text_then_a_final_completion_with_cache_aware_usage() {
    let server = MockServer::start(vec![Reply::Sse(fixture("anthropic/text.sse"))]).await;
    let provider = provider("claude-sonnet-4-6", config(&server.base_url));

    let (deltas, error) = drain(&provider, request("Say hello")).await;
    assert_eq!(error, None);
    let texts: Vec<&str> = deltas
        .iter()
        .filter(|delta| !delta.done)
        .map(|delta| delta.text.as_str())
        .collect();
    assert_eq!(texts, ["Hello", ", world"]);
    let completion = deltas.last().unwrap().completion.clone().unwrap();
    assert_eq!(completion.text, "Hello, world");
    assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(completion.model.as_deref(), Some("claude-sonnet-4-6"));
    // input_tokens is the uncached remainder; Usage counts the whole prompt.
    let usage = &completion.usage;
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens
        ),
        (120, 3, 100)
    );
    // 20 fresh and 100 cached input tokens at $3/M (cache reads at a tenth), 3 out at $15/M.
    assert!(
        (usage.cost_usd - 0.000135).abs() < 1e-12,
        "{}",
        usage.cost_usd
    );

    let sent = &server.requests()[0];
    assert_eq!(sent.request_line, "POST /v1/messages HTTP/1.1");
    assert_eq!(header(&sent.headers, "x-api-key"), Some(TEST_KEY));
    assert_eq!(
        header(&sent.headers, "anthropic-version"),
        Some("2023-06-01")
    );
    assert_eq!(header(&sent.headers, "authorization"), None);
    assert_eq!(
        sent.body,
        json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "Say hello"}],
            "max_tokens": 8192,
            "temperature": 0.0,
            "stream": true,
        })
    );
}

fn conversation() -> CompletionRequest {
    let mut call = ToolCall {
        id: "toolu_01".to_owned(),
        name: "read_file".to_owned(),
        arguments: serde_json::Map::new(),
    };
    call.arguments.insert("path".to_owned(), json!("a.txt"));
    let mut asked = message(Role::Assistant, "Reading.");
    asked.tool_calls = vec![call];
    let mut answered = message(Role::Tool, "contents");
    answered.tool_call_id = Some("toolu_01".to_owned());
    answered.name = Some("read_file".to_owned());
    let mut request = request("Read a.txt");
    request
        .messages
        .insert(0, message(Role::System, "Be brief."));
    request.messages.extend([asked, answered]);
    request.tools = vec![ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file.".to_owned(),
        parameters: json!({"type": "object"}),
        side_effecting: false,
    }];
    request
}

#[test]
fn the_payload_splits_the_system_prompt_and_converts_tool_turns() {
    let provider = provider("claude-opus-4-8", AnthropicConfig::default());
    let mut request = conversation();
    request.max_tokens = Some(64);

    let payload = provider.payload(&request);

    assert_eq!(payload["system"], json!("Be brief."));
    assert_eq!(payload["max_tokens"], json!(64));
    assert_eq!(
        payload["messages"],
        json!([
            {"role": "user", "content": "Read a.txt"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Reading."},
                {"type": "tool_use", "id": "toolu_01", "name": "read_file", "input": {"path": "a.txt"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "contents"},
            ]},
        ])
    );
    assert_eq!(
        payload["tools"],
        json!([{"name": "read_file", "description": "Read a file.", "input_schema": {"type": "object"}}])
    );
    // Opus 4.8 removed temperature; it must be left out entirely.
    assert!(payload.get("temperature").is_none(), "{payload}");
}

#[test]
fn a_long_enough_prefix_is_marked_for_the_prompt_cache() {
    let mut request = conversation();
    // Opus 4.8 caches from 1024 tokens (about 4096 characters).
    let system = "Follow the rules. ".repeat(250);
    request.messages[0].content = system.clone();
    request.tools[0].description = "d".repeat(4200);

    let cached = provider("claude-opus-4-8", AnthropicConfig::default()).payload(&request);
    assert_eq!(
        cached["system"],
        json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(
        cached["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );

    // A short tool list gets no marker, but still counts toward the system's floor.
    request.tools[0].description = "d".repeat(2000);
    let partly = provider("claude-opus-4-8", AnthropicConfig::default()).payload(&request);
    assert!(partly["tools"][0].get("cache_control").is_none());
    assert!(partly["system"].is_array());

    // Off for an endpoint that rejects the field.
    let off = AnthropicConfig {
        prompt_cache: false,
        ..AnthropicConfig::default()
    };
    let plain = provider("claude-opus-4-8", off).payload(&request);
    assert_eq!(plain["system"], json!(system));
}

#[tokio::test]
async fn tool_use_blocks_become_tool_calls() {
    let server = MockServer::start(vec![Reply::Sse(fixture("anthropic/tools.sse"))]).await;
    let provider = provider("claude-sonnet-4-6", config(&server.base_url));

    let completion = provider.complete(request("Look around")).await.unwrap();

    assert_eq!(completion.text, "Looking.");
    let calls: Vec<(&str, &str)> = completion
        .tool_calls
        .iter()
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect();
    assert_eq!(calls, [("toolu_01", "read_file"), ("toolu_02", "list_dir")]);
    assert_eq!(
        completion.tool_calls[0].arguments["path"],
        json!("README.md")
    );
    assert!(completion.tool_calls[1].arguments.is_empty());
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(completion.usage.input_tokens, 180);
}

#[tokio::test]
async fn errors_read_as_the_sdk_words_them_with_the_key_redacted() {
    let unauthorized = format!(
        "{{\"type\":\"error\",\"error\":{{\"type\":\"authentication_error\",\
         \"message\":\"invalid x-api-key {TEST_KEY}\"}}}}"
    );
    let overloaded = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n\
                      event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let server = MockServer::start(vec![
        status(401, Vec::new(), &unauthorized),
        Reply::Sse(overloaded.to_owned()),
    ])
    .await;
    let provider = provider("claude-sonnet-4-6", config(&server.base_url));

    let refused = provider
        .complete(request("hi"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.starts_with(
            "Anthropic API error: Error code: 401 - {'type': 'error', 'error': \
             {'type': 'authentication_error', 'message': 'invalid x-api-key "
        ),
        "{refused}"
    );
    assert!(!refused.contains(TEST_KEY), "{refused}");

    let failed = provider.complete(request("hi")).await.unwrap_err();
    assert_eq!(
        failed.to_string(),
        "Anthropic API error: {'type': 'error', 'error': {'type': 'overloaded_error', \
         'message': 'Overloaded'}}"
    );
    assert_eq!(server.requests().len(), 2, "neither is retried");
}

#[tokio::test]
async fn the_sdk_retries_transient_statuses_but_not_after_the_stream_opens() {
    let busy = status(529, vec![("retry-after", "0.01".to_owned())], "{}");
    let partial = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n\
                   event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                   event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Half\"}}\n\n";
    let server = MockServer::start(vec![
        busy,
        Reply::Sse(fixture("anthropic/text.sse")),
        status(400, Vec::new(), "{\"error\":\"bad\"}"),
        Reply::Truncated(partial.to_owned()),
    ])
    .await;
    let provider = provider("claude-sonnet-4-6", config(&server.base_url));

    let completion = provider.complete(request("hi")).await.unwrap();
    assert_eq!(completion.text, "Hello, world");
    assert_eq!(server.requests().len(), 2);

    let bad = provider.complete(request("hi")).await.unwrap_err();
    assert_eq!(
        bad.to_string(),
        "Anthropic API error: Error code: 400 - {'error': 'bad'}"
    );
    assert_eq!(server.requests().len(), 3, "a 400 is not retried");

    let (deltas, error) = drain(&provider, request("hi")).await;
    assert_eq!(deltas.len(), 1);
    let error = error.unwrap().to_string();
    assert!(
        error.starts_with(
            "The 'anthropic' provider failed while streaming model 'claude-sonnet-4-6' — ReadError: "
        ) && error.ends_with(
            ". The turn was halted and the session saved; retry the request, or check the \
             endpoint and network."
        ),
        "{error}"
    );
    assert_eq!(
        server.requests().len(),
        4,
        "an open stream is never retried"
    );
}

#[tokio::test]
async fn a_refused_temperature_is_dropped_and_remembered() {
    let refusal = "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\
                   \"message\":\"temperature is deprecated for this model\"}}";
    let server = MockServer::start(vec![
        status(400, Vec::new(), refusal),
        Reply::Sse(fixture("anthropic/text.sse")),
        Reply::Sse(fixture("anthropic/text.sse")),
    ])
    .await;
    // A model no table knows, so only the API's answer can teach the adapter.
    let model = "claude-test-sampling-refused";
    let provider = provider(model, config(&server.base_url));

    let completion = provider.complete(request("hi")).await.unwrap();
    assert_eq!(completion.text, "Hello, world");
    provider.complete(request("hi")).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests[0].body["temperature"], json!(0.0));
    assert!(requests[1].body.get("temperature").is_none());
    assert!(
        requests[2].body.get("temperature").is_none(),
        "learned for the session"
    );
}

#[tokio::test]
async fn connection_failures_and_stalls_use_the_sdk_messages() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let gone = AnthropicConfig {
        max_retries: 0,
        ..config(&base_url)
    };
    let error = provider("claude-sonnet-4-6", gone)
        .complete(request("hi"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Anthropic API error: Connection error.");

    let server = MockServer::start(vec![Reply::StallAfter(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n"
            .to_owned(),
    )])
    .await;
    let stalled = AnthropicConfig {
        read_timeout: Duration::from_millis(300),
        ..config(&server.base_url)
    };
    let (_, error) = drain(&provider("claude-sonnet-4-6", stalled), request("hi")).await;
    assert_eq!(
        error.unwrap().to_string(),
        "The 'anthropic' provider failed while streaming model 'claude-sonnet-4-6' — \
         ReadTimeout: The read operation timed out. The turn was halted and the session saved; \
         retry the request, or check the endpoint and network."
    );
}

#[test]
fn the_factory_needs_a_key_and_names_its_variable() {
    let paths = ConfigPaths {
        user: None,
        project: std::path::PathBuf::from("no-such-dir/config.toml"),
        project_trust: agent86::config::ProjectTrust::Trusted,
    };
    let mut config = load_config(&paths, &Overrides::default()).unwrap();
    config.providers.get_mut("anthropic").unwrap().api_key_env =
        Some("AGENT86_TEST_UNSET_ANTHROPIC_KEY".to_owned());
    let reference = ModelRef::parse("anthropic:claude-opus-4-8").unwrap();

    let error = provider_for_ref_with_store(&reference, &config, &NoSecretStore)
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "No Anthropic API key found. Set the AGENT86_TEST_UNSET_ANTHROPIC_KEY environment \
         variable or store a key in the OS keyring via /config model."
    );

    let provider =
        agent86::provider::provider_for_ref_with_key(&reference, &config, TEST_KEY).unwrap();
    assert_eq!(
        (provider.name(), provider.model()),
        ("anthropic", "claude-opus-4-8")
    );
}
