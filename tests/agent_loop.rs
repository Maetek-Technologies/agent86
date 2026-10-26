//! The ReAct loop against the mock provider: tool round trips, error
//! observations, circuit breakers, and continuations.

mod support;

use agent86::agent::{Harness, HarnessError, CONTINUE_PROMPT};
use agent86::approval::{ApprovalGate, ApprovalPrompt};
use agent86::config::LimitsConfig;
use agent86::provider::openai::{OpenAiConfig, OpenAiProvider};
use agent86::tools::{builtin_registry, ToolContext};
use agent86::turn::RunOutput;
use agent86::types::{ApprovalMode, Message, Role};
use serde_json::{json, Value};
use std::path::PathBuf;
use support::{sse_text, sse_tool_calls, MockServer, Reply};

/// A throwaway workspace with a couple of files in it.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-loop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("README.md"), "# Demo\nA demo project.\n").unwrap();
        std::fs::write(root.join("notes.txt"), "remember the milk\n").unwrap();
        Self { root }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn limits() -> LimitsConfig {
    LimitsConfig {
        max_steps: 40,
        max_cost_usd: 5.0,
        max_wall_clock_s: 900,
        max_consecutive_errors: 3,
        max_output_tokens: 8192,
        parallel_tools: true,
    }
}

fn provider(server: &MockServer) -> OpenAiProvider {
    let config = OpenAiConfig {
        base_url: Some(server.base_url.clone()),
        require_key: false,
        max_retries: 0,
        ..OpenAiConfig::default()
    };
    OpenAiProvider::new("test/model", config, None).unwrap()
}

fn system() -> Message {
    Message {
        role: Role::System,
        content: "You are a test.".to_owned(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}

async fn run(
    server: &MockServer,
    workspace: &Workspace,
    limits: LimitsConfig,
    goal: &str,
) -> Result<RunOutput, HarnessError> {
    // `ask` with no prompt is what a piped `agent86 run` gets.
    run_gated(
        server,
        workspace,
        limits,
        goal,
        ApprovalGate::new(ApprovalMode::Ask, None),
    )
    .await
}

async fn run_gated(
    server: &MockServer,
    workspace: &Workspace,
    limits: LimitsConfig,
    goal: &str,
    gate: ApprovalGate,
) -> Result<RunOutput, HarnessError> {
    let provider = provider(server);
    let harness = Harness {
        provider: &provider,
        registry: builtin_registry(),
        context: ToolContext::new(&workspace.root).unwrap(),
        limits,
        system: system(),
        max_tokens: 1024,
        gate,
    };
    harness.run_turn(goal, |_| {}).await
}

#[tokio::test]
async fn tool_results_are_fed_back_and_the_turn_ends_with_text() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("call_1", "list_dir", "{}")]),
        sse_text("The project has a README.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("round-trip");

    let output = run(&server, &workspace, limits(), "What is here?")
        .await
        .unwrap();

    assert_eq!(
        output.output,
        "\n[tool] list_dir({})\n[tool] list_dir -> src/\nThe project has a README."
    );
    assert_eq!(output.steps, 2);
    let turn = output.turn.unwrap();
    assert_eq!(turn.steps, 2);
    assert_eq!(turn.tool_calls, 1);
    assert_eq!(turn.input_tokens, 30);
    assert_eq!(output.usage.output_tokens, 13);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let tools: Vec<&str> = requests[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["read_file", "write_file", "edit_file", "list_dir"]);

    let second = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(second.len(), 4);
    assert_eq!(second[0]["role"], "system");
    assert_eq!(
        second[1],
        json!({"role": "user", "content": "What is here?"})
    );
    assert_eq!(
        second[2],
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "list_dir", "arguments": "{}"},
            }],
        })
    );
    assert_eq!(
        second[3],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "src/\nnotes.txt\nREADME.md"})
    );
}

#[tokio::test]
async fn parallel_calls_are_observed_in_call_order() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "read_file", r#"{"path": "notes.txt"}"#),
            ("b", "read_file", r#"{"path": "README.md"}"#),
        ]),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("parallel");

    let output = run(&server, &workspace, limits(), "read both")
        .await
        .unwrap();

    assert_eq!(
        output.output,
        concat!(
            "\n[tool] read_file({\"path\": \"notes.txt\"})\n",
            "\n[tool] read_file({\"path\": \"README.md\"})\n",
            "[tool] read_file -> remember the milk\n",
            "[tool] read_file -> # Demo\n",
            "done"
        )
    );
    let messages = server.requests()[1].body["messages"].clone();
    assert_eq!(messages[3]["tool_call_id"], "a");
    assert_eq!(messages[3]["content"], "remember the milk\n");
    assert_eq!(messages[4]["tool_call_id"], "b");
}

