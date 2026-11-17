//! The `google` provider (beyond Python): Gemini on `generateContent`, streamed.
//! Every test runs against the local mock server with fixtures in the shape of
//! Google's documented responses (`tests/fixtures/google`), a key made up for the
//! test, and key variables nothing sets.

mod support;

use agent86::config::{load_config, Config, ConfigPaths, Overrides, ProjectTrust};
use agent86::provider::catalog::fetch_catalog;
use agent86::provider::google::{GoogleConfig, GoogleProvider, SKIP_SIGNATURE};
use agent86::provider::remote_windows::google_context_window;
use agent86::provider::{provider_for_ref_with_store, Provider, ProviderError};
use agent86::secrets::{NoSecretStore, SecretStore};
use agent86::types::{
    CompletionDelta, CompletionRequest, Message, ModelRef, Role, ToolCall, ToolSpec,
};
use futures_util::StreamExt;
use serde_json::json;
use support::{fixture, MockServer, Reply};

/// Not shaped like a real Google key, so no scanner mistakes it for one.
const TEST_KEY: &str = "gemini-test-key-0123456789";
const UNSET: &str = "AGENT86_TEST_UNSET_GEMINI_KEY";

struct OneKey;

impl SecretStore for OneKey {
    fn get_password(&self, _service: &str, account: &str) -> Option<String> {
        (account == "google").then(|| TEST_KEY.to_owned())
    }
}

fn provider(server: &MockServer) -> GoogleProvider {
    let config = GoogleConfig {
        base_url: Some(format!("{}/v1", server.base_url)),
        max_retries: 1,
        ..GoogleConfig::default()
    };
    GoogleProvider::new("gemini-3.8-flash", config, Some(TEST_KEY.to_owned()))
        .unwrap()
        .with_pricing(
            agent86::pricing::PriceTable::builtin(),
            "google:gemini-3.8-flash",
        )
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

fn request(messages: Vec<Message>, max_tokens: Option<u32>) -> CompletionRequest {
    CompletionRequest {
        model: "google:gemini-3.8-flash".to_owned(),
        messages,
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens,
        stream: true,
    }
}

fn hello() -> CompletionRequest {
    request(
        vec![
            message(Role::System, "You are a test."),
            message(Role::User, "Hi"),
        ],
        Some(256),
    )
}

async fn drain(
    provider: &dyn Provider,
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

fn status(code: u16, body: &str) -> Reply {
    Reply::Status {
        status: code,
        headers: Vec::new(),
        body: body.to_owned(),
    }
}

fn builtin_config() -> Config {
    let paths = ConfigPaths {
        user: None,
        project: std::path::PathBuf::from("no-such-dir/config.toml"),
        project_trust: ProjectTrust::Trusted,
    };
    load_config(&paths, &Overrides::default()).unwrap()
}

#[tokio::test]
async fn the_request_is_generate_content_with_the_key_in_a_header_and_storage_off() {
    let server = MockServer::start(vec![Reply::Sse(fixture("google/text.sse"))]).await;
    let provider = provider(&server);
    assert_eq!(drain(&provider, hello()).await.1, None);

    let recorded = &server.requests()[0];
    assert!(
        recorded
            .request_line
            .starts_with("POST /v1/models/gemini-3.8-flash:streamGenerateContent?alt=sse "),
        "{}",
        recorded.request_line
    );
    assert!(
        !recorded.request_line.contains(TEST_KEY),
        "never in the URL"
    );
    assert_eq!(recorded.header("x-goog-api-key"), Some(TEST_KEY));
    assert_eq!(recorded.header("authorization"), None);
    assert_eq!(
        recorded.body,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Hi"}]}],
            "generationConfig": {
                "thinkingConfig": {"includeThoughts": true},
                "maxOutputTokens": 256,
            },
            "store": false,
            "systemInstruction": {"parts": [{"text": "You are a test."}]},
        }),
        "no sampling parameters, storage off"
    );
}

