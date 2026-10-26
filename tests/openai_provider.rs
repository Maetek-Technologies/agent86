mod support;

use agent86::pricing::PriceTable;
use agent86::provider::openai::{OpenAiConfig, OpenAiProvider};
use agent86::provider::{Provider, ProviderError};
use agent86::types::{CompletionDelta, CompletionRequest, Message, Role, ToolSpec};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;
use support::{fixture, MockServer, Reply};

const TEST_KEY: &str = "sk-test-0123456789abcdefghij";

fn config(server: &MockServer) -> OpenAiConfig {
    OpenAiConfig {
        base_url: Some(server.base_url.clone()),
        api_key_env: Some("OPENROUTER_API_KEY".to_owned()),
        ..OpenAiConfig::default()
    }
}

fn provider(config: OpenAiConfig) -> OpenAiProvider {
    OpenAiProvider::new("test/model", config, Some(TEST_KEY.to_owned())).unwrap()
}

fn request(prompt: &str) -> CompletionRequest {
    CompletionRequest {
        model: "openrouter:test/model".to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: prompt.to_owned(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: None,
        stream: true,
    }
}

fn status(status: u16, body: &str) -> Reply {
    Reply::Status {
        status,
        headers: Vec::new(),
        body: body.to_owned(),
    }
}

/// Collect every item the stream yields, stopping after the first error.
async fn drain(
    provider: &OpenAiProvider,
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

#[tokio::test]
async fn streams_text_deltas_then_a_final_completion() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let mut config = config(&server);
    config.max_tokens = Some(256);
    let provider = provider(config);

    let (deltas, error) = drain(&provider, request("Say hello")).await;
    assert_eq!(error, None);

    let texts: Vec<&str> = deltas
        .iter()
        .filter(|d| !d.done)
        .map(|d| d.text.as_str())
        .collect();
    assert_eq!(texts, ["Hello", ", world"]);

    let last = deltas.last().unwrap();
    assert!(last.done);
    let completion = last.completion.as_ref().unwrap();
    assert_eq!(completion.text, "Hello, world");
    assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(completion.model.as_deref(), Some("test/model"));
    assert_eq!(completion.usage.input_tokens, 12);
    assert_eq!(completion.usage.output_tokens, 3);
    assert_eq!(completion.usage.cache_read_tokens, 4);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {TEST_KEY}").as_str())
    );
    assert_eq!(
        sent.body,
        json!({
            "model": "test/model",
            "messages": [{"role": "user", "content": "Say hello"}],
            "stream": true,
            "stream_options": {"include_usage": true},
            "temperature": 0.0,
            "max_completion_tokens": 256,
        })
    );
}

#[tokio::test]
async fn complete_returns_the_same_text_as_streaming() {
    let body = fixture("openai/text.sse");
    let server = MockServer::start(vec![Reply::Sse(body.clone()), Reply::Sse(body)]).await;
    let provider = provider(config(&server));

    let (deltas, _) = drain(&provider, request("hi")).await;
    let streamed: String = deltas.iter().map(|d| d.text.as_str()).collect();
    let completion = provider.complete(request("hi")).await.unwrap();

    assert_eq!(completion.text, streamed);
    assert_eq!(completion.text, "Hello, world");
}

#[tokio::test]
async fn tool_call_fragments_are_accumulated_by_index() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/tool_calls.sse"))]).await;
    let provider = provider(config(&server));
    let mut tool_request = request("Read the README");
    tool_request.tools = vec![ToolSpec::new("read_file", "Read a file")];

    let completion = provider.complete(tool_request).await.unwrap();

    assert_eq!(completion.text, "");
    assert_eq!(completion.tool_calls.len(), 2);
    assert_eq!(completion.tool_calls[0].id, "call_abc");
    assert_eq!(completion.tool_calls[0].name, "read_file");
    assert_eq!(completion.tool_calls[0].arguments["path"], "README.md");
    assert_eq!(completion.tool_calls[1].id, "call_1");
    assert!(completion.tool_calls[1].arguments.is_empty());
    // A plain `stop` on a turn that emitted tool calls is still a tool turn.
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));

    let sent = &server.requests()[0].body;
    assert_eq!(sent["tool_choice"], "auto");
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "read_file");
}

