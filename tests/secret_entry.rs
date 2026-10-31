//! Secure secret entry, beyond Python (`agent86::vault`): the user enters a
//! secret in a masked prompt, tools get it as an environment variable, and the
//! model, the history, the output, and the trace only ever see its name.

mod support;

use agent86::agent::Harness;
use agent86::approval::ApprovalGate;
use agent86::config::{
    AgentsConfig, CompactionMode, EgressMode, IngressMode, LimitsConfig, ObservabilityConfig,
    RedactMode,
};
use agent86::guardrails::Guardrails;
use agent86::observability::Recorder;
use agent86::provider::openai::{OpenAiConfig, OpenAiProvider};
use agent86::secrets::{SecretStore, SecretStoreError};
use agent86::session::AgentState;
use agent86::tools::{default_registry, ToolContext, ToolRegistry};
use agent86::types::{ApprovalMode, Message, Role, ToolCall};
use agent86::vault::{SecretAnswer, SecretRequest};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use support::{sse_text, sse_tool_calls, MockServer};

const VALUE: &str = "tok_9f8e7d6c5b4a3921";
const PLACEHOLDER: &str = "${secret:API_TOKEN}";

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("agent86-secret-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("traces")).unwrap();
        Self { root }
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.root.join("traces").join("trace.jsonl")).unwrap_or_default()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn limits() -> LimitsConfig {
    LimitsConfig {
        max_steps: 10,
        max_cost_usd: 5.0,
        max_wall_clock_s: 900,
        max_consecutive_errors: 3,
        max_context_tokens: 0,
        max_output_tokens: 8192,
        context_reserve_tokens: 4096,
        compaction: CompactionMode::Summarize,
        parallel_tools: true,
        tool_timeout_s: 60,
    }
}

