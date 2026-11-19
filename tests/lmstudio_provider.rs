//! The `lmstudio` provider (beyond Python): the OpenAI-compatible adapter under
//! LM Studio's name, with an optional key, the model list and the loaded context
//! length from LM Studio's native REST API, local pricing, and hints. Every test
//! runs against the local mock server with fixtures in the documented shape of
//! `GET /api/v1/models` (`tests/fixtures/lmstudio`), a fake key store, and key
//! variables nothing sets.

#[path = "../crates/agent86-core/tests/support/mod.rs"]
mod support;

use agent86::config::{load_config, Config, ConfigPaths, Overrides, ProjectTrust};
use agent86::provider::catalog::fetch_catalog;
use agent86::provider::remote_windows::lmstudio_context_window;
use agent86::provider::{provider_for_ref_with_store, Provider, ProviderError};
use agent86::secrets::NoSecretStore;
use agent86::types::{CompletionRequest, Message, ModelRef, Role};
use futures_util::StreamExt;
use serde_json::Value;
use support::{fixture, sse_text, MockServer, Reply};

const UNSET: &str = "AGENT86_TEST_UNSET_LMSTUDIO_TOKEN";

fn json_reply(body: String) -> Reply {
    Reply::Status {
        status: 200,
        headers: Vec::new(),
        body,
    }
}

fn status(status: u16, body: &str) -> Reply {
    Reply::Status {
        status,
        headers: Vec::new(),
        body: body.to_owned(),
    }
}

/// The native list with every model unloaded, as before the first request.
fn unloaded() -> String {
    let mut models: Value = serde_json::from_str(&fixture("lmstudio/models.json")).unwrap();
    for model in models["models"].as_array_mut().unwrap() {
        model["loaded_instances"] = Value::Array(Vec::new());
    }
    models.to_string()
}

/// The built-in `lmstudio` section pointed at `server`.
fn lmstudio_config(server: &MockServer) -> Config {
    let paths = ConfigPaths {
        user: None,
        project: std::path::PathBuf::from("no-such-dir/config.toml"),
        project_trust: ProjectTrust::Trusted,
    };
    let mut config = load_config(&paths, &Overrides::default()).unwrap();
    let section = config.providers.get_mut("lmstudio").unwrap();
    section.base_url = Some(format!("{}/v1", server.base_url.trim_end_matches("/v1")));
    section.api_key_env = Some(UNSET.to_owned());
    section.max_retries = 0;
    config
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "lmstudio:qwen/qwen3-8b".to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: "Hi".to_owned(),
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

async fn first_error(provider: &dyn Provider) -> Option<ProviderError> {
    let mut stream = provider.stream(request());
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            return Some(error);
        }
    }
    None
}

