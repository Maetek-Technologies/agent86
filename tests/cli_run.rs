//! End-to-end tests of `agent86 run` against the local mock provider.

mod support;

use serde_json::Value;
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
            .env_remove("AGENT86_MODEL")
            .env_remove("AGENT86_SANDBOX")
            .env_remove("AGENT86_APPROVAL");
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
}