fn harness(server: &MockServer, workspace: &Workspace, registry: ToolRegistry) -> Harness {
    let config = OpenAiConfig {
        base_url: Some(server.base_url.clone()),
        require_key: false,
        max_retries: 0,
        ..OpenAiConfig::default()
    };
    let context = ToolContext::new(&workspace.root).unwrap();
    let recorder = Recorder::from_config(&ObservabilityConfig {
        trace: true,
        path: workspace.root.join("traces").display().to_string(),
        otel: false,
        otel_exporter: Default::default(),
        otel_endpoint: None,
        // Python's own redaction off, so only the vault keeps the value out.
        redact: RedactMode::None,
        max_field_chars: 2000,
        max_trace_bytes: 0,
        keep_traces: 5,
    })
    .with_secrets(context.secrets.clone());
    Harness {
        provider: Arc::new(OpenAiProvider::new("test/model", config, None).unwrap()),
        router: Default::default(),
        tracer: Default::default(),
        registry,
        context,
        limits: limits(),
        agents: AgentsConfig {
            enabled: false,
            max_depth: 2,
            max_steps: 8,
        },
        system: Message {
            role: Role::System,
            content: "You are a test.".to_owned(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        },
        max_tokens: 1024,
        session_limits: Default::default(),
        context_window: 128_000,
        gate: ApprovalGate::new(ApprovalMode::Auto, None),
        guardrails: Guardrails {
            ingress: IngressMode::Warn,
            egress: EgressMode::Warn,
            scan_observations: true,
        },
        recorder,
        mcp: None,
        memory: None,
        cancel: Default::default(),
    }
}

/// A command that prints the variable `name`, in the platform's shell.
fn print_variable(name: &str) -> String {
    if cfg!(windows) {
        format!("echo got %{name}%")
    } else {
        format!("echo got ${name}")
    }
}

fn arguments(value: Value) -> String {
    value.to_string()
}

#[tokio::test]
async fn an_entered_secret_reaches_tools_but_never_the_model_the_history_or_the_trace() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[(
            "a",
            "request_secret",
            &arguments(json!({"name": "API_TOKEN", "reason": "to call the billing API"})),
        )]),
        sse_tool_calls(&[(
            "b",
            "run_command",
            &arguments(json!({"command": print_variable("API_TOKEN")})),
        )]),
        sse_text("All set.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("flow");
    let harness = harness(
        &server,
        &workspace,
        default_registry(false, false, false, true),
    );
    let asked: Arc<Mutex<Vec<SecretRequest>>> = Arc::default();
    let seen = Arc::clone(&asked);
    harness
        .context
        .secrets
        .set_prompt(Some(Box::new(move |request| {
            seen.lock().unwrap().push(request.clone());
            Some(SecretAnswer {
                value: VALUE.to_owned(),
                remember: false,
            })
        })));

    let mut state = AgentState::new();
    let output = harness
        .run_turn(&mut state, "Check my invoices", |_| {})
        .await
        .unwrap();
    drop(harness);

    assert_eq!(
        *asked.lock().unwrap(),
        [SecretRequest {
            name: "API_TOKEN".to_owned(),
            reason: "to call the billing API".to_owned(),
            replacing: false,
            // No keyring is attached to this harness, so none is offered.
            can_remember: false,
            remembered: false,
        }]
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    // The tool is offered, and its result names the variable, not the value.
    let tools: Vec<&str> = requests[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools.last(), Some(&"request_secret"));
    let second = requests[1].body["messages"].as_array().unwrap();
    let announced = second[3]["content"].as_str().unwrap();
    assert!(
        announced.starts_with("The secret API_TOKEN is set."),
        "{announced}"
    );
    // From then on the system message names it.
    let system = second[0]["content"].as_str().unwrap();
    assert!(system.contains("## Secrets\n"), "{system}");
    // The secrets section is last before the turn's time line.
    assert!(
        system.contains("\n- API_TOKEN\n\nCurrent date and time: "),
        "{system}"
    );
    // The command saw the value; the model sees the placeholder in its place.
    let third = requests[2].body["messages"].as_array().unwrap();
    let observed = third.last().unwrap()["content"].as_str().unwrap();
    assert_eq!(
        observed,
        format!("exit code: 0\nstdout:\ngot {PLACEHOLDER}")
    );

    // Nowhere else either.
    for request in &requests {
        assert!(!request.body.to_string().contains(VALUE));
    }
    assert!(!output.contains(VALUE), "{output}");
    assert!(!serde_json::to_string(&state).unwrap().contains(VALUE));
    let trace = workspace.trace();
    assert!(trace.contains("\"tool_call\""), "{trace}");
    assert!(!trace.contains(VALUE), "{trace}");
}

#[tokio::test]
async fn a_secret_in_the_users_text_or_the_models_reply_is_replaced() {
    let server = MockServer::start(vec![sse_text(
        &format!("You told me {VALUE}; I will not repeat it."),
        "stop",
    )])
    .await;
    let workspace = Workspace::new("text");
    let harness = harness(
        &server,
        &workspace,
        default_registry(false, false, false, true),
    );
    harness.context.secrets.set("API_TOKEN", VALUE.to_owned());

    let mut state = AgentState::new();
    let mut streamed = String::new();
    let output = harness
        .run_turn(&mut state, &format!("my token is {VALUE}"), |text| {
            streamed.push_str(text)
        })
        .await
        .unwrap();
    // Whatever an event carries, the recorder takes the value out.
    harness
        .recorder
        .event(&state.session_id, "probe", vec![("detail", json!(VALUE))]);
    drop(harness);

    let sent = &server.requests()[0].body["messages"];
    assert_eq!(sent[1]["content"], format!("my token is {PLACEHOLDER}"));
    let expected = format!("You told me {PLACEHOLDER}; I will not repeat it.");
    assert_eq!(output, expected);
    assert_eq!(streamed, expected);
    assert!(!serde_json::to_string(&state).unwrap().contains(VALUE));
    let trace = workspace.trace();
    assert!(trace.contains("turn_start"), "{trace}");
    assert!(
        trace.contains(&format!("\"detail\": \"{PLACEHOLDER}\"")),
        "{trace}"
    );
    assert!(!trace.contains(VALUE), "{trace}");
}

fn call(name: &str, arguments: Value) -> ToolCall {
    let Value::Object(arguments) = arguments else {
        unreachable!()
    };
    ToolCall {
        id: "c1".to_owned(),
        name: name.to_owned(),
        arguments,
    }
}

fn context(root: &Path) -> ToolContext {
    ToolContext::new(root).unwrap()
}

#[test]
fn nobody_to_ask_means_no_tool_and_a_clear_refusal() {
    // Unattended runs are offered exactly Python's tools.
    assert!(!default_registry(true, true, true, false)
        .names()
        .contains(&"request_secret".to_owned()));

    let workspace = Workspace::new("unattended");
    let registry = default_registry(false, false, false, true);
    let request = call("request_secret", json!({"name": "A", "reason": "r"}));

    let result = registry.dispatch(&request, &context(&workspace.root));
    assert!(!result.ok);
    assert_eq!(
        result.error.as_deref(),
        Some(
            "Cannot ask for the secret A: nobody is at a terminal to enter it. Ask the user \
             to run agent86 interactively and enter it with /secret A."
        )
    );

    let ctx = context(&workspace.root);
    ctx.secrets.set_prompt(Some(Box::new(|_| None)));
    let declined = registry.dispatch(&request, &ctx);
    assert_eq!(
        declined.error.as_deref(),
        Some("The user declined to enter the secret A.")
    );
    assert!(ctx.secrets.is_empty());
}

#[test]
fn a_name_must_be_an_environment_variable_name() {
    let workspace = Workspace::new("names");
    let registry = default_registry(false, false, false, true);
    let ctx = context(&workspace.root);
    ctx.secrets
        .set_prompt(Some(Box::new(|_| panic!("never asked"))));

    let result = registry.dispatch(
        &call("request_secret", json!({"name": "my key", "reason": "r"})),
        &ctx,
    );
    assert_eq!(
        result.error.as_deref(),
        Some(
            "Invalid arguments for 'request_secret': name: String should match pattern \
             '^[A-Za-z_][A-Za-z0-9_]*$'"
        )
    );
    let missing = registry.dispatch(&call("request_secret", json!({})), &ctx);
    assert_eq!(
        missing.error.as_deref(),
        Some(
            "Invalid arguments for 'request_secret': name: Field required; reason: Field required"
        )
    );
}

#[test]
fn every_tool_result_is_redacted_including_read_files() {
    let workspace = Workspace::new("files");
    std::fs::write(
        workspace.root.join(".env"),
        format!("API_TOKEN={VALUE}\nOTHER=1\n"),
    )
    .unwrap();
    let ctx = context(&workspace.root);
    ctx.secrets.set("API_TOKEN", VALUE.to_owned());
    let registry = default_registry(false, false, false, false);

    let read = registry.dispatch(&call("read_file", json!({"path": ".env"})), &ctx);
    assert!(read.ok, "{:?}", read.error);
    assert_eq!(read.content, format!("API_TOKEN={PLACEHOLDER}\nOTHER=1\n"));

    // An error message that quotes the value is redacted too.
    let missing = registry.dispatch(&call("read_file", json!({"path": VALUE})), &ctx);
    let error = missing.error.unwrap();
    assert!(error.contains(PLACEHOLDER), "{error}");
    assert!(!error.contains(VALUE), "{error}");
}

#[test]
fn the_tool_schema_is_pinned() {
    let registry = default_registry(false, false, false, true);
    let tool = registry.get("request_secret").unwrap();
    assert!(!tool.side_effecting());
    assert!(!tool.parallel_safe());
    let mut expected = Map::new();
    expected.insert(
        "properties".to_owned(),
        json!({
            "name": {
                "description": "The environment variable name to set it as, such as STRIPE_API_KEY.",
                "pattern": "^[A-Za-z_][A-Za-z0-9_]*$",
                "title": "Name",
                "type": "string",
            },
            "reason": {
                "description": "What it is for, shown to the user when they are asked.",
                "title": "Reason",
                "type": "string",
            },
        }),
    );
    expected.insert("required".to_owned(), json!(["name", "reason"]));
    expected.insert("type".to_owned(), json!("object"));
    assert_eq!(tool.parameters(), Value::Object(expected));
}

/// The `.env` file, with Windows' text-mode line endings undone.
fn env_file(workspace: &Workspace) -> String {
    std::fs::read_to_string(workspace.root.join(".env"))
        .unwrap()
        .replace("\r\n", "\n")
}

#[test]
fn file_tools_write_the_value_a_placeholder_names() {
    let workspace = Workspace::new("placeholders");
    let ctx = context(&workspace.root);
    ctx.secrets.set("API_TOKEN", VALUE.to_owned());
    let registry = default_registry(false, false, false, false);

    let written = registry.dispatch(
        &call(
            "write_file",
            json!({"path": ".env", "content": format!("API_TOKEN={PLACEHOLDER}\nOTHER=1\n")}),
        ),
        &ctx,
    );
    assert!(written.ok, "{:?}", written.error);
    assert_eq!(
        env_file(&workspace),
        format!("API_TOKEN={VALUE}\nOTHER=1\n")
    );
    assert!(!written.content.contains(VALUE), "{}", written.content);

    // What the model reads back, it can edit: the placeholder matches the value.
    let read = registry.dispatch(&call("read_file", json!({"path": ".env"})), &ctx);
    let line = read.content.lines().next().unwrap().to_owned();
    assert_eq!(line, format!("API_TOKEN={PLACEHOLDER}"));
    let edited = registry.dispatch(
        &call(
            "edit_file",
            json!({
                "path": ".env",
                "old_string": format!("{line}\nOTHER=1"),
                "new_string": format!("{line}\nOTHER=2"),
            }),
        ),
        &ctx,
    );
    assert!(edited.ok, "{:?}", edited.error);
    assert_eq!(
        env_file(&workspace),
        format!("API_TOKEN={VALUE}\nOTHER=2\n")
    );
    assert!(!edited.content.contains(VALUE), "{}", edited.content);
}

#[test]
fn an_unknown_placeholder_fails_and_writes_nothing() {
    let workspace = Workspace::new("unknown");
    let ctx = context(&workspace.root);
    ctx.secrets.set("API_TOKEN", VALUE.to_owned());
    let registry = default_registry(false, false, false, false);

    let result = registry.dispatch(
        &call(
            "write_file",
            json!({"path": ".env", "content": "KEY=${secret:STRIPE_KEY}\n"}),
        ),
        &ctx,
    );
    assert!(!result.ok);
    assert_eq!(
        result.error.as_deref(),
        Some(
            "Unknown secret STRIPE_KEY in ${secret:STRIPE_KEY}. The secrets held are \
             API_TOKEN; ask the user for it with request_secret."
        )
    );
    assert_eq!(result.call_id, "c1");
    assert!(!workspace.root.join(".env").exists());
}

#[test]
fn only_the_file_writing_tools_expand_placeholders() {
    // A command line or an MCP server never gets a value this way: commands use the
    // environment variable, and everything else gets the placeholder text.
    let registry = default_registry(true, true, true, true);
    let expanding: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|name| registry.get(name).unwrap().expands_secrets())
        .collect();
    assert_eq!(expanding, ["edit_file", "write_file"]);
}

