//! The Ollama provider against the local mock server: NDJSON streaming, tool
//! calls, usage, retries, and Python's error messages.

mod support;

use agent86::config::{load_config, ConfigPaths, Overrides};
use agent86::provider::ollama::{OllamaConfig, OllamaProvider};
use agent86::provider::{provider_for_ref_with_store, Provider, ProviderError};
use agent86::secrets::NoSecretStore;
use agent86::types::{
    CompletionDelta, CompletionRequest, Message, ModelRef, Role, ToolCall, ToolSpec,
};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;
use support::{fixture, MockServer, Reply};

fn config(base_url: &str) -> OllamaConfig {
    OllamaConfig {
        base_url: Some(base_url.to_owned()),
        num_ctx: Some(8192),
        max_tokens: None,
        connect_timeout: Duration::from_secs(10),
        read_timeout: Duration::from_secs(300),
        max_retries: 2,
    }
}

fn provider(config: OllamaConfig) -> OllamaProvider {
    OllamaProvider::new("llama3.1:8b", config).unwrap()
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
        model: "ollama:llama3.1:8b".to_owned(),
        messages: vec![message(Role::User, prompt)],
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

async fn drain(
    provider: &OllamaProvider,
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
async fn thinking_streams_apart_from_the_text_and_is_kept_on_the_completion() {
    let server = MockServer::start(vec![Reply::Sse(fixture("ollama/thinking.ndjson"))]).await;
    let provider = provider(config(&server.base_url));

    let (deltas, error) = drain(&provider, request("Score?")).await;
    assert_eq!(error, None);
    let thinking: Vec<&str> = deltas
        .iter()
        .filter(|delta| !delta.done)
        .map(|delta| delta.thinking.as_str())
        .collect();
    assert_eq!(thinking, ["The Dream lead", " 51-28."]);
    assert!(deltas.iter().all(|delta| delta.text.is_empty()));
    let completion = deltas.last().unwrap().completion.clone().unwrap();
    assert_eq!(completion.text, "");
    assert_eq!(completion.thinking, "The Dream lead 51-28.");
    assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
}

#[tokio::test]
async fn shown_thinking_rules_out_a_retry() {
    let line = r#"{"model":"m","message":{"role":"assistant","content":"","thinking":"Hmm"},"done":false}"#;
    let server = MockServer::start(vec![
        Reply::Truncated(format!("{line}\n")),
        Reply::Sse(fixture("ollama/text.ndjson")),
    ])
    .await;
    let provider = provider(config(&server.base_url));

    let (deltas, error) = drain(&provider, request("Hi")).await;
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].thinking, "Hmm");
    assert!(error.is_some());
    assert_eq!(
        server.requests().len(),
        1,
        "shown thinking is never retried"
    );
}

#[tokio::test]
async fn streams_ndjson_text_then_a_final_completion_at_zero_cost() {
    let server = MockServer::start(vec![Reply::Sse(fixture("ollama/text.ndjson"))]).await;
    let provider = provider(config(&server.base_url));

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
    assert_eq!(completion.model.as_deref(), Some("llama3.1:8b"));
    assert_eq!(
        (
            completion.usage.input_tokens,
            completion.usage.output_tokens
        ),
        (26, 3)
    );
    assert_eq!(completion.usage.cost_usd, 0.0);

    let sent = &server.requests()[0];
    assert_eq!(sent.request_line, "POST /api/chat HTTP/1.1");
    assert_eq!(
        sent.body,
        json!({
            "model": "llama3.1:8b",
            "messages": [{"role": "user", "content": "Say hello"}],
            "stream": true,
            "options": {"temperature": 0.0, "num_ctx": 8192},
        })
    );
}

#[tokio::test]
async fn the_payload_carries_tools_tool_results_and_the_output_cap() {
    let server = MockServer::start(vec![Reply::Sse(fixture("ollama/text.ndjson"))]).await;
    let mut config = config(&format!("{}/", server.base_url));
    config.num_ctx = None;
    config.max_tokens = Some(512);
    let provider = provider(config);
    let mut call = ToolCall {
        id: "ollama-0".to_owned(),
        name: "read_file".to_owned(),
        arguments: serde_json::Map::new(),
    };
    call.arguments.insert("path".to_owned(), json!("a.txt"));
    let mut asked = message(Role::Assistant, "");
    asked.tool_calls = vec![call];
    let mut answered = message(Role::Tool, "contents");
    answered.tool_call_id = Some("ollama-0".to_owned());
    answered.name = Some("read_file".to_owned());
    let mut request = request("Read a.txt");
    request.messages.extend([asked, answered]);
    request.tools = vec![ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file.".to_owned(),
        parameters: json!({"type": "object"}),
        side_effecting: false,
    }];
    request.max_tokens = Some(64);

    provider.complete(request).await.unwrap();

    let body = &server.requests()[0].body;
    assert_eq!(
        body["options"],
        json!({"temperature": 0.0, "num_predict": 64}),
        "the request's cap wins over the section's"
    );
    assert_eq!(
        body["messages"][1],
        json!({"role": "assistant", "content": "",
               "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "a.txt"}}}]})
    );
    assert_eq!(
        body["messages"][2],
        json!({"role": "tool", "content": "contents", "name": "read_file"})
    );
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "function": {
            "name": "read_file", "description": "Read a file.", "parameters": {"type": "object"}}}])
    );
}

