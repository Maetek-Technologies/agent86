//! `web_fetch`: SSRF vetting, redirects, content handling, and HTML reduction.

mod support;

use agent86::tools::web::{block_reason, html_to_text};
use agent86::tools::{builtin_registry, ToolContext};
use agent86::types::{ToolCall, ToolResult};
use serde_json::{json, Value};
use std::net::IpAddr;
use std::path::PathBuf;
use support::{fixture, MockServer, Reply};

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-web-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    /// A context that may reach the local mock server.
    fn local(&self) -> ToolContext {
        let mut ctx = ToolContext::new(&self.root).unwrap();
        ctx.web.allow_private = true;
        ctx
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn fetch(ctx: ToolContext, url: String) -> ToolResult {
    tokio::task::spawn_blocking(move || {
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
    .unwrap()
}

fn raw(status: u16, headers: &[(&'static str, &str)], body: &[u8]) -> Reply {
    Reply::Raw {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect(),
        body: body.to_vec(),
    }
}

fn golden() -> Value {
    serde_json::from_str(&fixture("web/golden.json")).unwrap()
}

/// Where the guard deliberately blocks what Python 3.12's `ipaddress` lets through
/// (see the plan): the shared address space, which holds Alibaba Cloud's metadata
/// service, and the 6to4 relay anycast range.
const BEYOND_PYTHON: &[(&str, &str)] = &[
    (
        "100.64.0.1",
        "shared (carrier-grade NAT and cloud metadata range) address 100.64.0.1",
    ),
    (
        "100.100.100.200",
        "shared (carrier-grade NAT and cloud metadata range) address 100.100.100.200",
    ),
    (
        "100.127.255.255",
        "shared (carrier-grade NAT and cloud metadata range) address 100.127.255.255",
    ),
    (
        "::ffff:100.100.100.200",
        "shared (carrier-grade NAT and cloud metadata range) address 100.100.100.200 via ::ffff:6464:64c8",
    ),
    ("192.88.99.1", "reserved address 192.88.99.1"),
];

#[test]
fn addresses_are_classified_like_python_ipaddress() {
    let golden = golden();
    for (address, expected) in golden["classify"].as_object().unwrap() {
        if BEYOND_PYTHON.iter().any(|(differs, _)| differs == address) {
            continue;
        }
        let ip: IpAddr = address.parse().unwrap();
        assert_eq!(block_reason(ip).as_deref(), expected.as_str(), "{address}");
    }
}

#[test]
fn the_shared_address_space_and_6to4_relays_are_blocked_beyond_python() {
    for (address, expected) in BEYOND_PYTHON {
        let ip: IpAddr = address.parse().unwrap();
        assert_eq!(block_reason(ip).as_deref(), Some(*expected), "{address}");
    }
    // The edges of the shared range stay public.
    for address in [
        "100.63.255.255",
        "100.128.0.0",
        "192.88.98.255",
        "192.88.100.0",
    ] {
        let ip: IpAddr = address.parse().unwrap();
        assert_eq!(block_reason(ip), None, "{address}");
    }
}

#[test]
fn html_is_reduced_like_the_python_regex_reducer() {
    for page in golden()["html"].as_array().unwrap() {
        let html = page["html"].as_str().unwrap();
        assert_eq!(html_to_text(html), page["text"].as_str().unwrap(), "{html}");
    }
}

#[test]
fn a_page_of_unclosed_tags_is_reduced_in_linear_time() {
    // About 2 MB, the body cap: before, each unclosed tag scanned to the end.
    let mut html = String::from("<p>kept</p>");
    while html.len() < 2_000_000 {
        html.push_str("<form><table>x");
    }
    let started = std::time::Instant::now();
    let text = html_to_text(&html);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    assert!(text.starts_with("kept"), "{}", &text[..40.min(text.len())]);
    // A tag that does close is still dropped with what it holds.
    assert_eq!(
        html_to_text("<form>a<form>b</form>c<script>x</script>d"),
        "c d"
    );
}

#[test]
fn the_spec_matches_pydantic_and_is_read_only() {
    let registry = builtin_registry();
    let tool = registry.get("web_fetch").unwrap();
    assert!(!tool.side_effecting());
    let expected: Value = serde_json::from_str(&fixture("web/schema.json")).unwrap();
    // Compared as re-serialized text, so key order is checked too.
    assert_eq!(
        serde_json::to_string(&tool.parameters()).unwrap(),
        serde_json::to_string(&expected).unwrap()
    );
}

#[tokio::test]
async fn non_http_and_private_targets_are_refused() {
    let workspace = Workspace::new("refuse");
    let ctx = || ToolContext::new(&workspace.root).unwrap();
    let error = |result: ToolResult| {
        assert!(!result.ok, "{result:?}");
        result.error.unwrap()
    };

    assert_eq!(
        error(fetch(ctx(), "ftp://example.com/file".into()).await),
        "Refusing to fetch 'ftp://example.com/file': only http and https URLs are allowed (got 'ftp')."
    );
    assert_eq!(
        error(fetch(ctx(), "example.com/page".into()).await),
        "Refusing to fetch 'example.com/page': only http and https URLs are allowed (got 'no scheme')."
    );
    assert_eq!(
        error(fetch(ctx(), "http://".into()).await),
        "Refusing to fetch 'http://': no host in URL."
    );
    assert_eq!(
        error(fetch(ctx(), "http://127.0.0.1:9/".into()).await),
        "Refusing to fetch 'http://127.0.0.1:9/': host '127.0.0.1' resolves to a loopback address \
         127.0.0.1, which is inside the local trust boundary. Set [tools] web_allow_private = true \
         (or tools.web_allow_private in config) to allow private/loopback targets."
    );
    let metadata = error(fetch(ctx(), "http://169.254.169.254/latest/meta-data/".into()).await);
    assert!(
        metadata
            .contains("resolves to a link-local (cloud metadata range) address 169.254.169.254,"),
        "{metadata}"
    );
    let mapped = error(fetch(ctx(), "http://[::ffff:169.254.169.254]/".into()).await);
    assert!(
        mapped.contains(
            "link-local (cloud metadata range) address 169.254.169.254 via ::ffff:a9fe:a9fe"
        ),
        "{mapped}"
    );
    let localhost = error(fetch(ctx(), "http://localhost/".into()).await);
    assert!(
        localhost.starts_with("Refusing to fetch 'http://localhost/': host 'localhost' resolves to a loopback address"),
        "{localhost}"
    );
}

#[tokio::test]
async fn html_pages_are_fetched_and_reduced() {
    let page = b"<html><body><nav>Menu</nav><main><h1>Title</h1><p>Body &amp; text.</p></main></body></html>";
    let server = MockServer::start(vec![raw(
        200,
        &[("Content-Type", "text/html; charset=utf-8")],
        page,
    )])
    .await;
    let workspace = Workspace::new("html");
    let url = format!("{}/page", server.base_url);

    let result = fetch(workspace.local(), url.clone()).await;

    assert!(result.ok, "{result:?}");
    assert_eq!(
        result.content,
        format!("HTTP 200 {url} (text/html; charset=utf-8)\n\nTitle  Body & text.")
    );
    let sent = &server.requests()[0];
    assert_eq!(sent.request_line, "GET /page HTTP/1.1");
    assert_eq!(
        sent.header("user-agent"),
        Some(
            format!(
                "agent86/{} (+https://github.com/Maetek-Technologies/agent86)",
                env!("CARGO_PKG_VERSION")
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn json_passes_through_and_binary_is_refused() {
    let server = MockServer::start(vec![
        raw(
            200,
            &[("Content-Type", "application/json")],
            br#"{"ok": true}"#,
        ),
        raw(200, &[("Content-Type", "image/png")], b"\x89PNG"),
    ])
    .await;
    let workspace = Workspace::new("types");

    let json = fetch(workspace.local(), format!("{}/data", server.base_url)).await;
    assert!(
        json.content
            .ends_with("(application/json)\n\n{\"ok\": true}"),
        "{json:?}"
    );

    let url = format!("{}/image", server.base_url);
    let image = fetch(workspace.local(), url.clone()).await;
    assert_eq!(
        image.error.as_deref(),
        Some(
            format!(
                "Refusing to read '{url}': content type 'image/png' is not text. \
             web_fetch only returns text/HTML/JSON/XML content."
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn http_errors_keep_the_page_but_fail_the_call() {
    let server = MockServer::start(vec![raw(
        404,
        &[("Content-Type", "text/plain")],
        b"not here",
    )])
    .await;
    let workspace = Workspace::new("404");
    let url = format!("{}/missing", server.base_url);

    let result = fetch(workspace.local(), url.clone()).await;

    assert!(!result.ok);
    assert_eq!(
        result.error.as_deref(),
        Some(format!("HTTP 404 ({url})").as_str())
    );
    assert_eq!(
        result.content,
        format!("HTTP 404 {url} (text/plain)\n\nnot here")
    );
}

#[tokio::test]
async fn redirects_are_followed_and_bounded() {
    let server = MockServer::start(vec![
        raw(302, &[("Location", "/final")], b""),
        raw(200, &[("Content-Type", "text/plain")], b"arrived"),
    ])
    .await;
    let workspace = Workspace::new("redirect");

    let result = fetch(workspace.local(), format!("{}/start", server.base_url)).await;
    assert_eq!(
        result.content,
        format!("HTTP 200 {}/final (text/plain)\n\narrived", server.base_url)
    );

    let looping = MockServer::start(vec![raw(301, &[("Location", "/same")], b"")]).await;
    let url = format!("{}/same", looping.base_url);
    let result = fetch(workspace.local(), url.clone()).await;
    assert_eq!(
        result.error.as_deref(),
        Some(format!("Refusing to fetch '{url}': redirect loop at '{url}'.").as_str())
    );

    let hops: Vec<Reply> = (1..=6)
        .map(|hop| Reply::Raw {
            status: 307,
            headers: vec![("Location", format!("/hop{hop}"))],
            body: Vec::new(),
        })
        .collect();
    let chain = MockServer::start(hops).await;
    let start = format!("{}/hop0", chain.base_url);
    let result = fetch(workspace.local(), start.clone()).await;
    assert_eq!(
        result.error.as_deref(),
        Some(
            format!(
                "Refusing to fetch '{start}': more than 5 redirects (last: '{}/hop6').",
                chain.base_url
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn long_text_and_huge_bodies_are_capped() {
    let long = "word ".repeat(3000);
    let huge = vec![b'x'; 2_100_000];
    let server = MockServer::start(vec![
        raw(200, &[("Content-Type", "text/plain")], long.as_bytes()),
        raw(200, &[("Content-Type", "text/plain")], &huge),
    ])
    .await;
    let workspace = Workspace::new("caps");

    let capped = fetch(workspace.local(), format!("{}/long", server.base_url)).await;
    assert!(
        capped
            .content
            .ends_with("word\n\n... [truncated to 8000 chars]"),
        "{}",
        &capped.content[capped.content.len() - 60..]
    );

    let mut ctx = workspace.local();
    ctx.web.max_chars = 0;
    ctx.policy.max_output_chars = 3_000_000;
    let huge = fetch(ctx, format!("{}/huge", server.base_url)).await;
    assert!(huge
        .content
        .ends_with("\n\n... [body truncated at 2000000 bytes]"));
}

#[tokio::test]
async fn charsets_are_honoured() {
    let server = MockServer::start(vec![
        raw(
            200,
            &[("Content-Type", "text/plain; charset=iso-8859-1")],
            b"caf\xe9",
        ),
        raw(
            200,
            &[("Content-Type", "text/plain; charset=x-nope")],
            b"abc",
        ),
    ])
    .await;
    let workspace = Workspace::new("charset");

    let latin = fetch(workspace.local(), format!("{}/latin", server.base_url)).await;
    assert!(latin.content.ends_with("\n\ncaf\u{e9}"), "{latin:?}");

    let unknown = fetch(workspace.local(), format!("{}/nope", server.base_url)).await;
    assert_eq!(
        unknown.error.as_deref(),
        Some("LookupError: unknown encoding: x-nope")
    );
}

#[tokio::test]
async fn a_disabled_network_is_a_policy_error() {
    let workspace = Workspace::new("offline");
    let mut ctx = workspace.local();
    ctx.policy.network = false;

    let result = fetch(ctx, "https://example.com/".into()).await;

    assert_eq!(
        result.error.as_deref(),
        Some("PolicyError: Network access is disabled by the sandbox policy.")
    );
}