#[tokio::test]
async fn tool_failures_and_bad_json_are_observed_not_fatal() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "read_file", r#"{"path": "../escape.txt"}"#),
            ("b", "read_file", r#"{"path": "#),
            ("c", "run_command", r#"{"command": "ls"}"#),
        ]),
        sse_text("I could not do that.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("failures");

    let output = run(&server, &workspace, limits(), "try things")
        .await
        .unwrap();

    let messages = server.requests()[1].body["messages"].clone();
    let observed: Vec<&str> = (3..6)
        .map(|index| messages[index]["content"].as_str().unwrap())
        .collect();
    assert!(
        observed[0]
            .starts_with("PolicyError: Path '../escape.txt' resolves outside the workspace jail"),
        "{}",
        observed[0]
    );
    assert_eq!(
        observed[1],
        "invalid JSON in tool arguments: '{\"path\": ' could not be parsed. \
         Call the tool again with its arguments as a single valid JSON object."
    );
    assert_eq!(
        observed[2],
        "Unknown tool 'run_command'. Available: edit_file, list_dir, read_file, write_file."
    );
    assert!(output
        .output
        .contains("[tool] run_command -> error: Unknown tool 'run_command'."));
    assert!(output.output.ends_with("I could not do that."));
}

#[tokio::test]
async fn too_many_consecutive_tool_errors_trip_the_circuit() {
    let missing = || sse_tool_calls(&[("m", "read_file", r#"{"path": "missing.txt"}"#)]);
    let server = MockServer::start(vec![missing(), missing(), missing(), missing()]).await;
    let workspace = Workspace::new("error-streak");

    let error = run(&server, &workspace, limits(), "loop")
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "Circuit tripped: aborted after 4 consecutive tool errors"
    );
    assert_eq!(server.requests().len(), 4);
}

#[tokio::test]
async fn the_step_budget_trips_before_another_model_call() {
    let server = MockServer::start(vec![sse_tool_calls(&[("a", "list_dir", "{}")])]).await;
    let workspace = Workspace::new("step-budget");
    let tight = LimitsConfig {
        max_steps: 1,
        ..limits()
    };

    let error = run(&server, &workspace, tight, "go").await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "Circuit tripped: step budget reached (1 steps)"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn answers_cut_off_by_the_output_cap_are_continued() {
    let server = MockServer::start(vec![
        sse_text("Hello, wor", "length"),
        sse_text("ld!", "stop"),
    ])
    .await;
    let workspace = Workspace::new("continuation");

    let output = run(&server, &workspace, limits(), "greet").await.unwrap();

    assert_eq!(output.output, "Hello, wor\n[continuation 1/3]\nld!");
    assert_eq!(output.steps, 2);
    assert_eq!(output.turn.unwrap().continuations, 1);
    let second = server.requests()[1].body["messages"].clone();
    assert_eq!(
        second[2],
        json!({"role": "assistant", "content": "Hello, wor"})
    );
    assert_eq!(
        second[3],
        json!({"role": "user", "content": CONTINUE_PROMPT})
    );
}

#[tokio::test]
async fn continuations_stop_after_three() {
    let cut = || sse_text("more", "length");
    let server = MockServer::start(vec![cut(), cut(), cut(), cut()]).await;
    let workspace = Workspace::new("continuation-cap");

    let output = run(&server, &workspace, limits(), "ramble").await.unwrap();

    assert_eq!(server.requests().len(), 4);
    assert_eq!(output.turn.unwrap().continuations, 3);
    assert!(output.output.ends_with("[continuation 3/3]\nmore"));
}

#[tokio::test]
async fn provider_failures_mid_turn_surface_as_provider_errors() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("a", "list_dir", "{}")]),
        Reply::Status {
            status: 400,
            headers: Vec::new(),
            body: "bad request".to_owned(),
        },
    ])
    .await;
    let workspace = Workspace::new("provider-failure");

    let error = run(&server, &workspace, limits(), "go").await.unwrap_err();

    assert!(matches!(error, HarnessError::Provider(_)), "{error:?}");
    assert_eq!(error.to_string(), "HTTP 400: bad request");
}

#[test]
fn arguments_sent_back_to_the_model_use_python_json_dumps() {
    // Covered end to end above for "{}"; check the ensure_ascii form here.
    let mut arguments = serde_json::Map::new();
    arguments.insert("path".to_owned(), Value::String("caf\u{e9}.txt".to_owned()));
    let text = agent86::pyjson::python_json_dumps(&Value::Object(arguments), true);
    assert_eq!(text, format!("{{\"path\": \"caf{}u00e9.txt\"}}", '\x5c'));
}