#[tokio::test]
async fn every_request_ends_with_user_content_and_history_calls_carry_the_placeholder() {
    let server = MockServer::start(vec![Reply::Sse(fixture("google/text.sse"))]).await;
    let mut assistant = message(Role::Assistant, "");
    assistant.tool_calls = vec![ToolCall {
        id: "call_from_another_provider".into(),
        name: "read_file".into(),
        arguments: json!({"path": "a.txt"}).as_object().unwrap().clone(),
    }];
    let mut result = message(Role::Tool, "hello");
    result.tool_call_id = Some("call_from_another_provider".into());
    result.name = Some("read_file".into());
    let messages = vec![
        message(Role::System, "You are a test."),
        message(Role::User, "Read a.txt"),
        assistant,
        result,
    ];
    let provider = provider(&server);
    assert_eq!(drain(&provider, request(messages, None)).await.1, None);
    let body = &server.requests()[0].body;
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents.last().unwrap()["role"], "user");
    assert_eq!(
        contents[1]["parts"][0]["thoughtSignature"],
        json!(SKIP_SIGNATURE)
    );
    assert!(
        contents[1]["parts"][0]["functionCall"].get("id").is_none(),
        "another provider's id is not Google's to check"
    );
    assert!(body["generationConfig"].get("maxOutputTokens").is_none());
}

#[tokio::test]
async fn thought_summaries_stream_apart_from_the_text_with_usage_and_cost() {
    let server = MockServer::start(vec![Reply::Sse(fixture("google/text.sse"))]).await;
    let provider = provider(&server);
    let (deltas, error) = drain(&provider, hello()).await;
    assert_eq!(error, None);
    let thinking: Vec<&str> = deltas
        .iter()
        .filter(|delta| !delta.thinking.is_empty())
        .map(|delta| delta.thinking.as_str())
        .collect();
    assert_eq!(thinking, ["**Greeting the user**\n\nThe user says hi."]);
    let text: Vec<&str> = deltas
        .iter()
        .filter(|delta| !delta.text.is_empty())
        .map(|delta| delta.text.as_str())
        .collect();
    assert_eq!(text, ["Hello", " there!"]);

    let completion = deltas.last().unwrap().completion.clone().unwrap();
    assert_eq!(completion.text, "Hello there!");
    assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(completion.model.as_deref(), Some("gemini-3.8-flash"));
    // Thinking is billed as output; the cached slice is inside the prompt.
    assert_eq!(completion.usage.input_tokens, 120);
    assert_eq!(completion.usage.output_tokens, 38);
    assert_eq!(completion.usage.cache_read_tokens, 100);
    let expected = (20.0 * 0.75 + 100.0 * 0.075 + 38.0 * 3.75) / 1_000_000.0;
    assert!(
        (completion.usage.cost_usd - expected).abs() < 1e-12,
        "{}",
        completion.usage.cost_usd
    );
}

#[tokio::test]
async fn a_reply_google_stops_itself_carries_the_reason() {
    for (name, reason) in [
        ("google/stopped.sse", "stopped:SAFETY"),
        ("google/blocked.sse", "stopped:PROHIBITED_CONTENT"),
    ] {
        let server = MockServer::start(vec![Reply::Sse(fixture(name))]).await;
        let (deltas, error) = drain(&provider(&server), hello()).await;
        assert_eq!(error, None, "{name}");
        let completion = deltas.last().unwrap().completion.clone().unwrap();
        assert_eq!(completion.stop_reason.as_deref(), Some(reason), "{name}");
        assert_eq!(completion.text, "");
    }
    let cut = "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Half\"}],\"role\": \"model\"},\"finishReason\": \"MAX_TOKENS\"}]}\r\n\r\n";
    let server = MockServer::start(vec![Reply::Sse(cut.to_owned())]).await;
    let (deltas, _) = drain(&provider(&server), hello()).await;
    let completion = deltas.last().unwrap().completion.clone().unwrap();
    assert_eq!(completion.stop_reason.as_deref(), Some("max_tokens"));
    assert_eq!(completion.text, "Half");
}

