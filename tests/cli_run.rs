//! End-to-end tests of `agent86 run` against the local mock provider.

mod support;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::{fixture, sse_text, sse_tool_calls, MockServer, Reply};

const TEST_KEY: &str = "sk-test-0123456789abcdefghij";

/// An isolated home and project directory for one test.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home").join(".agent86")).unwrap();
        std::fs::create_dir_all(root.join("project").join(".agent86")).unwrap();
        // The default sentence-transformers model is not cached in the
        // throwaway home, so it would be downloaded (or, offline, its fallback
        // note would precede every run's output); the hash embedder keeps the
        // runs self-contained.
        std::fs::write(
            root.join("home").join(".agent86").join("config.toml"),
            "[memory]\nembeddings = \"hash:256\"\n",
        )
        .unwrap();
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
            .env_remove("AGENT86_MODEL")
            .env_remove("AGENT86_SANDBOX")
            .env_remove("AGENT86_APPROVAL")
            // No test may reach the Hugging Face hub or read the real cache.
            .env_remove("HF_HUB_CACHE")
            .env_remove("HF_HOME")
            .env_remove("HF_ENDPOINT")
            .env("HF_HUB_OFFLINE", "1");
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
async fn run_works_against_ollama_and_local_models_cost_nothing() {
    let server = MockServer::start(vec![Reply::Sse(fixture("ollama/text.ndjson"))]).await;
    let workspace = Workspace::new("ollama");
    workspace.write_project_config(&format!(
        "[model]\ndefault = \"ollama:llama3.1:8b\"\n\n[providers.ollama]\nbase_url = \"{}\"\n",
        server.base_url
    ));

    let output = workspace.run(&["run", "Say hello"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hello, world\n");
    let summary = stderr(&output);
    assert!(
        summary.starts_with("— 1 step · 0 tools · 26 in / 3 out · $0.0000 · "),
        "{summary}"
    );
    let sent = &server.requests()[0];
    assert_eq!(sent.request_line, "POST /api/chat HTTP/1.1");
    assert_eq!(sent.body["model"], "llama3.1:8b");
    // The built-in num_ctx, and the harness's output cap as num_predict, as in Python.
    assert_eq!(
        sent.body["options"],
        serde_json::json!({"temperature": 0.0, "num_ctx": 8192, "num_predict": 8192})
    );
}

#[tokio::test]
async fn run_works_against_anthropic_and_prices_cached_tokens() {
    let server = MockServer::start(vec![Reply::Sse(fixture("anthropic/text.sse"))]).await;
    let workspace = Workspace::new("anthropic");
    // The key comes from the environment, so the keyring is never consulted.
    workspace.write_project_config(&format!(
        "[model]\ndefault = \"anthropic:claude-sonnet-4-6\"\n\n[providers.anthropic]\n\
         base_url = \"{}\"\napi_key_env = \"AGENT86_TEST_MOCK_KEY\"\n",
        server.base_url
    ));

    let output = workspace.run(&["run", "Say hello"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hello, world\n");
    let summary = stderr(&output);
    assert!(
        summary.starts_with("— 1 step · 0 tools · 120 in / 3 out (100 cached) · $0.0001 · "),
        "{summary}"
    );
    let sent = &server.requests()[0];
    assert_eq!(sent.request_line, "POST /v1/messages HTTP/1.1");
    // The harness's output cap, and Python's system prompt split out of the messages,
    // long enough to be marked for the prompt cache.
    assert_eq!(sent.body["max_tokens"], 8192);
    assert_eq!(
        sent.body["system"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert_eq!(sent.body["messages"][0]["role"], "user");
}

#[tokio::test]
async fn run_works_against_llamacpp_without_a_key_at_no_cost() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let workspace = Workspace::new("llamacpp");
    // A base_url without /v1, as the built-in section has it.
    workspace.write_project_config(&format!(
        "[model]\ndefault = \"llamacpp:qwen3\"\n\n[providers.llamacpp]\nbase_url = \"{}\"\n",
        server.base_url
    ));

    let output = workspace.run(&["run", "Say hello"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hello, world\n");
    let summary = stderr(&output);
    assert!(
        summary.starts_with("— 1 step · 0 tools · 12 in / 3 out (4 cached) · $0.0000 · "),
        "{summary}"
    );
    let sent = &server.requests()[0];
    assert_eq!(sent.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(sent.body["model"], "qwen3");
    assert!(
        !sent
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
        "no key is sent to a local server"
    );
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
    let messages = sent.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    let expected_system = format!(
        "{}

Environment: agent86 v{}. Default model: mock:test/model. Sandbox: subprocess.",
        fixture("prompt/base_identity.txt").trim(),
        env!("CARGO_PKG_VERSION"),
    );
    assert_eq!(messages[0]["content"], expected_system.as_str());
    assert_eq!(
        messages[1],
        serde_json::json!({"role": "user", "content": "Say hello"})
    );
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

    // serde_json's preserve_order keeps document order, so this checks Python's order.
    let keys: Vec<&String> = document.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["session_id", "output", "steps", "usage", "turn"]);
    let turn_keys: Vec<&String> = document["turn"].as_object().unwrap().keys().collect();
    assert_eq!(
        turn_keys,
        [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "cost_usd",
            "steps",
            "tool_calls",
            "duration_s",
            "compactions",
            "continuations",
        ]
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
            "error: No API key found. Set the AGENT86_TEST_MOCK_KEY environment variable or store \
             a key in the OS keyring (service \"agent86\", account \"mock\").\n"
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
fn unknown_providers_are_explained() {
    let workspace = Workspace::new("providers");

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

#[tokio::test]
async fn the_system_prompt_reports_overridden_model_and_sandbox() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let workspace = Workspace::with_mock_provider("prompt-env", &server);

    let mut command = workspace.command(&["--model", "mock:other-model", "run", "hi"]);
    command.env("AGENT86_SANDBOX", "docker");
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    let system = server.requests()[0].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        system.ends_with("Default model: mock:other-model. Sandbox: docker."),
        "{system}"
    );
}

/// Run `agent86` with `input` on stdin.
fn run_with_stdin(workspace: &Workspace, args: &[&str], input: &str) -> Output {
    use std::io::Write as _;
    let mut child = workspace
        .command(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

// These `keys` tests only exercise refusals, which all happen before the OS
// keyring is touched; a successful `keys set` would write the real keyring.

#[test]
fn keys_set_refuses_unknown_and_keyless_providers() {
    let workspace = Workspace::new("keys-providers");

    let unknown = workspace
        .command(&["keys", "set", "openroutr"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown)
            .starts_with("error: Unknown provider 'openroutr'. Providers that take a key: "),
        "{}",
        stderr(&unknown)
    );

    let keyless = workspace
        .command(&["keys", "delete", "ollama"])
        .output()
        .unwrap();
    assert_eq!(keyless.status.code(), Some(1));
    assert!(
        stderr(&keyless).contains("has no api_key_env"),
        "{}",
        stderr(&keyless)
    );
}

#[test]
fn keys_set_will_not_prompt_without_a_terminal() {
    let workspace = Workspace::new("keys-no-tty");

    let output = workspace
        .command(&["keys", "set", "openrouter"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Pass --stdin"),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "");
}

#[test]
fn keys_set_rejects_empty_and_repeated_pastes() {
    let workspace = Workspace::new("keys-bad-paste");

    let empty = run_with_stdin(&workspace, &["keys", "set", "openrouter", "--stdin"], "\n");
    assert_eq!(empty.status.code(), Some(1));
    assert!(
        stderr(&empty).contains("No key was entered."),
        "{}",
        stderr(&empty)
    );

    let key = "sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab";
    let repeated = run_with_stdin(
        &workspace,
        &["keys", "set", "openrouter", "--stdin"],
        &key.repeat(4),
    );
    assert_eq!(repeated.status.code(), Some(1));
    let errors = stderr(&repeated);
    assert!(errors.contains("pasted more than once"), "{errors}");
    assert!(
        !errors.contains(key),
        "the key must never be echoed: {errors}"
    );
}

#[tokio::test]
async fn run_uses_workspace_tools_and_reports_them_inline() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("call_1", "list_dir", "{}")]),
        sse_text("There is a config folder.", "stop"),
        sse_tool_calls(&[("call_1", "list_dir", "{}")]),
        sse_text("There is a config folder.", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("tools", &server);

    let text = workspace.run(&["run", "What is here?"]).await;
    assert!(text.status.success(), "{}", stderr(&text));
    assert_eq!(
        stdout(&text),
        "\n[tool] list_dir({})\n[tool] list_dir -> .agent86/\nThere is a config folder.\n"
    );
    assert!(
        stderr(&text).starts_with("— 2 steps · 1 tool · "),
        "{}",
        stderr(&text)
    );

    let json = workspace.run(&["run", "--json", "What is here?"]).await;
    let document: Value = serde_json::from_str(&stdout(&json)).unwrap();
    // As in Python, the JSON output carries the tool notices too.
    assert_eq!(
        document["output"],
        "\n[tool] list_dir({})\n[tool] list_dir -> .agent86/\nThere is a config folder."
    );
    assert_eq!(document["steps"], 2);
    assert_eq!(document["turn"]["tool_calls"], 1);
}

#[tokio::test]
async fn a_tripped_circuit_exits_1_without_the_model_hint() {
    let server = MockServer::start(vec![sse_tool_calls(&[("call_1", "list_dir", "{}")])]).await;
    let workspace = Workspace::with_mock_provider("circuit", &server);
    workspace.append_project_config("\n[limits]\nmax_steps = 1\n");

    let output = workspace.run(&["run", "loop"]).await;

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "\nerror: Circuit tripped: step budget reached (1 steps)\n"
    );
}

fn write_request() -> Vec<Reply> {
    vec![
        sse_tool_calls(&[(
            "w",
            "write_file",
            r#"{"path": "made.txt", "content": "made\n"}"#,
        )]),
        sse_text("ok", "stop"),
    ]
}

#[tokio::test]
async fn piped_runs_decline_side_effects_unless_told_otherwise() {
    let server = MockServer::start(write_request()).await;
    let workspace = Workspace::with_mock_provider("approval-piped", &server);

    // stdin is not a terminal here, so `ask` has no one to ask.
    let output = workspace.run(&["run", "make a file"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains(
            "[tool] write_file -> error: Not executed: approval required but no prompt available."
        ),
        "{}",
        stdout(&output)
    );
    assert!(!workspace.project().join("made.txt").exists());
}

#[tokio::test]
async fn yes_and_auto_approve_side_effects() {
    for (name, args, env) in [
        ("approval-yes", vec!["run", "--yes", "make a file"], None),
        ("approval-y", vec!["run", "-y", "make a file"], None),
        (
            "approval-flag",
            vec!["--approval", "auto", "run", "make a file"],
            None,
        ),
        ("approval-env", vec!["run", "make a file"], Some("auto")),
    ] {
        let server = MockServer::start(write_request()).await;
        let workspace = Workspace::with_mock_provider(name, &server);
        let mut command = workspace.command(&args);
        if let Some(mode) = env {
            command.env("AGENT86_APPROVAL", mode);
        }
        let output = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();

        assert!(output.status.success(), "{name}: {}", stderr(&output));
        assert!(
            stdout(&output).contains("[tool] write_file -> Wrote 5 bytes to made.txt (new file)."),
            "{name}: {}",
            stdout(&output)
        );
        assert!(workspace.project().join("made.txt").exists(), "{name}");
    }
}

#[tokio::test]
async fn approval_deny_blocks_side_effects() {
    let server = MockServer::start(write_request()).await;
    let workspace = Workspace::with_mock_provider("approval-deny", &server);

    let output = workspace
        .run(&["--approval", "deny", "run", "make a file"])
        .await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Not executed: blocked by approval policy (deny)."));
    assert!(!workspace.project().join("made.txt").exists());
}

#[test]
fn unknown_approval_modes_are_invalid_config() {
    let workspace = Workspace::new("approval-bad");

    let output = workspace
        .command(&["--approval", "sometimes", "run", "hi"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("error: Invalid config: "),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn redact_mode_from_config_covers_text_and_json_output() {
    let answer = || sse_text("Contact jane@example.com today.", "stop");
    let server = MockServer::start(vec![answer(), answer()]).await;
    let workspace = Workspace::with_mock_provider("redact", &server);
    workspace.append_project_config("\n[guardrails]\negress = \"redact\"\n");

    let text = workspace.run(&["run", "who?"]).await;
    assert!(text.status.success(), "{}", stderr(&text));
    assert_eq!(
        stdout(&text),
        "Contact [REDACTED:email] today.\n[guardrail] output flagged: pii:email\n\n"
    );

    let json = workspace.run(&["run", "--json", "who?"]).await;
    let document: Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(
        document["output"],
        "Contact [REDACTED:email] today.\n[guardrail] output flagged: pii:email\n"
    );
    assert!(!stdout(&json).contains("jane@example.com"));
}

#[tokio::test]
async fn shell_commands_need_approval_like_any_side_effect() {
    let command_then_answer = || {
        vec![
            sse_tool_calls(&[(
                "c",
                "run_command",
                r#"{"command": "echo from-the-sandbox"}"#,
            )]),
            sse_text("ran it", "stop"),
        ]
    };

    let server = MockServer::start(command_then_answer()).await;
    let workspace = Workspace::with_mock_provider("shell-piped", &server);
    let piped = workspace.run(&["run", "run it"]).await;
    assert!(
        stdout(&piped).contains(
            "[tool] run_command -> error: Not executed: approval required but no prompt available."
        ),
        "{}",
        stdout(&piped)
    );

    let server = MockServer::start(command_then_answer()).await;
    let workspace = Workspace::with_mock_provider("shell-yes", &server);
    let approved = workspace.run(&["run", "--yes", "run it"]).await;
    assert!(approved.status.success(), "{}", stderr(&approved));
    assert!(
        stdout(&approved).contains("[tool] run_command -> exit code: 0\n"),
        "{}",
        stdout(&approved)
    );
    let observed = server.requests()[1].body["messages"][3]["content"].clone();
    assert_eq!(observed, "exit code: 0\nstdout:\nfrom-the-sandbox");
}

impl Workspace {
    fn memory_db(&self) -> PathBuf {
        self.root.join("home").join(".agent86").join("memory.db")
    }

    /// `(title, state_json)` for a stored session.
    fn stored_session(&self, session_id: &str) -> Option<(Option<String>, Value)> {
        let connection = rusqlite::Connection::open(self.memory_db()).ok()?;
        connection
            .query_row(
                "SELECT title, state_json FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| {
                    let json: String = row.get(1)?;
                    Ok((row.get(0)?, serde_json::from_str(&json).unwrap()))
                },
            )
            .ok()
    }
}

#[tokio::test]
async fn sessions_are_saved_and_resumed_with_their_history() {
    let server = MockServer::start(vec![
        sse_text("Nice to meet you, Ada.", "stop"),
        sse_text("Your name is Ada.", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("session-resume", &server);

    let first = workspace.run(&["run", "--json", "My name is Ada."]).await;
    assert!(first.status.success(), "{}", stderr(&first));
    let first: Value = serde_json::from_str(&stdout(&first)).unwrap();
    let session_id = first["session_id"].as_str().unwrap().to_owned();

    let (title, state) = workspace.stored_session(&session_id).unwrap();
    assert_eq!(title.as_deref(), Some("My name is Ada."));
    assert_eq!(state["phase"], "done");
    assert_eq!(state["messages"].as_array().unwrap().len(), 2);

    let second = workspace
        .run(&[
            "run",
            "--json",
            "--session",
            &session_id,
            "What is my name?",
        ])
        .await;
    assert!(second.status.success(), "{}", stderr(&second));
    let second: Value = serde_json::from_str(&stdout(&second)).unwrap();

    // The resumed request carries the earlier exchange.
    let messages = server.requests()[1].body["messages"].clone();
    let roles: Vec<&str> = messages
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(messages[2]["content"], "Nice to meet you, Ada.");

    // Steps and usage accumulate across the session; `turn` is this turn only.
    assert_eq!(second["session_id"], session_id.as_str());
    assert_eq!(second["steps"], 2);
    assert_eq!(second["usage"]["input_tokens"], 20);
    assert_eq!(second["turn"]["steps"], 1);
    assert_eq!(second["turn"]["input_tokens"], 10);

    let (title, state) = workspace.stored_session(&session_id).unwrap();
    assert_eq!(title.as_deref(), Some("My name is Ada."));
    assert_eq!(state["messages"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn unknown_session_ids_start_a_new_session() {
    let server = MockServer::start(vec![sse_text("hello", "stop")]).await;
    let workspace = Workspace::with_mock_provider("session-unknown", &server);

    let output = workspace
        .run(&["run", "--json", "-s", "doesnotexist", "hi"])
        .await;

    assert!(output.status.success(), "{}", stderr(&output));
    let document: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let session_id = document["session_id"].as_str().unwrap();
    assert_ne!(session_id, "doesnotexist");
    assert!(workspace.stored_session(session_id).is_some());
    assert!(workspace.stored_session("doesnotexist").is_none());
}

#[tokio::test]
async fn failed_turns_are_saved_in_the_error_phase() {
    let server = MockServer::start(vec![Reply::Status {
        status: 500,
        headers: Vec::new(),
        body: "boom".to_owned(),
    }])
    .await;
    let workspace = Workspace::with_mock_provider("session-error", &server);

    let output = workspace.run(&["run", "this will fail"]).await;

    assert_eq!(output.status.code(), Some(1));
    let connection = rusqlite::Connection::open(workspace.memory_db()).unwrap();
    let (title, json): (Option<String>, String) = connection
        .query_row("SELECT title, state_json FROM sessions", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    let state: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(title.as_deref(), Some("this will fail"));
    assert_eq!(state["phase"], "error");
    assert_eq!(state["messages"][0]["content"], "this will fail");
    assert!(state["last_turn"].is_object());
}

#[tokio::test]
async fn memory_can_be_turned_off() {
    let server = MockServer::start(vec![sse_text("ok", "stop")]).await;
    let workspace = Workspace::with_mock_provider("session-off", &server);
    workspace.append_project_config("\n[memory]\nenabled = false\n");

    let output = workspace.run(&["run", "hi"]).await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!workspace.memory_db().exists());
    // Without memory, the agent gets no `remember` or `recall`.
    let tools = server.requests()[0].body["tools"].to_string();
    assert!(!tools.contains("\"remember\""), "{tools}");

    let stats = workspace.run(&["memory", "stats"]).await;
    assert_eq!(stats.status.code(), Some(1));
    assert_eq!(
        stderr(&stats),
        "Memory is disabled in config. Set `[memory] enabled = true` to turn it on \
         (`agent86 config path` shows which file to edit).\n"
    );
}

/// The system message of the `index`th request.
fn system_of(server: &MockServer, index: usize) -> String {
    server.requests()[index].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn finished_turns_are_recalled_into_later_similar_tasks() {
    let server = MockServer::start(vec![
        sse_text("README.md and notes.txt", "stop"),
        sse_text("Still README.md and notes.txt", "stop"),
        sse_text("Paris", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("recall", &server);

    for goal in [
        "list the files in this directory",
        "list the files in this directory please",
        "what is the capital of France",
    ] {
        let output = workspace.run(&["run", goal]).await;
        assert!(output.status.success(), "{}", stderr(&output));
    }

    // Nothing to recall on the first turn; the second is told what happened.
    let note = "Relevant past experience (from memory; may help or warn):\n\
                - task: 'list the files in this directory' -> README.md and notes.txt";
    assert!(!system_of(&server, 0).contains("Relevant past experience"));
    assert!(
        system_of(&server, 1).ends_with(&format!("\n\n{note}")),
        "{}",
        system_of(&server, 1)
    );
    // An unrelated task recalls nothing.
    assert!(!system_of(&server, 2).contains("Relevant past experience"));

    // With memory on, the agent can remember and recall facts.
    let names: Vec<String> = server.requests()[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    // Then `delegate`, which `[agents]` turns on by default, as in Python.
    assert_eq!(names[names.len() - 3..], ["remember", "recall", "delegate"]);

    let search = workspace
        .run(&["memory", "search", "list the files", "-k", "1"])
        .await;
    assert!(search.status.success(), "{}", stderr(&search));
    let lines: Vec<String> = stdout(&search).lines().map(str::to_owned).collect();
    assert_eq!(lines[0], "episodes");
    assert!(
        lines[1].starts_with("  [0.") && lines[1].contains("] list the files in this directory"),
        "{lines:?}"
    );
    assert_eq!(lines[2..], ["facts", "  (none)"]);
}

#[tokio::test]
async fn memory_commands_report_and_prune_the_store() {
    let server = MockServer::start(vec![sse_text("hello", "stop")]).await;
    let workspace = Workspace::with_mock_provider("memory-commands", &server);
    let run = workspace.run(&["run", "--json", "say hi"]).await;
    let session = serde_json::from_str::<Value>(&stdout(&run)).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let memory = |args: &[&str]| workspace.command(args).output().unwrap();

    let stats = memory(&["memory", "stats"]);
    assert!(stats.status.success(), "{}", stderr(&stats));
    assert_eq!(
        stdout(&stats),
        format!(
            "Kind      Count\nsessions  1\nepisodes  1\nmemories  0\ndb: {}  embedder: hash:256\n",
            workspace.memory_db().display()
        )
    );

    let sessions = memory(&["memory", "sessions"]);
    assert_eq!(
        stdout(&sessions),
        format!("Session       Title\n{session}  say hi\n")
    );

    let counts = "{'sessions': 1, 'episodes': 1, 'memories': 0}\n";
    let bare = memory(&["memory", "prune"]);
    assert!(bare.status.success());
    assert_eq!(
        stdout(&bare),
        format!("No retention limit given. Pass --older-than and/or --keep-last.\n{counts}")
    );

    let dry = memory(&[
        "memory",
        "prune",
        "--keep-last",
        "0",
        "--memories",
        "--dry-run",
    ]);
    assert_eq!(
        stdout(&dry),
        format!(
            "dry run — would prune episodes+sessions + memories (older_than=None, keep_last=0). \
             Current counts:\n{counts}"
        )
    );

    // Piped with no answer, the prompt aborts as Click's does; "n" declines.
    let question = "Prune episodes, sessions (older_than=30.0, keep_last=0)? [y/N]: ";
    let ended = run_with_stdin(
        &workspace,
        &["memory", "prune", "--older-than", "30", "--keep-last", "0"],
        "",
    );
    assert_eq!(ended.status.code(), Some(1));
    assert_eq!(stdout(&ended), format!("{question}\n"));
    assert_eq!(stderr(&ended), "Aborted!\n");
    let declined = run_with_stdin(
        &workspace,
        &["memory", "prune", "--older-than", "30", "--keep-last", "0"],
        "maybe\nn\n",
    );
    assert!(declined.status.success());
    assert_eq!(stdout(&declined), format!("{question}{question}aborted\n"));
    assert_eq!(stderr(&declined), "Error: invalid input\n");

    let pruned = memory(&[
        "memory",
        "prune",
        "--older-than",
        "30",
        "--keep-last",
        "0",
        "-y",
    ]);
    assert!(pruned.status.success(), "{}", stderr(&pruned));
    assert_eq!(
        stdout(&pruned),
        "Pruned 2 row(s): {'episodes': 1, 'sessions': 1}\n\
         {'sessions': 0, 'episodes': 0, 'memories': 0}\n"
    );

    let forget = memory(&["memory", "forget", "7"]);
    assert!(forget.status.success());
    assert_eq!(stdout(&forget), "No memory with id 7.\n");

    let empty = memory(&["memory", "sessions"]);
    assert_eq!(stdout(&empty), "no sessions yet\n");

    // A model that is not cached, with the hub offline: the fallback is said,
    // as Python says it.
    workspace.append_project_config(
        "\n[memory]\nembeddings = \"sentence-transformers:all-MiniLM-L6-v2\"\n",
    );
    let fallback = memory(&["memory", "stats"]);
    assert!(fallback.status.success());
    assert_eq!(
        stderr(&fallback),
        "memory: sentence-transformers unavailable (OSError); using the offline hash embedder.\n"
    );
    assert!(stdout(&fallback).contains("embedder: hash:256"));
}

#[test]
fn a_sentence_transformers_model_embeds_the_memory() {
    let workspace = Workspace::new("st-model");
    let model = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("memory")
        .join("tiny-st");
    let spec = format!("sentence-transformers:{}", model.display());
    // A literal string, so a Windows path needs no escaping.
    workspace.write_project_config(&format!("[memory]\nembeddings = '{spec}'\n"));
    let memory = |args: &[&str]| workspace.command(args).output().unwrap();

    let stats = memory(&["memory", "stats"]);
    assert!(stats.status.success(), "{}", stderr(&stats));
    assert_eq!(stderr(&stats), "");
    assert!(
        stdout(&stats).ends_with(&format!("  embedder: {spec}\n")),
        "{}",
        stdout(&stats)
    );

    // Facts stored through the model are found by it, at its dimension.
    let db = workspace
        .root
        .join("home")
        .join(".agent86")
        .join("memory.db");
    let store = agent86::session::SessionStore::open_with(
        &db,
        agent86::memory::embeddings::build_embedder(&spec, true).0,
    )
    .unwrap();
    store
        .add_memory(
            "the deploy key lives in the vault",
            "fact",
            &Default::default(),
        )
        .unwrap();
    drop(store);
    let search = memory(&["memory", "search", "the deploy key lives in the vault"]);
    assert!(search.status.success(), "{}", stderr(&search));
    assert!(
        stdout(&search).contains("the deploy key lives in the vault"),
        "{}",
        stdout(&search)
    );
}

#[tokio::test]
async fn retention_caps_prune_the_log_at_startup_and_are_traced() {
    let server = MockServer::start(vec![
        sse_text("one", "stop"),
        sse_text("two", "stop"),
        sse_text("three", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("retention", &server);
    workspace.append_project_config(
        "\n[memory]\nretention_max_episodes = 1\nretention_max_sessions = 2\n",
    );

    for goal in ["first", "second", "third"] {
        let output = workspace.run(&["run", goal]).await;
        assert!(output.status.success(), "{}", stderr(&output));
    }

    // The third run started with two of each: one episode over its cap.
    let trace = std::fs::read_to_string(
        workspace
            .root
            .join("home")
            .join(".agent86")
            .join("traces")
            .join("trace.jsonl"),
    )
    .unwrap();
    let prunes: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["kind"] == "retention_prune")
        .collect();
    assert_eq!(prunes.len(), 1, "{prunes:?}");
    assert_eq!(prunes[0]["session"], "memory");
    assert_eq!(prunes[0]["episodes"], 1);
    assert!(prunes[0].get("sessions").is_none());

    let stats = workspace.command(&["memory", "stats"]).output().unwrap();
    assert!(
        stdout(&stats).contains("sessions  3\nepisodes  2\n"),
        "{}",
        stdout(&stats)
    );
}

#[tokio::test]
async fn runs_are_traced_and_the_trace_commands_read_them() {
    let server = MockServer::start(vec![sse_text("Hello there.", "stop")]).await;
    let workspace = Workspace::with_mock_provider("trace-commands", &server);
    let key = "sk-abcdefghijklmnopqrstuvwxyz012345";

    let run = workspace
        .run(&["run", "--json", &format!("say hi, my key is {key}")])
        .await;
    assert!(run.status.success(), "{}", stderr(&run));
    let session = serde_json::from_str::<Value>(&stdout(&run)).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let trace_file = workspace
        .root
        .join("home")
        .join(".agent86")
        .join("traces")
        .join("trace.jsonl");

    let path = workspace.command(&["trace", "path"]).output().unwrap();
    assert_eq!(
        stdout(&path),
        format!("{} (exists)\n", trace_file.display())
    );

    let raw = std::fs::read_to_string(&trace_file).unwrap();
    assert!(!raw.contains(key), "the key must be redacted: {raw}");
    assert!(raw.contains("***REDACTED***"));

    let export = workspace
        .command(&["trace", "export", "-s", &session])
        .output()
        .unwrap();
    let kinds: Vec<String> = stdout(&export)
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["turn_start", "model_call", "turn_end"]);

    let json = workspace
        .command(&["trace", "export", "-f", "json"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stdout(&json))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let out_file = workspace.root.join("spans.json");
    let otlp = workspace
        .command(&[
            "trace",
            "export",
            "-f",
            "otlp-json",
            "-o",
            &out_file.display().to_string(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        stdout(&otlp),
        format!("Wrote 3 event(s) to {} (otlp-json).\n", out_file.display())
    );
    let document: Value =
        serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    let spans = document["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap();
    let names: Vec<&str> = spans
        .iter()
        .map(|span| span["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["model_call", "turn"]);

    let show = workspace
        .command(&["trace", "show", "-k", "model_call"])
        .output()
        .unwrap();
    let table = stdout(&show);
    let lines: Vec<&str> = table.lines().collect();
    assert!(lines[0].starts_with("time"), "{table}");
    assert!(lines[1].contains("model_call"), "{table}");
    assert!(
        lines[1].contains("  10  ") || lines[1].contains(" 10 "),
        "{table}"
    );
    assert!(
        lines[2].starts_with("10 in / 5 out tokens, $0.0000 across 1 event(s)"),
        "{table}"
    );
}

#[test]
fn trace_commands_handle_empty_traces_and_bad_options() {
    let workspace = Workspace::new("trace-empty");

    let show = workspace.command(&["trace", "show"]).output().unwrap();
    assert!(show.status.success());
    assert_eq!(stdout(&show), "no trace events\n");

    let since = workspace
        .command(&["trace", "show", "--since", "soon"])
        .output()
        .unwrap();
    assert_eq!(since.status.code(), Some(2));
    assert!(
        stderr(&since).contains("Invalid value for '--since': 'soon' is not a duration"),
        "{}",
        stderr(&since)
    );

    let format = workspace
        .command(&["trace", "export", "-f", "xml"])
        .output()
        .unwrap();
    assert_eq!(format.status.code(), Some(2));
    assert!(stderr(&format).contains("unknown format 'xml' (jsonl | json | otlp-json)"));
}

/// `python3` or `python`, whichever runs.
fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

#[tokio::test]
async fn mcp_commands_without_servers_say_where_to_add_one() {
    let workspace = Workspace::new("mcp-none");
    let list = workspace.run(&["mcp", "list"]).await;
    assert!(list.status.success(), "{}", stderr(&list));
    assert_eq!(
        stdout(&list),
        "No MCP servers configured. Add one with `/config mcp` in the REPL, or an \
         `[mcp_servers.<name>]` block in the file `agent86 config path` names.\n"
    );
    let tools = workspace.run(&["mcp", "tools"]).await;
    assert_eq!(
        stdout(&tools),
        "No MCP servers configured (or MCP disabled). `agent86 mcp list` shows what is \
         configured and whether each is enabled.\n"
    );
}

#[tokio::test]
async fn run_offers_and_calls_mcp_tools() {
    let Some(python) = python() else {
        return;
    };
    let server = MockServer::start(vec![
        sse_tool_calls(&[("call_1", "mcp__test__add", r#"{"a": 2, "b": 3}"#)]),
        sse_text("It is 5.", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("mcp", &server);
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp")
        .join("server.py");
    workspace.append_project_config(&format!(
        "
[mcp.servers.test]
command = '{python}'
args = ['{}']
",
        script.display()
    ));

    let output = workspace.run(&["run", "What is 2 + 3?"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).ends_with("It is 5.\n"),
        "{}",
        stdout(&output)
    );

    let requests = server.requests();
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains(
            "Sandbox: subprocess.

MCP servers (every tool listed is callable now;"
        ),
        "{system}"
    );
    assert!(
        system.contains("- test: mcp__test__echo, mcp__test__add,"),
        "{system}"
    );
    let add = |request: usize| {
        requests[request].body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["function"]["name"] == "mcp__test__add")
            .unwrap()["function"]
            .clone()
    };
    // Deferred in the first request, materialised by the call for the next.
    assert_eq!(add(0)["parameters"]["properties"], serde_json::json!({}));
    assert_eq!(
        add(1)["parameters"]["required"],
        serde_json::json!(["a", "b"])
    );
    assert_eq!(requests[1].body["messages"][3]["content"], "5");
}

/// Three runs in one session, the first with a long goal, under a 260-token
/// conversation cap: the third request has to drop the long goal, and the
/// fourth turn starts over budget with something old enough to compact.
async fn long_session(workspace: &Workspace) -> String {
    let long_goal = format!("Remember this note: {}", "x".repeat(980));
    let first = workspace.run(&["run", "--json", &long_goal]).await;
    assert!(first.status.success(), "{}", stderr(&first));
    let first: Value = serde_json::from_str(&stdout(&first)).unwrap();
    let session = first["session_id"].as_str().unwrap().to_owned();
    for goal in ["second question", "third question"] {
        let output = workspace.run(&["run", "-s", &session, goal]).await;
        assert!(output.status.success(), "{}", stderr(&output));
    }
    session
}

fn trace_events(workspace: &Workspace, kind: &str) -> Vec<Value> {
    let path = workspace
        .root
        .join("home")
        .join(".agent86")
        .join("traces")
        .join("trace.jsonl");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["kind"] == kind)
        .collect()
}

fn request_roles(request: &support::Recorded) -> Vec<String> {
    request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn long_sessions_are_compacted_into_a_summary() {
    let server = MockServer::start(vec![
        sse_text("ok 1", "stop"),
        sse_text("ok 2", "stop"),
        sse_text("ok 3", "stop"),
        sse_text("GOAL: keep the long note.", "stop"),
        sse_text("ok 4", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("compaction", &server);
    workspace.append_project_config("\n[limits]\nmax_context_tokens = 260\n");
    let session = long_session(&workspace).await;

    let fourth = workspace
        .run(&["run", "-s", &session, "fourth question"])
        .await;
    assert!(fourth.status.success(), "{}", stderr(&fourth));
    assert_eq!(
        stdout(&fourth),
        "\n[compacted 1 messages into a summary]\nok 4\n"
    );
    assert!(
        stderr(&fourth).contains("1 compaction"),
        "{}",
        stderr(&fourth)
    );

    let requests = server.requests();
    // The third turn still fit (259 tokens); the fourth is the first over budget.
    assert_eq!(
        request_roles(&requests[2]),
        ["system", "user", "assistant", "user", "assistant", "user"]
    );

    // The summary request: Python's instructions, the transcript of the span,
    // a bounded answer, and no tools.
    let summary = &requests[3].body;
    let constants: Value = serde_json::from_str(&fixture("memory/working.json")).unwrap();
    assert_eq!(
        summary["messages"][0]["content"],
        constants["SUMMARY_SYSTEM_PROMPT"]
    );
    assert_eq!(
        summary["messages"][1]["content"].as_str().unwrap(),
        format!("USER: Remember this note: {}", "x".repeat(980))
    );
    assert_eq!(summary["max_completion_tokens"], 1200);
    assert!(summary.get("tools").is_none(), "{summary}");

    // The fourth turn's request: the summary leads, everything newer is kept.
    assert_eq!(
        request_roles(&requests[4]),
        [
            "system",
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant",
            "user"
        ]
    );
    assert_eq!(
        requests[4].body["messages"][1]["content"],
        format!(
            "{}\nGOAL: keep the long note.",
            constants["SUMMARY_HEADER"].as_str().unwrap()
        )
    );

    // The compacted history is what the session keeps.
    let (_, state) = workspace.stored_session(&session).unwrap();
    let messages = state["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 8);
    assert!(messages[0]["content"]
        .as_str()
        .unwrap()
        .starts_with("[Conversation summary"));
    assert_eq!(state["last_turn"]["compactions"], 1);

    let events = trace_events(&workspace, "compaction");
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["status"], "ok");
    assert_eq!(event["dropped"], 1);
    assert_eq!(event["dropped_tokens"], 250);
    assert_eq!(event["kept"], 7);
    assert_eq!(event["model"], "test/model");

    // The originals are archived as an episode recall never returns.
    let connection = rusqlite::Connection::open(workspace.memory_db()).unwrap();
    let (task, outcome, metadata): (String, String, String) = connection
        .query_row(
            "SELECT task, outcome, metadata_json FROM episodes WHERE task LIKE '[compacted%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(task, "[compacted 1 messages] GOAL: keep the long note.");
    let archived: Value = serde_json::from_str(&outcome).unwrap();
    assert_eq!(archived[0]["role"], "user");
    assert_eq!(archived[0]["tool_calls"], json!([]));
    assert_eq!(archived[0]["name"], Value::Null);
    assert_eq!(
        serde_json::from_str::<Value>(&metadata).unwrap(),
        json!({"kind": "compaction", "dropped": 1, "summary": "GOAL: keep the long note."})
    );
}

#[tokio::test]
async fn a_failed_summary_falls_back_to_dropping_the_oldest_turns() {
    let server = MockServer::start(vec![
        sse_text("ok 1", "stop"),
        sse_text("ok 2", "stop"),
        sse_text("ok 3", "stop"),
        Reply::Status {
            status: 400,
            headers: Vec::new(),
            body: json!({"error": {"message": "no summaries today"}}).to_string(),
        },
        sse_text("ok 4", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("compaction-failed", &server);
    workspace.append_project_config("\n[limits]\nmax_context_tokens = 260\n");
    let session = long_session(&workspace).await;

    let fourth = workspace
        .run(&["run", "-s", &session, "fourth question"])
        .await;
    assert!(fourth.status.success(), "{}", stderr(&fourth));
    assert_eq!(
        stdout(&fourth),
        "\n[compaction failed; dropped 1 messages]\nok 4\n"
    );
    // The request was fitted instead: the long goal is gone, nothing replaced it.
    assert_eq!(
        request_roles(&server.requests()[4]),
        [
            "system",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant",
            "user"
        ]
    );
    let (_, state) = workspace.stored_session(&session).unwrap();
    assert_eq!(state["messages"].as_array().unwrap().len(), 8);

    let events = trace_events(&workspace, "compaction");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["status"], "failed");
    assert_eq!(events[0]["fallback"], "drop");
    assert_eq!(events[0]["dropped"], 1);
    assert!(
        events[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("ProviderError: "),
        "{events:?}"
    );
}

#[tokio::test]
async fn drop_mode_trims_requests_without_summarizing() {
    let server = MockServer::start(vec![
        sse_text("ok 1", "stop"),
        sse_text("ok 2", "stop"),
        sse_text("ok 3", "stop"),
        sse_text("ok 4", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("compaction-drop", &server);
    workspace
        .append_project_config("\n[limits]\nmax_context_tokens = 260\ncompaction = \"drop\"\n");
    let session = long_session(&workspace).await;

    let fourth = workspace
        .run(&["run", "-s", &session, "fourth question"])
        .await;
    assert!(fourth.status.success(), "{}", stderr(&fourth));
    assert_eq!(stdout(&fourth), "ok 4\n");
    assert_eq!(server.requests().len(), 4);
    assert_eq!(
        request_roles(&server.requests()[3]),
        [
            "system",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant",
            "user"
        ]
    );
    assert!(trace_events(&workspace, "compaction").is_empty());
}

#[tokio::test]
async fn skills_are_advertised_loaded_and_enforce_their_tool_list() {
    // The workspace's path is known before it exists, so the scripted read can name
    // the user skill's bundled file, which sits outside the workspace.
    let root = std::env::temp_dir().join(format!("agent86-cli-skills-{}", std::process::id()));
    let user_skill = root
        .join("home")
        .join(".claude")
        .join("skills")
        .join("style");
    let read = json!({ "path": user_skill.join("reference.md").display().to_string() }).to_string();
    let server = MockServer::start(vec![
        sse_tool_calls(&[("s", "use_skill", r#"{"name": "release-notes"}"#)]),
        sse_tool_calls(&[
            ("r", "read_file", &read),
            ("w", "write_file", r#"{"path": "notes.md", "content": "x"}"#),
        ]),
        sse_text("Done.", "stop"),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("skills", &server);
    assert_eq!(workspace.root, root);
    let project_skill = workspace
        .project()
        .join(".agent86")
        .join("skills")
        .join("notes");
    std::fs::create_dir_all(&project_skill).unwrap();
    std::fs::write(
        project_skill.join("SKILL.md"),
        "---\nname: release-notes\ndescription: >\n  Draft release notes\n  from the changelog.\nallowed-tools: read_file\n---\nRead the reference first.\n",
    )
    .unwrap();
    std::fs::create_dir_all(&user_skill).unwrap();
    std::fs::write(
        user_skill.join("SKILL.md"),
        "---\nname: style\ndescription: House style.\n---\nBe brief.\n",
    )
    .unwrap();
    std::fs::write(user_skill.join("reference.md"), "use the past tense").unwrap();

    let output = workspace.run(&["run", "--yes", "draft the notes"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = server.requests();

    // Advertised: names and one-line descriptions in the system message, and the tool.
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.ends_with(
            "\n\nAvailable skills (call use_skill with the name to load full instructions):\n\
             - release-notes: Draft release notes from the changelog. [tools while active: read_file]\n\
             - style: House style."
        ),
        "{system}"
    );
    let tools: Vec<&str> = requests[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"use_skill"), "{tools:?}");

    // Loaded on demand.
    let results = |index: usize| -> Vec<Value> {
        requests[index].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .cloned()
            .collect()
    };
    assert_eq!(
        results(1)[0]["content"],
        "Read the reference first.\n\nWhile this skill is active you may only call these \
         tools: read_file (plus use_skill). Other tools will be refused."
    );

    // While active, the listed tool runs, even on a skill's bundled file outside the
    // workspace, and anything else is refused.
    let results = results(2);
    assert_eq!(results.len(), 3);
    assert_eq!(results[1]["content"], "use the past tense");
    assert_eq!(
        results[2]["content"],
        "Tool 'write_file' is not permitted while the skill 'release-notes' is active. That \
         skill's allowed-tools are: read_file. Use one of those, or finish with the skill first."
    );
    assert!(!workspace.project().join("notes.md").exists());
}

#[tokio::test]
async fn run_flags_override_the_step_and_output_limits() {
    // Beyond Python, which reads these limits only from config. The output cap
    // replaces the provider's `max_tokens` too, so the flag always takes effect.
    let server = MockServer::start(vec![
        support::sse_tool_calls(&[("a", "list_dir", "{}")]),
        support::sse_tool_calls(&[("b", "list_dir", "{}")]),
    ])
    .await;
    let workspace = Workspace::with_mock_provider("limit-flags", &server);
    workspace.append_project_config("max_tokens = 4096\n");

    let output = workspace
        .run(&[
            "run",
            "--max-steps",
            "1",
            "--max-output-tokens",
            "32000",
            "--max-agent-steps",
            "3",
            "look",
        ])
        .await;

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("step budget reached (1 steps)"),
        "{}",
        stderr(&output)
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["max_completion_tokens"], 32000);

    for args in [
        ["run", "--max-steps", "0", "x"],
        ["run", "--max-output-tokens", "-5", "x"],
        ["run", "--max-agent-steps", "many", "x"],
    ] {
        let output = workspace.run(&args).await;
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[tokio::test]
async fn models_and_config_list_providers_roles_and_where_config_is_read() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::with_mock_provider("models", &server);
    // Every provider's key variable is set, so the listing never consults the real
    // OS keyring (the environment is checked first).
    let listing = |args: &'static [&'static str]| {
        let mut command = workspace.command(args);
        for variable in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OPENROUTER_API_KEY",
            "GROQ_API_KEY",
        ] {
            command.env(variable, "placeholder");
        }
        tokio::task::spawn_blocking(move || command.output().unwrap())
    };

    // As Python's `_list_models`: providers, the keyring line, then the roles.
    let models = listing(&["models"]).await.unwrap();
    assert!(models.status.success(), "{}", stderr(&models));
    let text = stdout(&models);
    let providers = text.find("Providers\n").unwrap();
    let keyring = text.find("\nOS keyring: available\n").unwrap();
    let roles = text.find("Model roles\n").unwrap();
    assert!(providers < keyring && keyring < roles, "{text}");
    assert!(
        text.lines().any(|line| line.starts_with("default ")
            && line.contains(" mock:test/model ")
            && line.ends_with(" ok")),
        "{text}"
    );
    assert!(text.contains("AGENT86_TEST_MOCK_KEY  env"), "{text}");
    assert!(!text.contains("keyring\n"), "{text}");
    // A bare `config` is the same listing.
    assert_eq!(stdout(&listing(&["config"]).await.unwrap()), text);

    let path = stdout(&workspace.run(&["config", "path"]).await);
    let lines: Vec<&str> = path.lines().collect();
    assert_eq!(lines.len(), 3, "{path}");
    assert!(lines[0].starts_with("Layer"), "{path}");
    let user = workspace
        .root
        .join("home")
        .join(".agent86")
        .join("config.toml");
    let project = workspace.project().join(".agent86").join("config.toml");
    assert_eq!(lines[1], format!("user     {} (exists)", user.display()));
    assert_eq!(lines[2], format!("project  {} (exists)", project.display()));

    // `config show` is the resolved configuration as JSON, and `--sandbox`
    // overrides the mode as Python's global option does.
    let show = workspace
        .run(&["--sandbox", "docker", "config", "show"])
        .await;
    let resolved: Value = serde_json::from_str(&stdout(&show)).unwrap();
    assert_eq!(resolved["model"]["default"], "mock:test/model");
    assert_eq!(resolved["sandbox"]["mode"], "docker");
    let invalid = workspace.run(&["--sandbox", "vm", "config", "show"]).await;
    assert_eq!(invalid.status.code(), Some(1));
    assert!(
        stderr(&invalid).contains("`agent86 config path` lists every layer that is read."),
        "{}",
        stderr(&invalid)
    );
}

#[tokio::test]
async fn a_model_that_cannot_be_built_points_at_agent86_models() {
    let workspace = Workspace::new("model-help");
    let output = workspace.run(&["--model", "nope", "run", "x"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).ends_with(
            "Run `agent86 models` to see the configured providers and which of them has a \
             key, then pick one with `--model provider:model`.\n"
        ),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn skills_are_listed_and_shown_with_their_resources() {
    let workspace = Workspace::new("skills-cli");
    let empty = stdout(&workspace.run(&["skills", "list"]).await);
    assert!(empty.starts_with("No skills found. Searched: "), "{empty}");
    assert!(empty.contains(".agent86"), "{empty}");

    let skill = workspace
        .project()
        .join(".agent86")
        .join("skills")
        .join("release");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: release\ndescription: Cut a release\n---\nBump the version, then tag.\n",
    )
    .unwrap();
    std::fs::write(skill.join("checklist.md"), "- tests\n").unwrap();

    let list = stdout(&workspace.run(&["skills", "list"]).await);
    assert_eq!(list, "Skill    Description\nrelease  Cut a release\n");

    let show = workspace.run(&["skills", "show", "release"]).await;
    assert!(show.status.success(), "{}", stderr(&show));
    assert_eq!(
        stdout(&show),
        "release - Cut a release\n\nBump the version, then tag.\n\nresources: checklist.md\n"
    );

    let missing = workspace.run(&["skills", "show", "deploy"]).await;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        stderr(&missing),
        "No skill named 'deploy'. Known: release. Run `agent86 skills list` to see where \
         they were discovered from.\n"
    );
}
