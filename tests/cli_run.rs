//! End-to-end tests of `agent86 run` against the local mock provider.

mod support;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::{fixture, MockServer, Reply};

const TEST_KEY: &str = "sk-test-0123456789abcdefghij";

/// An isolated home and project directory for one test.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("project").join(".agent86")).unwrap();
        Self { root }
    }

    /// Point a `[providers.mock]` block at `server`, with `mock:test/model` as the default.
    fn with_mock_provider(name: &str, server: &MockServer) -> Self {
        let workspace = Self::new(name);
        workspace.write_project_config(&format!(
            "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\napi_key_env = \"AGENT86_TEST_MOCK_KEY\"\nmax_retries = 0\n",
            server.base_url
        ));
        workspace
    }

    fn write_project_config(&self, text: &str) {
        std::fs::write(self.project().join(".agent86").join("config.toml"), text).unwrap();
    }

    fn append_project_config(&self, text: &str) {
        let path = self.project().join(".agent86").join("config.toml");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        std::fs::write(path, existing + text).unwrap();
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent86"));
        command
            .args(args)
            .current_dir(self.project())
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("AGENT86_TEST_MOCK_KEY", TEST_KEY)
            .env_remove("AGENT86_MODEL");
        command
    }

    async fn run(&self, args: &[&str]) -> Output {
        let mut command = self.command(args);
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn stdout(output: &Output) -> String {
    text(&output.stdout)
}

fn stderr(output: &Output) -> String {
    text(&output.stderr)
}

#[tokio::test]
async fn run_streams_the_answer_to_stdout_and_the_summary_to_stderr() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let workspace = Workspace::with_mock_provider("text", &server);

    let output = workspace.run(&["run", "Say hello"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hello, world\n");
    let summary = stderr(&output);
    assert!(
        summary.starts_with(
            "— 1 step · 0 tools · 12 in / 3 out (4 cached) · cost n/a (unpriced model) · "
        ),
        "{summary}"
    );
    assert!(summary.trim_end().ends_with('s'), "{summary}");

    let sent = &server.requests()[0];
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {TEST_KEY}").as_str())
    );
    assert_eq!(sent.body["model"], "test/model");
    assert_eq!(sent.body["messages"][0]["content"], "Say hello");
    assert_eq!(sent.body["max_completion_tokens"], 8192);
    assert_eq!(sent.body["temperature"], 0.0);
}

#[tokio::test]
async fn run_json_emits_exactly_one_object_with_the_contract_fields() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let workspace = Workspace::with_mock_provider("json", &server);

    let output = workspace.run(&["run", "--json", "Say hello"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let raw = stdout(&output);
    let document: Value = serde_json::from_str(&raw).unwrap();

    let keys: Vec<&String> = document.as_object().unwrap().keys().collect();
    let mut expected = ["session_id", "output", "steps", "usage", "turn"];
    expected.sort_unstable();
    assert_eq!(keys, expected, "serde_json sorts keys; this checks the set");
    let positions: Vec<usize> = [
        "\"session_id\"",
        "\"output\"",
        "\"steps\"",
        "\"usage\"",
        "\"turn\"",
    ]
    .iter()
    .map(|key| raw.find(key).unwrap())
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "key order: {raw}"
    );

    let session_id = document["session_id"].as_str().unwrap();
    assert_eq!(session_id.len(), 12);
    assert_eq!(document["output"], "Hello, world");
    assert_eq!(document["steps"], 1);
    assert_eq!(
        document["usage"],
        serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 3,
            "cost_usd": 0.0,
            "cache_read_tokens": 4,
            "cache_creation_tokens": 0,
        })
    );
    let turn = &document["turn"];
    assert_eq!(turn["steps"], 1);
    assert_eq!(turn["tool_calls"], 0);
    assert_eq!(turn["input_tokens"], 12);
    assert_eq!(turn["cache_read_tokens"], 4);
    assert!(turn["duration_s"].is_f64());
    assert_eq!(turn["compactions"], 0);
    assert_eq!(turn["continuations"], 0);
}

#[tokio::test]
async fn provider_failures_exit_1_with_a_redacted_error_and_no_stdout() {
    let body = format!("{{\"error\":{{\"message\":\"Invalid key {TEST_KEY}\"}}}}");
    let server = MockServer::start(vec![Reply::Status {
        status: 401,
        headers: Vec::new(),
        body,
    }])
    .await;
    let workspace = Workspace::with_mock_provider("http-error", &server);

    let output = workspace.run(&["run", "--json", "hi"]).await;

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    let errors = stderr(&output);
    assert!(errors.contains("error: HTTP 401: "), "{errors}");
    assert!(!errors.contains(TEST_KEY), "{errors}");
}

#[tokio::test]
async fn a_missing_key_names_the_environment_variable() {
    let server = MockServer::start(Vec::new()).await;
    let workspace = Workspace::with_mock_provider("missing-key", &server);

    let mut command = workspace.command(&["run", "hi"]);
    command.env_remove("AGENT86_TEST_MOCK_KEY");
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with(
            "error: No API key found. Set the AGENT86_TEST_MOCK_KEY environment variable.\n"
        ),
        "{}",
        stderr(&output)
    );
}