#[tokio::test]
async fn transient_failures_are_retried_and_a_402_is_not() {
    let busy = r#"{"error": {"code": 429, "message": "Resource has been exhausted (e.g. check quota).", "status": "RESOURCE_EXHAUSTED"}}"#;
    let server = MockServer::start(vec![
        status(429, busy),
        Reply::Sse(fixture("google/text.sse")),
    ])
    .await;
    let (_, error) = drain(&provider(&server), hello()).await;
    assert_eq!(error, None);
    assert_eq!(server.requests().len(), 2);

    let broke = r#"{"error": {"code": 402, "message": "Prepayment credits are depleted.", "status": "RESOURCE_EXHAUSTED"}}"#;
    let server = MockServer::start(vec![status(402, broke), status(402, broke)]).await;
    let (_, error) = drain(&provider(&server), hello()).await;
    let Some(ProviderError::HttpHint { status, body, hint }) = error else {
        panic!("a hinted 402");
    };
    assert_eq!((status, body.as_str()), (402, broke));
    assert!(hint.contains("prepaid credits"));
    assert_eq!(server.requests().len(), 1, "never retried");
}

#[tokio::test]
async fn an_error_body_keeps_googles_text_with_the_key_redacted() {
    let body = format!(
        r#"{{"error": {{"code": 400, "message": "API key {TEST_KEY} not valid. Please pass a valid API key.", "status": "INVALID_ARGUMENT"}}}}"#
    );
    let server = MockServer::start(vec![status(400, &body)]).await;
    let (_, error) = drain(&provider(&server), hello()).await;
    let Some(ProviderError::Http { status: 400, body }) = error else {
        panic!("an HTTP error");
    };
    assert!(body.contains("INVALID_ARGUMENT"));
    assert!(!body.contains(TEST_KEY), "{body}");
}

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: format!("The {name} tool."),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        side_effecting: false,
    }
}

fn call_reply(name: &str, id: Option<&str>) -> Reply {
    let mut call = json!({"name": name, "args": {"path": "x"}});
    if let Some(id) = id {
        call["id"] = json!(id);
    }
    let chunk = json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": call}]},
                                       "finishReason": "STOP"}]});
    Reply::Sse(format!("data: {chunk}\n\n"))
}

#[tokio::test]
async fn a_tool_whose_name_gemini_refuses_is_declared_renamed_and_mapped_back() {
    let server = MockServer::start(vec![call_reply("_1password-get", Some("fc-1"))]).await;
    let mut ask = hello();
    ask.tools = vec![tool("1password-get"), tool("read_file")];
    let (deltas, error) = drain(&provider(&server), ask).await;
    assert_eq!(error, None);
    let completion = deltas.last().unwrap().completion.clone().unwrap();
    assert_eq!(completion.tool_calls[0].name, "1password-get");
    assert_eq!(completion.tool_calls[0].id, "fc-1");
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    let requests = server.requests();
    let names: Vec<&str> = requests[0].body["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|declaration| declaration["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["_1password-get", "read_file"]);
}

#[tokio::test]
async fn a_call_without_an_id_gets_one_of_ours_which_never_goes_back() {
    let server = MockServer::start(vec![
        call_reply("read_file", None),
        Reply::Sse(fixture("google/text.sse")),
    ])
    .await;
    let provider = provider(&server);
    let mut ask = hello();
    ask.tools = vec![tool("read_file")];
    let (deltas, _) = drain(&provider, ask.clone()).await;
    let call = deltas
        .last()
        .unwrap()
        .completion
        .clone()
        .unwrap()
        .tool_calls[0]
        .clone();
    assert!(call.id.starts_with("call_g"), "{}", call.id);

    let mut step = message(Role::Assistant, "");
    step.tool_calls = vec![call.clone()];
    let mut result = message(Role::Tool, "x's contents");
    result.tool_call_id = Some(call.id.clone());
    result.name = Some("read_file".into());
    ask.messages.extend([step, result]);
    assert_eq!(drain(&provider, ask).await.1, None);
    let contents = server.requests()[1].body["contents"].clone();
    let contents = contents.as_array().unwrap();
    let sent_call = &contents[contents.len() - 2]["parts"][0];
    assert!(sent_call["functionCall"].get("id").is_none());
    // No signature came with it, so the step's first call gets the placeholder.
    assert_eq!(sent_call["thoughtSignature"], json!(SKIP_SIGNATURE));
    assert!(contents[contents.len() - 1]["parts"][0]["functionResponse"]
        .get("id")
        .is_none());
}

