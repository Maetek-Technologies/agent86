//! OpenAI-compatible provider behavior against a scripted local server.

mod common;

use agent86::{
    config::ProviderConfig,
    provider::{complete, ModelProvider, OpenAiProvider, ProviderErrorKind},
    types::{CompletionRequest, Message, Role},
};
use common::{fixture, refused_base_url, MockServer, Reply};
use serde_json::json;

const KEY: &str = "sk-test-0123456789abcdef";

fn provider(base_url: &str, tweak: impl FnOnce(&mut ProviderConfig)) -> OpenAiProvider {
    let mut config = ProviderConfig {
        api_key_env: Some("AGENT86_TEST_KEY".into()),
        base_url: Some(base_url.to_owned()),
        ..ProviderConfig::default()
    };
    tweak(&mut config);
    OpenAiProvider::new("test-model", config, Some(KEY.into()), true).unwrap()
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        messages: vec![Message {
            role: Role::User,
            content: "Say hello".into(),
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

async fn stream_collect(
    provider: &OpenAiProvider,
) -> (
    Vec<String>,
    Result<agent86::types::Completion, agent86::provider::ProviderError>,
) {
    let mut deltas = Vec::new();
    let request = request();
    let result = provider
        .stream(&request, &mut |text: &str| deltas.push(text.to_owned()))
        .await;
    (deltas, result)
}

#[tokio::test]
async fn streams_text_deltas_and_reports_usage() {
    let server = MockServer::start(vec![Reply::sse(fixture("text.sse"))]);
    let provider = provider(&server.base_url(), |_| {});

    let (deltas, result) = stream_collect(&provider).await;
    let completion = result.unwrap();

    assert_eq!(deltas, ["Hello", ", wor", "ld!"]);
    assert_eq!(completion.text, "Hello, world!");
    assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(completion.model.as_deref(), Some("test-model"));
    assert_eq!(completion.usage.input_tokens, 12);
    assert_eq!(completion.usage.output_tokens, 4);
    assert_eq!(completion.usage.cache_read_tokens, 8);
    assert!(completion.tool_calls.is_empty());
}

#[tokio::test]
async fn sends_a_chat_completions_request() {
    let server = MockServer::start(vec![Reply::sse(fixture("text.sse"))]);
    let provider = provider(&server.base_url(), |_| {});
    stream_collect(&provider).await.1.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(
        sent.body,
        json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Say hello"}],
            "stream": true,
            "stream_options": {"include_usage": true},
            "temperature": 0.0,
        })
    );
}

#[tokio::test]
async fn streamed_and_non_streamed_calls_agree() {
    let server = MockServer::start(vec![
        Reply::sse(fixture("text.sse")),
        Reply::sse(fixture("text.sse")),
    ]);
    let provider = provider(&server.base_url(), |_| {});

    let (deltas, streamed) = stream_collect(&provider).await;
    let streamed = streamed.unwrap();
    let completed = complete(&provider, &request()).await.unwrap();

    assert_eq!(deltas.concat(), streamed.text);
    assert_eq!(streamed, completed);
}

#[tokio::test]
async fn an_empty_response_is_an_empty_completion() {
    let server = MockServer::start(vec![Reply::sse(fixture("empty.sse"))]);
    let provider = provider(&server.base_url(), |_| {});

    let (deltas, result) = stream_collect(&provider).await;
    let completion = result.unwrap();

    assert!(deltas.is_empty());
    assert_eq!(completion.text, "");
    assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(completion.usage.input_tokens, 5);
}

#[tokio::test]
async fn a_malformed_chunk_is_a_structured_error() {
    let server = MockServer::start(vec![Reply::sse(fixture("malformed.sse"))]);
    let provider = provider(&server.base_url(), |_| {});

    let err = stream_collect(&provider).await.1.unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::MalformedStream);
    assert!(err.message().contains("test-model"), "{err}");
    assert!(err.message().contains("/v1/chat/completions"), "{err}");
}

#[tokio::test]
async fn a_stream_that_stops_before_the_model_finishes_is_truncated() {
    let server = MockServer::start(vec![Reply::sse(fixture("truncated.sse"))]);
    let provider = provider(&server.base_url(), |_| {});

    let (deltas, result) = stream_collect(&provider).await;
    let err = result.unwrap_err();
    assert_eq!(deltas.concat(), "Partial answ");
    assert_eq!(err.kind(), ProviderErrorKind::TruncatedStream);
}

#[tokio::test]
async fn a_finished_stream_without_done_is_accepted() {
    let server = MockServer::start(vec![Reply::sse(fixture("no_done.sse"))]);
    let provider = provider(&server.base_url(), |_| {});

    let completion = stream_collect(&provider).await.1.unwrap();
    assert_eq!(completion.text, "Complete");
}

#[tokio::test]
async fn an_error_chunk_inside_the_stream_is_a_provider_error() {
    let server = MockServer::start(vec![Reply::sse(fixture("stream_error.sse"))]);
    let provider = provider(&server.base_url(), |_| {});

    let err = stream_collect(&provider).await.1.unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::Stream);
    assert!(err
        .message()
        .contains("Upstream provider returned an error"));
}

