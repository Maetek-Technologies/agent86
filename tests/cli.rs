//! The `agent86 run` scripting contract, exercised through the real binary.

mod common;

use common::{fixture, MockServer, Reply};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

const KEY_ENV: &str = "AGENT86_RS_TEST_KEY";
const KEY: &str = "sk-cli-test-fedcba9876543210";

/// A scratch HOME and working directory whose project config points `mock` at `base_url`.
fn workspace(base_url: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".agent86")).unwrap();
    std::fs::write(
        dir.path().join(".agent86/config.toml"),
        format!(
            "[model]\ndefault = \"mock:test-model\"\n\n\
             [providers.mock]\nbase_url = \"{base_url}\"\napi_key_env = \"{KEY_ENV}\"\n\
             max_retries = 0\n"
        ),
    )
    .unwrap();
    dir
}

fn agent86(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent86"))
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env(KEY_ENV, KEY)
        .env_remove("AGENT86_MODEL")
        .output()
        .unwrap()
}

/// Keys of the pretty-printed top-level object, in the order they were written.
fn top_level_keys(json: &str) -> Vec<String> {
    keys_at_indent(json, 2)
}

/// Keys of the pretty-printed object stored under `field`, in written order.
fn nested_keys(json: &str, field: &str) -> Vec<String> {
    let start = json
        .find(&format!("\n  \"{field}\": {{"))
        .expect("field is an object");
    let body = &json[start..];
    let end = body.find("\n  }").expect("object closes");
    keys_at_indent(&body[..end], 4)
}

fn keys_at_indent(json: &str, indent: usize) -> Vec<String> {
    let prefix = format!("{}\"", " ".repeat(indent));
    json.lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .filter_map(|rest| rest.split_once('"').map(|(key, _)| key.to_owned()))
        .collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn run_json_emits_one_object_with_the_reference_fields() {
    let server = MockServer::start(vec![Reply::sse(fixture("text.sse"))]);
    let dir = workspace(&server.base_url());

    let output = agent86(dir.path(), &["run", "--json", "Say hello"]);

    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stderr),
        "",
        "nothing but the JSON may be printed"
    );
    let stdout = text(&output.stdout);
    let payload: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        top_level_keys(&stdout),
        ["session_id", "output", "steps", "usage", "turn"]
    );

    assert_eq!(payload["output"], "Hello, world!");
    assert_eq!(payload["steps"], 1);
    let session_id = payload["session_id"].as_str().unwrap();
    assert_eq!(session_id.len(), 12);
    assert_eq!(
        payload["usage"],
        serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 4,
            "cost_usd": 0.0,
            "cache_read_tokens": 8,
            "cache_creation_tokens": 0,
        })
    );
    assert_eq!(
        nested_keys(&stdout, "turn"),
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
    assert_eq!(payload["turn"]["steps"], 1);
    assert_eq!(payload["turn"]["input_tokens"], 12);
}

#[test]
fn plain_run_streams_the_answer_to_stdout_and_the_summary_to_stderr() {
    let server = MockServer::start(vec![Reply::sse(fixture("text.sse"))]);
    let dir = workspace(&server.base_url());

    let output = agent86(dir.path(), &["run", "Say hello"]);

    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "Hello, world!\n");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("— 1 step · 0 tools · 12 in / 4 out (8 cached) · cost n/a"),
        "{stderr}"
    );
}

#[test]
fn the_model_flag_overrides_the_config_default() {
    let server = MockServer::start(vec![Reply::sse(fixture("text.sse"))]);
    let dir = workspace(&server.base_url());

    let output = agent86(
        dir.path(),
        &["--model", "mock:other-model", "run", "--json", "hi"],
    );

    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(server.requests()[0].body["model"], "other-model");
}

#[test]
fn provider_failures_exit_1_with_nothing_on_stdout() {
    let server = MockServer::start(vec![
        Reply::status(500, "upstream exploded"),
        Reply::status(500, "upstream exploded"),
    ]);
    let dir = workspace(&server.base_url());

    for args in [&["run", "--json", "hi"][..], &["run", "hi"][..]] {
        let output = agent86(dir.path(), args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(text(&output.stdout), "", "{args:?}");
        let stderr = text(&output.stderr);
        assert!(
            stderr.contains("error: HTTP 500: upstream exploded"),
            "{stderr}"
        );
    }
}

#[test]
fn a_truncated_stream_is_not_reported_as_success() {
    let server = MockServer::start(vec![Reply::sse(fixture("truncated.sse"))]);
    let dir = workspace(&server.base_url());

    let output = agent86(dir.path(), &["run", "--json", "hi"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "", "no partial JSON result");
    assert!(text(&output.stderr).contains("before the model finished"));
}

#[test]
fn a_missing_key_names_the_env_var_to_set() {
    let server = MockServer::start(vec![]);
    let dir = workspace(&server.base_url());

    let output = Command::new(env!("CARGO_BIN_EXE_agent86"))
        .args(["run", "--json", "hi"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env_remove(KEY_ENV)
        .env_remove("AGENT86_MODEL")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert!(text(&output.stderr).contains(&format!("Set the {KEY_ENV} environment variable")));
    assert!(server.requests().is_empty());
}

#[test]
fn bad_model_references_fail_before_any_request() {
    let server = MockServer::start(vec![]);
    let dir = workspace(&server.base_url());

    for (model, expected) in [
        ("no-separator", "has no provider:model separator"),
        ("nowhere:model", "Unknown provider 'nowhere'"),
        (
            "anthropic:claude-opus-4-8",
            "not available in the Rust port yet",
        ),
    ] {
        let output = agent86(dir.path(), &["-m", model, "run", "--json", "hi"]);
        assert_eq!(output.status.code(), Some(1), "{model}");
        assert_eq!(text(&output.stdout), "", "{model}");
        assert!(
            text(&output.stderr).contains(expected),
            "{model}: {}",
            text(&output.stderr)
        );
    }
    assert!(server.requests().is_empty());
}

#[test]
fn a_malformed_config_file_is_named() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".agent86")).unwrap();
    std::fs::write(dir.path().join(".agent86/config.toml"), "[model\n").unwrap();

    let output = agent86(dir.path(), &["run", "--json", "hi"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("Malformed config at"), "{stderr}");
    assert!(stderr.contains("config.toml"), "{stderr}");
}

#[test]
fn the_api_key_never_reaches_the_output() {
    let server = MockServer::start(vec![Reply::status(
        401,
        format!(r#"{{"error":{{"message":"bad key {KEY}"}}}}"#),
    )]);
    let dir = workspace(&server.base_url());

    let output = agent86(dir.path(), &["run", "--json", "hi"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(!text(&output.stdout).contains(KEY));
    assert!(!text(&output.stderr).contains(KEY));
    assert!(text(&output.stderr).contains("bad key ***"));
}

#[test]
fn sessions_are_rejected_until_persistence_exists() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent86(dir.path(), &["run", "--session", "abc123", "hi"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("--session is not supported yet"));
}

#[test]
fn version_and_help_still_work() {
    let dir = tempfile::tempdir().unwrap();

    let version = agent86(dir.path(), &["--version"]);
    assert!(version.status.success());
    assert_eq!(
        text(&version.stdout),
        format!("agent86 {}\n", env!("CARGO_PKG_VERSION"))
    );

    let help = agent86(dir.path(), &[]);
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("run"));

    let missing_goal = agent86(dir.path(), &["run"]);
    assert_eq!(missing_goal.status.code(), Some(2));
}