// The catalog and window fetches block on a thread of their own while the mock
// server answers on this runtime, so these tests need a second worker.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catalog_lists_chat_models_with_what_is_loaded_and_what_lacks_tools() {
    let server = MockServer::start(vec![json_reply(fixture("lmstudio/models.json"))]).await;
    let config = lmstudio_config(&server);
    let section = config.providers["lmstudio"].clone();
    let entries =
        tokio::task::spawn_blocking(move || fetch_catalog("lmstudio", &section, None).unwrap())
            .await
            .unwrap();
    assert_eq!(
        entries,
        [
            (
                "google/gemma-4-26b-a4b".to_owned(),
                "Gemma 4 26B A4B".to_owned()
            ),
            ("qwen/qwen3-8b".to_owned(), "Qwen3 8B · loaded".to_owned()),
            (
                "tiny/storyteller-1b".to_owned(),
                "Storyteller 1B · no tool use".to_owned()
            ),
        ],
        "the embedding model is left out"
    );
    let requests = server.requests();
    assert!(
        requests[0].request_line.starts_with("GET /api/v1/models "),
        "the native API, without /v1: {}",
        requests[0].request_line
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_native_api_the_catalog_falls_back_to_v1_models() {
    let server = MockServer::start(vec![
        status(404, "Not Found"),
        json_reply(fixture("lmstudio/v1_models.json")),
    ])
    .await;
    let config = lmstudio_config(&server);
    let section = config.providers["lmstudio"].clone();
    let entries =
        tokio::task::spawn_blocking(move || fetch_catalog("lmstudio", &section, None).unwrap())
            .await
            .unwrap();
    let ids: Vec<&str> = entries.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["google/gemma-4-26b-a4b", "qwen/qwen3-8b"]);
    assert!(server.requests()[1]
        .request_line
        .starts_with("GET /v1/models "));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_window_is_the_loaded_length_or_a_safe_guess_before_loading() {
    let server = MockServer::start(vec![
        json_reply(fixture("lmstudio/models.json")),
        json_reply(fixture("lmstudio/models.json")),
        json_reply(fixture("lmstudio/models.json")),
        json_reply(fixture("lmstudio/models.json")),
    ])
    .await;
    let config = lmstudio_config(&server);
    let window = |model: &'static str| {
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            lmstudio_context_window("lmstudio", model, &config, &NoSecretStore)
        })
    };
    // Loaded with 4096 of its 40960: the loaded length is what the server takes.
    assert_eq!(window("qwen/qwen3-8b").await.unwrap(), Some(4_096));
    // Not loaded: the smaller of its maximum and the default window.
    assert_eq!(window("google/gemma-4-26b-a4b").await.unwrap(), Some(8_192));
    assert_eq!(window("tiny/storyteller-1b").await.unwrap(), Some(4_000));
    assert_eq!(window("not/listed").await.unwrap(), None);
    // Asked every time: reloading the model in LM Studio changes it.
    assert_eq!(server.requests().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_provider_learns_the_loaded_length_once_a_request_has_loaded_the_model() {
    let server = MockServer::start(vec![
        json_reply(unloaded()),
        sse_text("Hello!", "stop"),
        json_reply(fixture("lmstudio/models.json")),
    ])
    .await;
    let config = lmstudio_config(&server);
    let provider: std::sync::Arc<dyn Provider> = provider_for_ref_with_store(
        &ModelRef::parse("lmstudio:qwen/qwen3-8b").unwrap(),
        &config,
        &NoSecretStore,
    )
    .unwrap()
    .into();
    assert_eq!(provider.name(), "lmstudio");

    let live = |provider: std::sync::Arc<dyn Provider>| {
        tokio::task::spawn_blocking(move || provider.live_context_window())
    };
    assert_eq!(
        live(provider.clone()).await.unwrap(),
        None,
        "not loaded yet"
    );
    assert_eq!(first_error(&*provider).await, None);
    assert_eq!(live(provider.clone()).await.unwrap(), Some(4_096));
    assert_eq!(live(provider.clone()).await.unwrap(), Some(4_096));
    assert_eq!(server.requests().len(), 3, "kept once learned");

    // `[model.context_window]` wins: nothing is asked.
    let mut config = config;
    config
        .model
        .context_window
        .insert("lmstudio:qwen/qwen3-8b".into(), 16_000);
    let pinned = provider_for_ref_with_store(
        &ModelRef::parse("lmstudio:qwen/qwen3-8b").unwrap(),
        &config,
        &NoSecretStore,
    )
    .unwrap();
    assert_eq!(pinned.live_context_window(), None);
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn lm_studios_errors_keep_their_text_and_gain_a_hint() {
    let cases = [
        (
            500,
            r#"{"error":"Context size has been exceeded."}"#,
            "reload it with a larger context length",
        ),
        (
            400,
            r#"{"error":{"type":"model_not_found","message":"Model qwen3 not found in downloaded models"}}"#,
            "lms get",
        ),
        (401, r#"{"error":"Unauthorized"}"#, "LM_API_TOKEN"),
    ];
    for (code, body, needle) in cases {
        let server = MockServer::start(vec![status(code, body)]).await;
        let provider = provider_for_ref_with_store(
            &ModelRef::parse("lmstudio:qwen/qwen3-8b").unwrap(),
            &lmstudio_config(&server),
            &NoSecretStore,
        )
        .unwrap();
        let error = first_error(&*provider).await;
        let Some(ProviderError::HttpHint {
            status,
            body: sent,
            hint,
        }) = &error
        else {
            panic!("{code}: {error:?}");
        };
        assert_eq!((*status, sent.as_str()), (code, body));
        // A 401 names the section's own variable.
        let needle = if code == 401 { UNSET } else { needle };
        assert!(hint.contains(needle), "{code}: {hint}");
    }
}

#[tokio::test]
async fn the_key_is_optional() {
    let server = MockServer::start(vec![sse_text("Hello!", "stop")]).await;
    let provider = provider_for_ref_with_store(
        &ModelRef::parse("lmstudio:qwen/qwen3-8b").unwrap(),
        &lmstudio_config(&server),
        &NoSecretStore,
    )
    .unwrap();
    assert_eq!(first_error(&*provider).await, None);
    assert_eq!(server.requests()[0].header("authorization"), None);
    assert!(server.requests()[0]
        .request_line
        .starts_with("POST /v1/chat/completions "));
}