#[tokio::test]
async fn a_missing_signature_400_is_called_a_bug() {
    let body = r#"{"error": {"code": 400, "message": "Function call is missing a thought_signature in functionCall parts. This is required for tools to work correctly.", "status": "INVALID_ARGUMENT"}}"#;
    let server = MockServer::start(vec![status(400, body)]).await;
    let (_, error) = drain(&provider(&server), hello()).await;
    let Some(ProviderError::HttpHint {
        status: 400, hint, ..
    }) = error
    else {
        panic!("a hinted 400");
    };
    assert!(hint.contains("this is a bug"), "{hint}");
}

#[test]
fn the_factory_needs_a_key_and_builds_from_the_built_in_section() {
    let mut config = builtin_config();
    config.providers.get_mut("google").unwrap().api_key_env = Some(UNSET.into());
    let reference = ModelRef::parse("google:gemini-3.8-flash").unwrap();
    let Err(error) = provider_for_ref_with_store(&reference, &config, &NoSecretStore) else {
        panic!("no key, no provider");
    };
    assert!(matches!(error, ProviderError::MissingApiKey { .. }));
    let provider = provider_for_ref_with_store(&reference, &config, &OneKey).unwrap();
    assert_eq!(
        (provider.name(), provider.model()),
        ("google", "gemini-3.8-flash")
    );
    let section = &config.providers["google"];
    assert_eq!(section.read_timeout_s, 1200.0);
    assert!(!section.store);
}

// The catalog and window fetches block on a thread of their own while the mock
// server answers on this runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catalog_follows_pages_and_keeps_models_that_chat() {
    let page = |name: &str| Reply::Status {
        status: 200,
        headers: Vec::new(),
        body: fixture(name),
    };
    let server = MockServer::start(vec![
        page("google/models_page1.json"),
        page("google/models_page2.json"),
    ])
    .await;
    let mut section = builtin_config().providers["google"].clone();
    section.base_url = Some(format!("{}/v1", server.base_url));
    let entries = tokio::task::spawn_blocking(move || {
        fetch_catalog("google", &section, Some(TEST_KEY)).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        entries,
        [
            (
                "gemini-3.1-flash-lite".to_owned(),
                "Gemini 3.1 Flash-Lite".to_owned()
            ),
            ("gemini-3.8-flash".to_owned(), "Gemini 3.8 Flash".to_owned()),
        ],
        "the embedding model is left out"
    );
    let requests = server.requests();
    assert!(requests[0]
        .request_line
        .starts_with("GET /v1/models?pageSize=1000 "));
    assert!(
        requests[1]
            .request_line
            .contains("pageToken=page%2F2%3D%3D"),
        "{}",
        requests[1].request_line
    );
    assert_eq!(requests[1].header("x-goog-api-key"), Some(TEST_KEY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_window_is_the_models_input_token_limit() {
    let server = MockServer::start(vec![Reply::Status {
        status: 200,
        headers: Vec::new(),
        body: fixture("google/models_page2.json"),
    }])
    .await;
    let mut config = builtin_config();
    config.providers.get_mut("google").unwrap().base_url = Some(format!("{}/v1", server.base_url));
    let window = tokio::task::spawn_blocking(move || {
        google_context_window("google", "gemini-3.1-flash-lite", &config, None, &OneKey)
    })
    .await
    .unwrap();
    assert_eq!(window, Some(1_048_576));
    let request = &server.requests()[0];
    assert!(request
        .request_line
        .starts_with("GET /v1/models?pageSize=1000 "));
    assert_eq!(request.header("x-goog-api-key"), Some(TEST_KEY));
}
