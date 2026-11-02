//! `web_fetch` never goes through a proxy (beyond Python). A proxy resolves the
//! host itself, so it would reach whatever DNS answers next instead of the
//! addresses the SSRF guard vetted.
//!
//! In a file of its own: it sets `HTTP_PROXY` for the whole process.

mod support;

use agent86::tools::{builtin_registry, ToolContext};
use agent86::types::ToolCall;
use serde_json::{json, Value};
use support::{MockServer, Reply};

fn page(body: &str) -> Reply {
    Reply::Raw {
        status: 200,
        headers: vec![("Content-Type", "text/plain".to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn proxy_variables_are_ignored() {
    let target = MockServer::start(vec![page("direct")]).await;
    let proxy = MockServer::start(vec![page("through the proxy")]).await;
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
    ] {
        std::env::set_var(name, &proxy.base_url);
    }
    for name in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(name);
    }

    let root = std::env::temp_dir().join(format!("agent86-web-proxy-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut ctx = ToolContext::new(&root).unwrap();
    ctx.web.allow_private = true;
    let url = format!("{}/page", target.base_url);
    let result = tokio::task::spawn_blocking(move || {
        let Value::Object(arguments) = json!({ "url": url }) else {
            unreachable!()
        };
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "web_fetch".to_owned(),
            arguments,
        };
        builtin_registry().dispatch(&call, &ctx)
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(&root);

    assert!(result.ok, "{result:?}");
    assert!(result.content.contains("direct"), "{}", result.content);
    assert!(proxy.requests().is_empty(), "the proxy was used");
}