fn write_then_answer() -> Vec<Reply> {
    vec![
        sse_tool_calls(&[(
            "w",
            "write_file",
            r#"{"path": "out.txt", "content": "hello\n"}"#,
        )]),
        sse_text("done", "stop"),
    ]
}

/// The tool observation the model received in the second request.
fn observed(server: &MockServer) -> String {
    server.requests()[1].body["messages"][3]["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn without_a_prompt_ask_declines_side_effects() {
    let server = MockServer::start(write_then_answer()).await;
    let workspace = Workspace::new("ask-no-prompt");

    let output = run(&server, &workspace, limits(), "write it")
        .await
        .unwrap();

    assert_eq!(
        observed(&server),
        "Not executed: approval required but no prompt available."
    );
    assert!(output.output.contains(
        "[tool] write_file -> error: Not executed: approval required but no prompt available.\n"
    ));
    assert!(!workspace.root.join("out.txt").exists());
}

#[tokio::test]
async fn deny_blocks_side_effects() {
    let server = MockServer::start(write_then_answer()).await;
    let workspace = Workspace::new("deny");
    let gate = ApprovalGate::new(ApprovalMode::Deny, None);

    run_gated(&server, &workspace, limits(), "write it", gate)
        .await
        .unwrap();

    assert_eq!(
        observed(&server),
        "Not executed: blocked by approval policy (deny)."
    );
    assert!(!workspace.root.join("out.txt").exists());
}

#[tokio::test]
async fn an_approving_prompt_lets_the_write_run() {
    let server = MockServer::start(write_then_answer()).await;
    let workspace = Workspace::new("approve");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = std::sync::Arc::clone(&seen);
    let prompt: ApprovalPrompt = Box::new(move |name, preview| {
        record
            .lock()
            .unwrap()
            .push((name.to_owned(), preview.detail.clone()));
        true
    });

    run_gated(
        &server,
        &workspace,
        limits(),
        "write it",
        ApprovalGate::new(ApprovalMode::Ask, Some(prompt)),
    )
    .await
    .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        [(
            "write_file".to_owned(),
            Some("new file: out.txt (1 line)\nhello\n".to_owned())
        )]
    );
    assert_eq!(
        observed(&server),
        "Wrote 6 bytes to out.txt (new file).\nnew file, 1 line"
    );
    assert!(workspace.root.join("out.txt").exists());
}

#[tokio::test]
async fn a_declining_prompt_is_reported_to_the_model() {
    let server = MockServer::start(write_then_answer()).await;
    let workspace = Workspace::new("decline");
    let prompt: ApprovalPrompt = Box::new(|_, _| false);

    run_gated(
        &server,
        &workspace,
        limits(),
        "write it",
        ApprovalGate::new(ApprovalMode::Ask, Some(prompt)),
    )
    .await
    .unwrap();

    assert_eq!(observed(&server), "Not executed: declined by user.");
    assert!(!workspace.root.join("out.txt").exists());
}

#[tokio::test]
async fn mixed_batches_prompt_once_per_write_and_observe_in_call_order() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("w1", "write_file", r#"{"path": "a.txt", "content": "A"}"#),
            ("r1", "read_file", r#"{"path": "notes.txt"}"#),
            (
                "w2",
                "edit_file",
                r#"{"path": "notes.txt", "old": "milk", "new": "eggs"}"#,
            ),
            ("r2", "read_file", r#"{"path": "README.md"}"#),
        ]),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("mixed");
    let prompts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = std::sync::Arc::clone(&prompts);
    let prompt: ApprovalPrompt = Box::new(move |_, _| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    });

    run_gated(
        &server,
        &workspace,
        limits(),
        "go",
        ApprovalGate::new(ApprovalMode::Ask, Some(prompt)),
    )
    .await
    .unwrap();

    assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 2);
    let messages = server.requests()[1].body["messages"].clone();
    let ids: Vec<&str> = (3..7)
        .map(|i| messages[i]["tool_call_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["w1", "r1", "w2", "r2"]);
    // Reads run before writes, so r1 saw the file before w2 edited it.
    assert_eq!(messages[4]["content"], "remember the milk\n");
    assert!(messages[5]["content"]
        .as_str()
        .unwrap()
        .starts_with("Edited notes.txt (1 replacement)."));
    assert_eq!(
        std::fs::read_to_string(workspace.root.join("notes.txt")).unwrap(),
        "remember the eggs\n"
    );
}