#[tokio::test]
async fn streamed_tool_call_fragments_are_assembled() {
    let server = MockServer::start(vec![Reply::sse(fixture("tool_call.sse"))]);
    let provider = provider(&server.base_url(), |_| {});

    let completion = stream_collect(&provider).await.1.unwrap();
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(completion.tool_calls.len(), 1);
    let call = &completion.tool_calls[0];
    assert_eq!(call.id, "call_abc");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.arguments.get("path"), Some(&json!("README.md")));
}

#[tokio::test]
async fn client_errors_are_reported_without_retrying() {
    let server = MockServer::start(vec![Reply::status(
        401,
        r#"{"error":{"message":"No auth credentials found","code":401}}"#,
    )]);
    let provider = provider(&server.base_url(), |_| {});

    let err = stream_collect(&provider).await.1.unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::Http);
    assert!(err.message().starts_with("HTTP 401: "), "{err}");
    assert!(err.message().contains("No auth credentials found"));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn transient_statuses_are_retried_honouring_retry_after() {
    let server = MockServer::start(vec![
        Reply::status(503, "overloaded").with_header("Retry-After", "0"),
        Reply::status(429, "slow down").with_header("Retry-After", "0"),
        Reply::sse(fixture("text.sse")),
    ]);
    let provider = provider(&server.base_url(), |_| {});

    let completion = stream_collect(&provider).await.1.unwrap();
    assert_eq!(completion.text, "Hello, world!");
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn the_retry_budget_is_bounded_by_max_retries() {
    let server = MockServer::start(vec![
        Reply::status(429, "slow down").with_header("Retry-After", "0"),
        Reply::status(429, "still slow").with_header("Retry-After", "0"),
        Reply::sse(fixture("text.sse")),
    ]);
    let provider = provider(&server.base_url(), |config| config.max_retries = 1);

    let err = stream_collect(&provider).await.1.unwrap_err();
    assert!(err.message().starts_with("HTTP 429: still slow"), "{err}");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn a_connection_lost_before_any_text_is_retried() {
    let server = MockServer::start(vec![
        Reply::CutOff {
            sent: ": waiting\n".into(),
            promised: 4096,
        },
        Reply::sse(fixture("text.sse")),
    ]);
    let provider = provider(&server.base_url(), |_| {});

    let completion = stream_collect(&provider).await.1.unwrap();
    assert_eq!(completion.text, "Hello, world!");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn a_connection_lost_after_text_is_never_retried() {
    let first_chunk = fixture("text.sse")
        .split("\n\n")
        .take(3)
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\n";
    let server = MockServer::start(vec![
        Reply::CutOff {
            sent: first_chunk,
            promised: 65_536,
        },
        Reply::sse(fixture("text.sse")),
    ]);
    let provider = provider(&server.base_url(), |_| {});

    let (deltas, result) = stream_collect(&provider).await;
    let err = result.unwrap_err();
    assert_eq!(deltas, ["Hello"], "text must not be duplicated by a retry");
    assert_eq!(err.kind(), ProviderErrorKind::Transport);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_stalled_endpoint_hits_the_read_timeout() {
    let server = MockServer::start(vec![Reply::Stall]);
    let provider = provider(&server.base_url(), |config| config.read_timeout_s = 0.3);

    let err = stream_collect(&provider).await.1.unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::ReadTimeout);
    assert!(err.message().contains("after 0.3s"), "{err}");
    assert!(err.message().contains("read_timeout_s"), "{err}");
    assert_eq!(server.requests().len(), 1, "read timeouts are not retried");
}

#[tokio::test]
async fn an_unreachable_endpoint_is_a_connect_error() {
    let base_url = refused_base_url();
    let provider = provider(&base_url, |config| config.max_retries = 0);

    let err = stream_collect(&provider).await.1.unwrap_err();
    assert_eq!(err.kind(), ProviderErrorKind::Connect);
    assert!(
        err.message().starts_with("Cannot reach http://127.0.0.1:"),
        "{err}"
    );
}

#[tokio::test]
async fn max_completion_tokens_falls_back_to_max_tokens_once() {
    let server = MockServer::start(vec![
        Reply::status(
            400,
            r#"{"error":{"message":"Unrecognized request argument supplied: max_completion_tokens"}}"#,
        ),
        Reply::sse(fixture("text.sse")),
        Reply::sse(fixture("text.sse")),
    ]);
    let provider = provider(&server.base_url(), |config| {
        config.max_tokens = Some(64);
        config.max_retries = 0;
    });

    stream_collect(&provider).await.1.unwrap();
    stream_collect(&provider).await.1.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body["max_completion_tokens"], json!(64));
    assert_eq!(requests[1].body["max_tokens"], json!(64));
    assert!(requests[1].body.get("max_completion_tokens").is_none());
    assert_eq!(
        requests[2].body["max_tokens"],
        json!(64),
        "the endpoint's dialect is remembered"
    );
}

#[tokio::test]
async fn api_keys_are_redacted_from_errors() {
    let server = MockServer::start(vec![Reply::status(
        401,
        format!(r#"{{"error":{{"message":"Invalid key {KEY}"}}}}"#),
    )]);
    let provider = provider(&server.base_url(), |_| {});

    let err = stream_collect(&provider).await.1.unwrap_err();
    assert!(!err.message().contains(KEY), "{err}");
    assert!(err.message().contains("Invalid key ***"), "{err}");
}