#[tokio::test]
async fn empty_and_unterminated_streams_complete_cleanly() {
    let server = MockServer::start(vec![
        Reply::Sse(fixture("openai/empty.sse")),
        Reply::Sse("data: {\"choices\":[{\"delta\":{\"content\":\"no done marker\"}}]}".to_owned()),
    ])
    .await;
    let provider = provider(config(&server));

    let empty = provider.complete(request("hi")).await.unwrap();
    assert_eq!(empty.text, "");
    assert_eq!(empty.stop_reason, None);

    let unterminated = provider.complete(request("hi")).await.unwrap();
    assert_eq!(unterminated.text, "no done marker");
}

#[tokio::test]
async fn malformed_chunks_become_structured_errors() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/malformed.sse"))]).await;
    let provider = provider(config(&server));

    let (deltas, error) = drain(&provider, request("hi")).await;

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].text, "Partial");
    assert!(
        matches!(error, Some(ProviderError::MalformedChunk { .. })),
        "{error:?}"
    );
}

#[tokio::test]
async fn truncated_streams_fail_without_retrying_emitted_text() {
    let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"Half\"}}]}\n\n".to_owned();
    let server = MockServer::start(vec![
        Reply::Truncated(partial),
        Reply::Sse(fixture("openai/text.sse")),
    ])
    .await;
    let provider = provider(config(&server));

    let (deltas, error) = drain(&provider, request("hi")).await;

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].text, "Half");
    assert!(
        matches!(error, Some(ProviderError::StreamInterrupted { .. })),
        "{error:?}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn client_errors_are_reported_redacted_and_not_retried() {
    let body = format!("{{\"error\":{{\"message\":\"Invalid key {TEST_KEY}\"}}}}");
    let server = MockServer::start(vec![status(401, &body), status(200, "")]).await;
    let provider = provider(config(&server));

    let error = provider.complete(request("hi")).await.unwrap_err();

    let ProviderError::Http { status, body } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(*status, 401);
    assert!(!body.contains(TEST_KEY), "{body}");
    assert!(!error.to_string().contains(TEST_KEY));
    assert!(body.contains("***redacted***"));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn transient_statuses_are_retried_honouring_retry_after() {
    let busy = Reply::Status {
        status: 503,
        headers: vec![("Retry-After", "0".to_owned())],
        body: "overloaded".to_owned(),
    };
    let server = MockServer::start(vec![busy, Reply::Sse(fixture("openai/text.sse"))]).await;
    let provider = provider(config(&server));

    let completion = provider.complete(request("hi")).await.unwrap();

    assert_eq!(completion.text, "Hello, world");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn the_retry_budget_is_finite() {
    let busy = || Reply::Status {
        status: 429,
        headers: vec![("Retry-After", "0".to_owned())],
        body: "rate limited".to_owned(),
    };
    let server = MockServer::start(vec![busy(), busy(), busy(), busy()]).await;
    let provider = provider(config(&server));

    let error = provider.complete(request("hi")).await.unwrap_err();

    assert_eq!(
        error,
        ProviderError::Http {
            status: 429,
            body: "rate limited".to_owned()
        }
    );
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn endpoints_rejecting_max_completion_tokens_are_retried_with_max_tokens() {
    let rejection =
        r#"{"error":{"message":"Unrecognized request argument supplied: max_completion_tokens"}}"#;
    let server = MockServer::start(vec![
        status(400, rejection),
        Reply::Sse(fixture("openai/text.sse")),
        Reply::Sse(fixture("openai/text.sse")),
    ])
    .await;
    let mut config = config(&server);
    config.max_retries = 0;
    config.max_tokens = Some(64);
    let provider = provider(config);

    provider.complete(request("hi")).await.unwrap();
    provider.complete(request("again")).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body["max_completion_tokens"], 64);
    assert_eq!(requests[1].body["max_tokens"], 64);
    assert!(requests[1].body.get("max_completion_tokens").is_none());
    // The endpoint's dialect is remembered for later requests.
    assert_eq!(requests[2].body["max_tokens"], 64);
}

#[tokio::test]
async fn stalled_streams_hit_the_read_timeout() {
    let first = "data: {\"choices\":[{\"delta\":{\"content\":\"Started\"}}]}\n\n".to_owned();
    let server = MockServer::start(vec![Reply::StallAfter(first)]).await;
    let mut config = config(&server);
    config.read_timeout = Duration::from_millis(300);
    let provider = provider(config);

    let (deltas, error) = drain(&provider, request("hi")).await;

    assert_eq!(deltas.len(), 1);
    assert!(
        matches!(error, Some(ProviderError::ReadTimeout { .. })),
        "{error:?}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn unreachable_endpoints_report_a_connect_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let config = OpenAiConfig {
        base_url: Some(base_url),
        max_retries: 0,
        ..OpenAiConfig::default()
    };
    let provider = provider(config);

    let error = provider.complete(request("hi")).await.unwrap_err();

    assert!(matches!(error, ProviderError::Connect { .. }), "{error:?}");
}

#[test]
fn a_missing_key_names_the_configured_environment_variable() {
    let config = OpenAiConfig {
        api_key_env: Some("OPENROUTER_API_KEY".to_owned()),
        config_name: "openrouter".to_owned(),
        ..OpenAiConfig::default()
    };
    let error = OpenAiProvider::new("m", config, Some(String::new())).unwrap_err();
    assert_eq!(
        error.to_string(),
        "No API key found. Set the OPENROUTER_API_KEY environment variable or store a key in the OS keyring (service \"agent86\", account \"openrouter\")."
    );

    let keyless = OpenAiConfig {
        base_url: Some("http://localhost:1234".to_owned()),
        require_key: false,
        ..OpenAiConfig::default()
    };
    assert!(OpenAiProvider::new("m", keyless, None).is_ok());
}

#[test]
fn base_urls_are_accepted_with_or_without_v1() {
    for (base, expected) in [
        (None, "https://api.openai.com/v1/chat/completions"),
        (
            Some("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/chat/completions",
        ),
        (
            Some("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/chat/completions",
        ),
        (
            Some("http://localhost:8080"),
            "http://localhost:8080/v1/chat/completions",
        ),
    ] {
        let config = OpenAiConfig {
            base_url: base.map(str::to_owned),
            ..OpenAiConfig::default()
        };
        let provider = OpenAiProvider::new("m", config, Some(TEST_KEY.to_owned())).unwrap();
        assert_eq!(provider.url(), expected);
    }
}

#[test]
fn sampling_parameters_are_omitted_for_models_that_removed_them() {
    let config = OpenAiConfig::default();
    let provider = OpenAiProvider::new("claude-opus-5", config, Some(TEST_KEY.to_owned())).unwrap();
    let payload = provider.payload(&request("hi"));
    assert!(payload.get("temperature").is_none());
}

#[test]
fn debug_output_never_contains_the_api_key() {
    let provider =
        OpenAiProvider::new("m", OpenAiConfig::default(), Some(TEST_KEY.to_owned())).unwrap();
    assert!(!format!("{provider:?}").contains(TEST_KEY));
}

#[tokio::test]
async fn usage_is_priced_from_the_builtin_table_under_the_price_ref() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let provider = OpenAiProvider::new("gpt-4o-mini", config(&server), Some(TEST_KEY.to_owned()))
        .unwrap()
        .with_pricing(PriceTable::builtin(), "openai:gpt-4o-mini");

    let completion = provider.complete(request("hi")).await.unwrap();

    // 12 prompt tokens, 4 of them read from cache, and 3 output tokens at $0.15 / $0.60.
    let expected = (8.0 / 1e6) * 0.15 + (4.0 / 1e6) * (0.15 * 0.1) + 0.0 + (3.0 / 1e6) * 0.60;
    assert_eq!(completion.usage.cost_usd, expected);
}

#[tokio::test]
async fn unpriced_models_report_zero_cost() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let provider =
        provider(config(&server)).with_pricing(PriceTable::builtin(), "openrouter:test/model");

    let completion = provider.complete(request("hi")).await.unwrap();

    assert_eq!(completion.usage.cost_usd, 0.0);
}