#[test]
fn malformed_model_refs_use_the_python_message() {
    let workspace = Workspace::new("bad-ref");
    let output = workspace
        .command(&["--model", "gpt-4o", "run", "hi"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with(
            "error: Model reference 'gpt-4o' must be 'provider:model' (e.g. 'anthropic:claude-opus-4-8').\n"
        ),
        "{}",
        stderr(&output)
    );
}

#[test]
fn unported_and_unknown_providers_are_explained() {
    let workspace = Workspace::new("providers");

    // The built-in default model is Anthropic, which has no Rust adapter yet.
    let default = workspace.command(&["run", "hi"]).output().unwrap();
    assert_eq!(default.status.code(), Some(1));
    assert!(
        stderr(&default)
            .contains("The 'anthropic' provider is not available in the Rust port yet."),
        "{}",
        stderr(&default)
    );

    let unknown = workspace
        .command(&["-m", "nowhere:model", "run", "hi"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("Unknown provider 'nowhere'."),
        "{}",
        stderr(&unknown)
    );
}

#[test]
fn the_model_resolves_flag_over_environment_over_config() {
    let workspace = Workspace::new("precedence");
    workspace.write_project_config("[model]\ndefault = \"fromconfig:m\"\n");

    let config_only = workspace.command(&["run", "hi"]).output().unwrap();
    assert!(
        stderr(&config_only).contains("'fromconfig'"),
        "{}",
        stderr(&config_only)
    );

    let mut env = workspace.command(&["run", "hi"]);
    env.env("AGENT86_MODEL", "fromenv:m");
    assert!(stderr(&env.output().unwrap()).contains("'fromenv'"));

    let mut flag = workspace.command(&["--model", "fromflag:m", "run", "hi"]);
    flag.env("AGENT86_MODEL", "fromenv:m");
    assert!(stderr(&flag.output().unwrap()).contains("'fromflag'"));
}

#[test]
fn malformed_config_exits_1_and_names_the_file() {
    let workspace = Workspace::new("bad-config");
    workspace.write_project_config("[model\n");

    let output = workspace.command(&["run", "hi"]).output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    let errors = stderr(&output);
    assert!(
        errors.starts_with("error: Malformed config at "),
        "{errors}"
    );
    assert!(errors.contains(
        &Path::new(".agent86")
            .join("config.toml")
            .display()
            .to_string()
    ));
}

#[test]
fn usage_errors_exit_2() {
    let workspace = Workspace::new("usage");

    for args in [
        &["frobnicate"][..],
        &["run"][..],
        // Like Typer, --model is a top-level option and must come before `run`.
        &["run", "--model", "a:b", "hi"][..],
    ] {
        let output = workspace.command(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
    }
}

#[test]
fn version_prints_the_package_version() {
    let workspace = Workspace::new("version");
    for flag in ["--version", "-V"] {
        let output = workspace.command(&[flag]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            stdout(&output),
            format!("agent86 {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

/// The fixture reports 12 prompt tokens (4 of them cached) and 3 output tokens.
fn fixture_cost(input_per_mtok: f64, output_per_mtok: f64) -> f64 {
    (8.0 / 1_000_000.0) * input_per_mtok
        + (4.0 / 1_000_000.0) * (input_per_mtok * 0.1)
        + (0.0 / 1_000_000.0) * (input_per_mtok * 1.25)
        + (3.0 / 1_000_000.0) * output_per_mtok
}

#[tokio::test]
async fn pricing_overrides_keyed_by_the_full_ref_are_applied() {
    let server = MockServer::start(vec![
        Reply::Sse(fixture("openai/text.sse")),
        Reply::Sse(fixture("openai/text.sse")),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("priced-ref", &server);
    workspace.append_project_config(
        "
[pricing.models.\"mock:test/model\"]
input_per_mtok = 1.0
output_per_mtok = 2.0
",
    );

    let text = workspace.run(&["run", "hi"]).await;
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(stderr(&text).contains(" · $0.0000 · "), "{}", stderr(&text));

    let json = workspace.run(&["run", "--json", "hi"]).await;
    let raw = stdout(&json);
    let document: Value = serde_json::from_str(&raw).unwrap();
    let expected = fixture_cost(1.0, 2.0);
    // Python's json.dumps writes this as 1.44e-05, not serde_json's 0.0000144.
    assert!(raw.contains("\"cost_usd\": 1.44e-05"), "{raw}");
    assert_eq!(document["usage"]["cost_usd"].as_f64(), Some(expected));
    assert_eq!(document["turn"]["cost_usd"].as_f64(), Some(expected));
}

#[tokio::test]
async fn pricing_overrides_keyed_by_the_bare_model_id_are_applied() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let workspace = Workspace::with_mock_provider("priced-bare", &server);
    workspace.append_project_config(
        "
[pricing.models.\"test/model\"]
input_per_mtok = 1000.0
output_per_mtok = 2000.0
",
    );

    let output = workspace.run(&["run", "hi"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    let expected = format!(" · ${:.4} · ", fixture_cost(1000.0, 2000.0));
    assert!(stderr(&output).contains(&expected), "{}", stderr(&output));
}

#[test]
fn negative_prices_are_rejected_as_invalid_config() {
    let workspace = Workspace::new("negative-price");
    workspace.write_project_config(
        "[pricing.models.m]
input_per_mtok = -1.0
output_per_mtok = 0.0
",
    );

    let output = workspace
        .command(&["-m", "openai:gpt-4o", "run", "hi"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("error: Invalid config: "),
        "{}",
        stderr(&output)
    );
}