#[test]
fn the_approval_preview_shows_the_real_change_with_the_value_hidden() {
    let workspace = Workspace::new("preview");
    std::fs::write(
        workspace.root.join(".env"),
        format!("API_TOKEN={VALUE}\nOTHER=1\n"),
    )
    .unwrap();
    let ctx = context(&workspace.root);
    ctx.secrets.set("API_TOKEN", VALUE.to_owned());
    let registry = default_registry(false, false, false, false);
    let tool = registry.get("write_file").unwrap();
    let request = call(
        "write_file",
        json!({"path": ".env", "content": format!("API_TOKEN={PLACEHOLDER}\nOTHER=2\n")}),
    );

    let preview = agent86::approval::build_preview(tool.as_ref(), &request, &ctx);
    let detail = preview.detail.unwrap();
    assert!(!detail.contains(VALUE), "{detail}");
    // The token line is unchanged context; only OTHER changes.
    assert!(
        detail.contains(&format!("\n API_TOKEN={PLACEHOLDER}\n")),
        "{detail}"
    );
    assert!(detail.contains("\n-OTHER=1\n+OTHER=2"), "{detail}");
    assert!(!detail.contains("-API_TOKEN"), "{detail}");
    assert!(preview.summary.contains(PLACEHOLDER), "{}", preview.summary);
}

