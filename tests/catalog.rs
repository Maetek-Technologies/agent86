//! Model catalogs and OpenRouter context lengths, fetched from the local mock server.

mod support;

use agent86::config::ProviderConfig;
use agent86::provider::catalog::{
    fetch_catalog, fetch_ollama, fetch_openai_compatible_context_lengths,
};
use std::time::Duration;
use support::{MockServer, Reply};

fn json(body: serde_json::Value) -> Reply {
    Reply::Status {
        status: 200,
        headers: vec![("Content-Type", "application/json".to_owned())],
        body: body.to_string(),
    }
}

fn section(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: Some(base_url.to_owned()),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn openai_compatible_catalogs_are_sorted_with_their_labels() {
    let server = MockServer::start(vec![json(serde_json::json!({"object": "list", "data": [
        {"id": "z-model", "name": "Zed"},
        {"id": "a-model"},
        {"name": "no id"},
    ]}))])
    .await;
    let base = format!("{}/v1/", server.base_url);
    let entries = tokio::task::spawn_blocking(move || {
        fetch_catalog("gateway", &section(&base), Some("sk-test-catalog-key-0123"))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        entries,
        [
            ("a-model".to_owned(), "a-model".to_owned()),
            ("z-model".to_owned(), "Zed".to_owned())
        ]
    );
    let request = &server.requests()[0];
    assert_eq!(request.request_line, "GET /v1/models HTTP/1.1");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-test-catalog-key-0123")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_and_empty_lists_are_unavailable_with_the_url() {
    let server = MockServer::start(vec![
        Reply::Status {
            status: 401,
            headers: vec![],
            body: "{\"error\": \"bad key sk-test-catalog-key-0123\"}".to_owned(),
        },
        json(serde_json::json!({"data": []})),
    ])
    .await;
    let base = format!("{}/v1", server.base_url);
    let (refused, empty) = tokio::task::spawn_blocking(move || {
        let key = Some("sk-test-catalog-key-0123");
        (
            fetch_catalog("gateway", &section(&base), key),
            fetch_catalog("gateway", &section(&base), key),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        refused.unwrap_err().0,
        format!(
            "Could not fetch models from {0}/v1/models: Client error '401 Unauthorized' for url \
             '{0}/v1/models'\nFor more information check: \
             https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/401",
            server.base_url
        )
    );
    assert_eq!(
        empty.unwrap_err().0,
        "Provider 'gateway' returned an empty model list."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ollama_lists_its_tags_and_openrouter_reports_context_lengths() {
    let server = MockServer::start(vec![
        json(serde_json::json!({"models": [{"name": "llama3.1:8b"}, {"name": "qwen3"}]})),
        json(serde_json::json!({"data": [
            {"id": "Vendor/Big", "context_length": 262144},
            {"id": "vendor/float", "context_length": 8192.0},
            {"id": "vendor/zero", "context_length": 0},
            {"id": "vendor/flag", "context_length": true},
            {"id": "vendor/none"},
        ]})),
    ])
    .await;
    let base = server.base_url.clone();
    let (tags, lengths) = tokio::task::spawn_blocking(move || {
        (
            fetch_ollama(&format!("{base}/")),
            fetch_openai_compatible_context_lengths(&base, None, Duration::from_secs(3)),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        tags.unwrap(),
        [
            ("llama3.1:8b".to_owned(), "llama3.1:8b".to_owned()),
            ("qwen3".to_owned(), "qwen3".to_owned())
        ]
    );
    let requests = server.requests();
    assert_eq!(requests[0].request_line, "GET /api/tags HTTP/1.1");
    assert_eq!(requests[0].header("authorization"), None);
    let lengths = lengths.unwrap();
    assert_eq!(lengths.len(), 2);
    assert_eq!(lengths["vendor/big"], 262_144);
    assert_eq!(lengths["vendor/float"], 8192);
}
