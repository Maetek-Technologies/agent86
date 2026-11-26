//! The `vllm` provider (beyond Python): the OpenAI-compatible adapter under
//! vLLM's name, with an optional key, the window from `max_model_len`, local
//! pricing, and hints for the errors vLLM sends. Every test runs against the
//! local mock server with fixtures built from vLLM's source (`tests/fixtures/vllm`),
//! a fake key store, and key variables nothing sets.

#[path = "../crates/agent86-core/tests/support/mod.rs"]
mod support;

use agent86::config::{load_config, Config, ConfigPaths, Overrides, ProjectTrust, ProviderConfig};
use agent86::provider::remote_windows::vllm_context_window;
use agent86::provider::{
    provider_for_ref_with_key, provider_for_ref_with_store, Provider, ProviderError,
};
use agent86::secrets::{NoSecretStore, SecretStore};
use agent86::types::{CompletionDelta, CompletionRequest, Message, ModelRef, Role};
use futures_util::StreamExt;
use support::{fixture, MockServer, Reply};

/// A key variable no environment sets, so only the store decides.
const UNSET: &str = "AGENT86_TEST_UNSET_VLLM_KEY";
const TEST_KEY: &str = "vllm-test-key-0123456789";

/// Holds a key for one account, as the OS keyring would.
struct OneKey(&'static str);

impl SecretStore for OneKey {
    fn get_password(&self, _service: &str, account: &str) -> Option<String> {
        (account == self.0).then(|| TEST_KEY.to_owned())
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

/// The built-in `vllm` section pointed at `server`.
fn vllm_config(server: &MockServer) -> Config {
    let mut config = builtin_config();
    let section = config.providers.get_mut("vllm").unwrap();
    section.base_url = Some(server.base_url.clone());
    section.api_key_env = Some(UNSET.to_owned());
    section.max_retries = 0;
    config
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "vllm:Qwen/Qwen3-8B".to_owned(),
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

async fn drain(provider: &dyn Provider) -> (Vec<CompletionDelta>, Option<ProviderError>) {
    let mut stream = provider.stream(request());
    let mut deltas = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(delta) => deltas.push(delta),
            Err(error) => return (deltas, Some(error)),
        }
    }
    (deltas, None)
}

fn status_reply(name: &str) -> Reply {
    let errors: serde_json::Value = serde_json::from_str(&fixture("vllm/errors.json")).unwrap();
    Reply::Status {
        status: errors[name]["status"].as_u64().unwrap() as u16,
        headers: Vec::new(),
        body: errors[name]["body"].as_str().unwrap().to_owned(),
    }
}

fn model(reference: &str) -> ModelRef {
    ModelRef::parse(reference).unwrap()
}

#[tokio::test]
async fn the_key_is_optional_and_sent_only_when_one_resolves() {
    let server = MockServer::start(vec![
        Reply::Sse(fixture("vllm/reasoning.sse")),
        Reply::Sse(fixture("vllm/reasoning.sse")),
    ])
    .await;
    let config = vllm_config(&server);

    // No key anywhere: the provider builds and the request carries no header.
    let open =
        provider_for_ref_with_store(&model("vllm:Qwen/Qwen3-8B"), &config, &NoSecretStore).unwrap();
    assert_eq!(open.name(), "vllm");
    assert_eq!(drain(&*open).await.1, None);

    // A stored key (account `vllm`) is sent as a bearer token.
    let keyed = provider_for_ref_with_store(&model("vllm:Qwen/Qwen3-8B"), &config, &OneKey("vllm"))
        .unwrap();
    assert_eq!(drain(&*keyed).await.1, None);

    let requests = server.requests();
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(
        requests[1].header("authorization"),
        Some(format!("Bearer {TEST_KEY}").as_str())
    );
    assert!(requests[0].request_line.contains("/v1/chat/completions"));
}

#[tokio::test]
async fn any_section_with_adapter_vllm_is_a_vllm_server() {
    let server = MockServer::start(vec![Reply::Sse(fixture("vllm/reasoning.sse"))]).await;
    let mut config = builtin_config();
    config.providers.insert(
        "gpu-box".into(),
        ProviderConfig {
            base_url: Some(server.base_url.clone()),
            api_key_env: Some(UNSET.to_owned()),
            adapter: Some("vllm".into()),
            ..Default::default()
        },
    );
    let provider =
        provider_for_ref_with_store(&model("gpu-box:qwen3"), &config, &OneKey("gpu-box")).unwrap();
    assert_eq!(provider.name(), "vllm");
    let (deltas, error) = drain(&*provider).await;
    assert_eq!(error, None);
    let completion = deltas.last().unwrap().completion.clone().unwrap();
    // Priced as local, so the cost is a real zero.
    assert_eq!(completion.usage.cost_usd, 0.0);
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {TEST_KEY}").as_str()),
        "the section's own keyring account"
    );

    // Without a base_url it reaches vLLM's default port.
    config.providers.get_mut("gpu-box").unwrap().base_url = None;
    assert!(provider_for_ref_with_store(&model("gpu-box:qwen3"), &config, &NoSecretStore).is_ok());
}

#[test]
fn an_unknown_adapter_is_refused() {
    let mut config = builtin_config();
    config.providers.insert(
        "box".into(),
        ProviderConfig {
            base_url: Some("http://127.0.0.1:9/v1".into()),
            adapter: Some("tgi".into()),
            ..Default::default()
        },
    );
    let Err(error) = provider_for_ref_with_store(&model("box:m"), &config, &NoSecretStore) else {
        panic!("an unknown adapter must not build");
    };
    assert!(matches!(error, ProviderError::InvalidSetting { .. }));
    assert!(error.to_string().contains("adapter must be \"vllm\""));
}

#[tokio::test]
async fn reasoning_arrives_under_either_name_vllm_has_used() {
    for name in ["vllm/reasoning.sse", "vllm/reasoning_content.sse"] {
        let server = MockServer::start(vec![Reply::Sse(fixture(name))]).await;
        let provider = provider_for_ref_with_store(
            &model("vllm:Qwen/Qwen3-8B"),
            &vllm_config(&server),
            &NoSecretStore,
        )
        .unwrap();
        let (deltas, error) = drain(&*provider).await;
        assert_eq!(error, None, "{name}");
        let completion = deltas.last().unwrap().completion.clone().unwrap();
        assert_eq!(
            completion.thinking, "The user greets me. Reply briefly.",
            "{name}"
        );
        assert_eq!(completion.text, "Hello!", "{name}");
        assert_eq!(
            (
                completion.usage.input_tokens,
                completion.usage.output_tokens
            ),
            (12, 12)
        );
    }
}

#[tokio::test]
async fn vllms_errors_keep_their_text_and_gain_a_hint() {
    let cases = [
        ("tool_choice", "--enable-auto-tool-choice"),
        ("context_length", "--max-model-len"),
        ("unauthorized", "agent86 keys set vllm"),
        ("unknown_model", "--served-model-name"),
    ];
    for (name, needle) in cases {
        let server = MockServer::start(vec![status_reply(name)]).await;
        let provider = provider_for_ref_with_store(
            &model("vllm:Qwen/Qwen3-8B"),
            &vllm_config(&server),
            &NoSecretStore,
        )
        .unwrap();
        let (_, error) = drain(&*provider).await;
        let error = error.unwrap_or_else(|| panic!("{name}: no error"));
        let ProviderError::HttpHint { status, body, hint } = &error else {
            panic!("{name}: {error:?}");
        };
        let errors: serde_json::Value = serde_json::from_str(&fixture("vllm/errors.json")).unwrap();
        assert_eq!(u64::from(*status), errors[name]["status"].as_u64().unwrap());
        assert_eq!(
            body,
            errors[name]["body"].as_str().unwrap(),
            "{name}: the body as sent"
        );
        assert!(hint.contains(needle), "{name}: {hint}");
        assert!(error.to_string().starts_with(&format!("HTTP {status}: ")));
    }
}

#[tokio::test]
async fn a_plain_openai_compatible_section_gets_no_vllm_hint() {
    let server = MockServer::start(vec![status_reply("tool_choice")]).await;
    let mut config = builtin_config();
    config.providers.insert(
        "plain".into(),
        ProviderConfig {
            base_url: Some(server.base_url.clone()),
            max_retries: 0,
            ..Default::default()
        },
    );
    let provider = provider_for_ref_with_store(&model("plain:m"), &config, &NoSecretStore).unwrap();
    let (_, error) = drain(&*provider).await;
    assert!(
        matches!(error, Some(ProviderError::Http { status: 400, .. })),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_error_mid_stream_is_shown_not_dropped() {
    let server = MockServer::start(vec![Reply::Sse(fixture("vllm/stream_error.sse"))]).await;
    let provider = provider_for_ref_with_store(
        &model("vllm:Qwen/Qwen3-8B"),
        &vllm_config(&server),
        &NoSecretStore,
    )
    .unwrap();
    let (_, error) = drain(&*provider).await;
    let Some(ProviderError::HttpHint { status, body, hint }) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(*status, 400);
    assert!(body.contains("maximum context length is 8192 tokens"));
    assert!(hint.contains("[model] context_window"));
}

// The window fetch blocks on a thread of its own while the mock server answers on
// this runtime, so the runtime needs a second worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_window_is_the_served_models_max_model_len() {
    let server = MockServer::start(vec![Reply::Status {
        status: 200,
        headers: Vec::new(),
        body: fixture("vllm/models.json"),
    }])
    .await;
    let config = vllm_config(&server);
    let lookup = |model: &'static str| {
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            vllm_context_window("vllm", model, &config, &NoSecretStore)
        })
    };
    assert_eq!(lookup("Qwen/Qwen3-8B").await.unwrap(), Some(32_768));
    // Another served name has its own card; a LoRA adapter's null is unknown;
    // all from the one request, memoized.
    assert_eq!(lookup("qwen3").await.unwrap(), Some(32_768));
    assert_eq!(lookup("sql-lora").await.unwrap(), None);
    assert_eq!(lookup("missing").await.unwrap(), None);
    assert_eq!(server.requests().len(), 1);
    assert!(server.requests()[0].request_line.contains("GET /v1/models"));

    // Only vLLM sections are asked.
    assert_eq!(
        vllm_context_window("openai", "gpt-4o", &config, &NoSecretStore),
        None
    );
}