#[tokio::test]
async fn tool_calls_are_numbered_and_their_arguments_parsed() {
    let server = MockServer::start(vec![Reply::Sse(fixture("ollama/tools.ndjson"))]).await;
    let provider = provider(config(&server.base_url));

    let completion = provider.complete(request("Look around")).await.unwrap();

    let ids: Vec<&str> = completion
        .tool_calls
        .iter()
        .map(|call| call.id.as_str())
        .collect();
    assert_eq!(ids, ["ollama-0", "ollama-1", "ollama-2"]);
    assert_eq!(
        completion.tool_calls[0].arguments["path"],
        json!("README.md")
    );
    assert_eq!(completion.tool_calls[1].arguments["path"], json!("."));
    assert_eq!(
        completion.tool_calls[2].invalid_arguments(),
        Some("{\"path\": ")
    );
    // Ollama says `stop` on a turn that asked for tools.
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(completion.usage.input_tokens, 180);
}

#[tokio::test]
async fn errors_carry_python_s_messages() {
    let server = MockServer::start(vec![
        status(404, "  {\"error\":\"model 'nope' not found\"}\n"),
        Reply::Sse("{\"error\":\"out of memory\"}\n".to_owned()),
        Reply::Sse("{\"message\":{\"content\":\"Hi\"},\"done\":false}\n{\"mess\n".to_owned()),
    ])
    .await;
    let provider = provider(config(&server.base_url));
    let endpoint = format!("{}/api/chat", server.base_url);

    let missing = provider.complete(request("hi")).await.unwrap_err();
    assert_eq!(
        missing.to_string(),
        "Ollama returned HTTP 404: {\"error\":\"model 'nope' not found\"}"
    );
    let refused = provider.complete(request("hi")).await.unwrap_err();
    assert_eq!(refused.to_string(), "Ollama error: out of memory");

    let (deltas, malformed) = drain(&provider, request("hi")).await;
    assert_eq!(deltas.len(), 1, "the text before the bad line was streamed");
    let malformed = malformed.unwrap().to_string();
    assert!(
        malformed.starts_with(&format!(
            "ollama: {endpoint} returned a malformed NDJSON line for model 'llama3.1:8b' — "
        )),
        "{malformed}"
    );
    assert!(malformed
        .ends_with(". The stream was probably truncated; retry, or check the Ollama server log."));
    assert_eq!(server.requests().len(), 3, "none of these is retried");
}

#[tokio::test]
async fn transient_statuses_are_retried_but_never_after_text() {
    let busy = Reply::Status {
        status: 503,
        headers: vec![("Retry-After", "0".to_owned())],
        body: "loading".to_owned(),
    };
    let partial = "{\"message\":{\"content\":\"Half\"},\"done\":false}\n".to_owned();
    let server = MockServer::start(vec![
        busy,
        Reply::Sse(fixture("ollama/text.ndjson")),
        Reply::Truncated(partial),
        Reply::Sse(fixture("ollama/text.ndjson")),
    ])
    .await;
    let provider = provider(config(&server.base_url));

    let completion = provider.complete(request("hi")).await.unwrap();
    assert_eq!(completion.text, "Hello, world");
    assert_eq!(server.requests().len(), 2);

    let (deltas, error) = drain(&provider, request("hi")).await;
    assert_eq!(deltas.len(), 1);
    let error = error.unwrap().to_string();
    assert!(
        error.contains("failed while streaming model 'llama3.1:8b'")
            && error.ends_with("check that 'ollama serve' is healthy."),
        "{error}"
    );
    assert_eq!(server.requests().len(), 3, "emitted text is never retried");
}

#[tokio::test]
async fn stalls_and_a_missing_server_name_the_setting_and_the_fix() {
    let server = MockServer::start(vec![Reply::StallAfter(
        "{\"message\":{\"content\":\"Started\"},\"done\":false}\n".to_owned(),
    )])
    .await;
    let mut stalled = config(&server.base_url);
    stalled.read_timeout = Duration::from_millis(300);
    let (_, error) = drain(&provider(stalled), request("hi")).await;
    assert_eq!(
        error.unwrap().to_string(),
        format!(
            "Timed out waiting for {}/api/chat after 0.3s with no data received \
             (read_timeout_s). This is the maximum gap between streamed chunks, not the total \
             generation time — a long but still-progressing generation is not affected. Raise \
             read_timeout_s in [providers.ollama] if this endpoint is just slow to respond.",
            server.base_url
        )
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut gone = config(&base_url);
    gone.max_retries = 0;
    let error = provider(gone).complete(request("hi")).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Cannot reach Ollama at {base_url}. Is it running? Start it with 'ollama serve'.")
    );
}

#[test]
fn ollama_refs_get_the_ollama_adapter_without_a_key() {
    let paths = ConfigPaths {
        user: None,
        project: std::path::PathBuf::from("no-such-dir/config.toml"),
        project_trust: agent86::config::ProjectTrust::Trusted,
    };
    let config = load_config(&paths, &Overrides::default()).unwrap();
    let reference = ModelRef::parse("ollama:llama3.1:8b").unwrap();
    let provider = provider_for_ref_with_store(&reference, &config, &NoSecretStore).unwrap();
    assert_eq!(
        (provider.name(), provider.model()),
        ("ollama", "llama3.1:8b")
    );
}