/// An in-memory keyring that outlives a harness, as the real one outlives a session.
#[derive(Default)]
struct FakeKeyring {
    entries: Mutex<Vec<((String, String), String)>>,
}

impl SecretStore for FakeKeyring {
    fn get_password(&self, service: &str, account: &str) -> Option<String> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .find(|(key, _)| key.0 == service && key.1 == account)
            .map(|(_, value)| value.clone())
    }

    fn set_password(
        &self,
        service: &str,
        account: &str,
        secret: &str,
    ) -> Result<(), SecretStoreError> {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|(key, _)| !(key.0 == service && key.1 == account));
        entries.push(((service.to_owned(), account.to_owned()), secret.to_owned()));
        Ok(())
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<bool, SecretStoreError> {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|(key, _)| !(key.0 == service && key.1 == account));
        Ok(entries.len() != before)
    }
}

#[tokio::test]
async fn a_remembered_secret_is_there_in_the_next_session_and_never_in_what_the_model_sees() {
    let keyring = Arc::new(FakeKeyring::default());
    let workspace = Workspace::new("remember");

    // Session one: the agent asks, and the user chooses to remember it.
    let server = MockServer::start(vec![
        sse_tool_calls(&[(
            "a",
            "request_secret",
            &arguments(json!({"name": "API_TOKEN", "reason": "to call the billing API"})),
        )]),
        sse_text("Saved.", "stop"),
    ])
    .await;
    let first = harness(
        &server,
        &workspace,
        default_registry(false, false, false, true),
    );
    first
        .context
        .secrets
        .attach_store(keyring.clone(), &workspace.root);
    let asked: Arc<Mutex<Vec<SecretRequest>>> = Arc::default();
    let seen = Arc::clone(&asked);
    first
        .context
        .secrets
        .set_prompt(Some(Box::new(move |request| {
            seen.lock().unwrap().push(request.clone());
            Some(SecretAnswer {
                value: VALUE.to_owned(),
                remember: true,
            })
        })));
    let mut state = AgentState::new();
    first.run_turn(&mut state, "Go", |_| {}).await.unwrap();
    assert!(asked.lock().unwrap()[0].can_remember);
    let announced = server.requests()[1].body["messages"].as_array().unwrap()[3]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        announced.contains("remember it for this project"),
        "{announced}"
    );
    drop(first);

    // Session two, a new vault on the same keyring: the secret is loaded before any
    // turn, the system message names it, and a command can use it without a prompt.
    let server = MockServer::start(vec![
        sse_tool_calls(&[(
            "b",
            "run_command",
            &arguments(json!({"command": print_variable("API_TOKEN")})),
        )]),
        sse_text("Done.", "stop"),
    ])
    .await;
    let second = harness(
        &server,
        &workspace,
        default_registry(false, false, false, true),
    );
    second
        .context
        .secrets
        .attach_store(keyring.clone(), &workspace.root);
    assert_eq!(second.context.secrets.load_remembered(), ["API_TOKEN"]);
    let mut state = AgentState::new();
    second.run_turn(&mut state, "Use it", |_| {}).await.unwrap();
    let requests = server.requests();
    let system = requests[0].body["messages"].as_array().unwrap()[0]["content"]
        .as_str()
        .unwrap();
    assert!(system.contains("\n- API_TOKEN"), "{system}");
    let observed = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert_eq!(
        observed,
        format!("exit code: 0\nstdout:\ngot {PLACEHOLDER}")
    );
    for request in &requests {
        assert!(!request.body.to_string().contains(VALUE));
    }
    assert!(!serde_json::to_string(&state).unwrap().contains(VALUE));
    assert!(!workspace.trace().contains(VALUE));

    // /secret forget removes it from the session and the keyring, for good.
    assert_eq!(second.context.secrets.forget("API_TOKEN"), Ok(true));
    assert!(second.context.secrets.is_empty());
    let third = ToolContext::new(&workspace.root).unwrap();
    third.secrets.attach_store(keyring, &workspace.root);
    assert!(third.secrets.load_remembered().is_empty());
}