#[test]
fn a_sections_temperature_reaches_every_adapter_but_geminis() {
    let mut config = builtin_config();
    config.providers.insert(
        "box".into(),
        ProviderConfig {
            base_url: Some("http://127.0.0.1:9/v1".into()),
            adapter: Some("vllm".into()),
            ..Default::default()
        },
    );
    for name in ["box", "anthropic", "ollama", "google", "openai"] {
        config.providers.entry(name.into()).or_default().temperature = Some(0.7);
    }
    for (reference, expected) in [
        ("box:qwen3", Some(0.7)),
        ("anthropic:claude-sonnet-4-6", Some(0.7)),
        ("ollama:qwen3", Some(0.7)),
        ("openai:gpt-5", Some(0.7)),
        // Gemini takes no sampling settings.
        ("google:gemini-4-pro", None),
    ] {
        let provider = provider_for_ref_with_key(&model(reference), &config, "k").unwrap();
        assert_eq!(provider.temperature(), expected, "{reference}");
        assert_eq!(
            provider.fresh().unwrap().temperature(),
            expected,
            "{reference}"
        );
    }
    // Unset is the default, 0.0, decided by the harness.
    config.providers.get_mut("openai").unwrap().temperature = None;
    let plain = provider_for_ref_with_key(&model("openai:gpt-5"), &config, "k").unwrap();
    assert_eq!(plain.temperature(), None);

    config.providers.get_mut("box").unwrap().temperature = Some(2.5);
    let Err(error) = provider_for_ref_with_store(&model("box:m"), &config, &NoSecretStore) else {
        panic!("a temperature out of range must not build");
    };
    assert!(matches!(error, ProviderError::InvalidSetting { .. }));
    assert!(error
        .to_string()
        .contains("temperature must be between 0 and 2"));
}
