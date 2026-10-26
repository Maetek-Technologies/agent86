//! API keys resolve from the environment first, then the keyring, as in Python.
//!
//! These tests use an in-memory store; they never read the real OS keyring.

mod support;

use agent86::config::{load_config, ConfigPaths, Overrides};
use agent86::provider::{provider_for_ref_with_store, ProviderError};
use agent86::secrets::{NoSecretStore, SecretStore, SERVICE_NAME};
use agent86::types::ModelRef;
use support::{fixture, MockServer, Reply};

const STORED_KEY: &str = "sk-stored-0123456789abcdefghij";

struct OneKey {
    account: &'static str,
}

impl SecretStore for OneKey {
    fn get_password(&self, service: &str, account: &str) -> Option<String> {
        (service == SERVICE_NAME && account == self.account).then(|| STORED_KEY.to_owned())
    }
}

fn config_for(server: &MockServer, name: &str) -> agent86::config::Config {
    let dir = std::env::temp_dir().join(format!("agent86-keys-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let project = dir.join("config.toml");
    std::fs::write(
        &project,
        format!(
            "[providers.mock]\nbase_url = \"{}\"\napi_key_env = \"AGENT86_TEST_KEYRING_ONLY\"\nmax_retries = 0\n",
            server.base_url
        ),
    )
    .unwrap();
    let paths = ConfigPaths {
        user: None,
        project,
    };
    load_config(&paths, &Overrides::default()).unwrap()
}

#[tokio::test]
async fn a_key_stored_under_the_section_name_is_sent() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let config = config_for(&server, "stored");
    let reference = ModelRef::parse("mock:test/model").unwrap();

    let provider =
        provider_for_ref_with_store(&reference, &config, &OneKey { account: "mock" }).unwrap();
    provider
        .complete(agent86::types::CompletionRequest {
            model: "test/model".to_owned(),
            messages: Vec::new(),
            tools: Vec::new(),
            temperature: 0.0,
            max_tokens: None,
            stream: true,
        })
        .await
        .unwrap();

    let sent = &server.requests()[0];
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {STORED_KEY}").as_str())
    );
}

#[tokio::test]
async fn keys_for_other_accounts_are_not_used() {
    let server = MockServer::start(Vec::new()).await;
    let config = config_for(&server, "other");
    let reference = ModelRef::parse("mock:test/model").unwrap();

    let error = provider_for_ref_with_store(&reference, &config, &OneKey { account: "groq" })
        .err()
        .unwrap();
    assert!(
        matches!(error, ProviderError::MissingApiKey { .. }),
        "{error:?}"
    );

    let error = provider_for_ref_with_store(&reference, &config, &NoSecretStore)
        .err()
        .unwrap();
    assert!(
        matches!(error, ProviderError::MissingApiKey { .. }),
        "{error:?}"
    );
}
