//! Skills the user runs as commands, beyond Python (`Harness::run_skill_turn`):
//! the skill's instructions reach the model in the user's message ahead of the
//! task, its `allowed-tools` apply from the first step and end with the turn, and
//! the turn is otherwise an ordinary one (trace, usage, saved messages, vault).

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
use agent86::session::AgentState;
use agent86::skills::{Skill, Skills};
use agent86::tools::{default_registry, ToolContext};
use agent86::types::{ApprovalMode, Message, Role};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use support::{sse_text, sse_tool_calls, MockServer};

const SECRET: &str = "tok_9f8e7d6c5b4a3921";

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agent86-skill-commands-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("traces")).unwrap();
        Self { root }
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.root.join("traces").join("trace.jsonl")).unwrap_or_default()
    }

    /// A skill folder under the project, returning the loaded skill.
    fn skill(&self, text: &str) -> Arc<Skill> {
        let dir = self
            .project()
            .join(".agent86")
            .join("skills")
            .join("pirate");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), text).unwrap();
        let project = self.root.join("config.toml");
        std::fs::write(&project, "").unwrap();
        let config = agent86::config::load_config(
            &agent86::config::ConfigPaths {
                user: None,
                project,
                project_trust: agent86::config::ProjectTrust::Trusted,
            },
            &agent86::config::Overrides::default(),
        )
        .unwrap();
        let found = agent86::skills::discover_skills(&config, &self.project(), None);
        found.values().next().unwrap().clone()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn harness(server: &MockServer, workspace: &Workspace, skill: &Arc<Skill>) -> Harness {
    let config = OpenAiConfig {
        base_url: Some(server.base_url.clone()),
        require_key: false,
        max_retries: 0,
        ..OpenAiConfig::default()
    };
    let mut context = ToolContext::new(&workspace.project()).unwrap();
    let mut skills = Skills::new();
    skills.insert(skill.name.clone(), Arc::clone(skill));
    context.skills = Arc::new(skills);
    let recorder = Recorder::from_config(&ObservabilityConfig {
        trace: true,
        path: workspace.root.join("traces").display().to_string(),
        otel: false,
        otel_exporter: Default::default(),
        otel_endpoint: None,
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
        registry: default_registry(false, true, false, false),
        context,
        limits: LimitsConfig {
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
        },
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

/// The last user message in a recorded request.
fn last_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap()
        .to_owned()
}

fn write_call(path: &str) -> String {
    json!({"path": path, "content": "x\n"}).to_string()
}

#[tokio::test]
async fn a_skill_turn_sends_the_instructions_and_restricts_tools_from_the_first_step() {
    let first = write_call("during.txt");
    let second = write_call("after.txt");
    let server = MockServer::start(vec![
        // The skill turn: a write the skill does not allow, then an answer.
        sse_tool_calls(&[("w1", "write_file", &first)]),
        sse_text("Arr.", "stop"),
        // The next, ordinary turn: the same write now runs.
        sse_tool_calls(&[("w2", "write_file", &second)]),
        sse_text("Done.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("restrict");
    let skill = workspace.skill(
        "---\nname: pirate-speak\ndescription: Pirate.\nallowed-tools: [read_file]\n---\n\nSpeak like a pirate.\n",
    );
    let harness = harness(&server, &workspace, &skill);
    harness.context.secrets.set("API_TOKEN", SECRET.to_owned());
    let mut state = AgentState::new();

    let task = format!("greet the crew with {SECRET}");
    let typed = format!("/pirate-speak {task}");
    harness
        .run_skill_turn(&mut state, &skill, &task, &typed, |_| {})
        .await
        .unwrap();

    let requests = server.requests();
    let sent = last_user(&requests[0].body);
    assert!(
        sent.starts_with(
            "The user ran the skill \"pirate-speak\". Follow its instructions below for this request.\n\nSpeak like a pirate."
        ),
        "{sent}"
    );
    assert!(
        sent.contains("you may only call these tools: read_file (plus use_skill)"),
        "{sent}"
    );
    // The task follows, with the vault's value already replaced by its name.
    assert!(
        sent.ends_with("\n\nRequest: greet the crew with ${secret:API_TOKEN}"),
        "{sent}"
    );
    // The skill was active on the first step: the write was refused, not run.
    let refused = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        refused.contains("not permitted while the skill 'pirate-speak' is active"),
        "{refused}"
    );
    assert!(!workspace.project().join("during.txt").exists());

    // An ordinary turn otherwise: the trace has what the user typed, usage and the
    // summary are kept, and the saved user message is the one the model saw.
    let trace = workspace.trace();
    assert!(
        trace.contains("\"task\": \"/pirate-speak greet the crew with ${secret:API_TOKEN}\""),
        "{trace}"
    );
    assert!(!trace.contains(SECRET));
    assert_eq!(state.usage.input_tokens, 30);
    assert!(state.last_turn.is_some());
    let saved = state
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .unwrap();
    assert_eq!(saved.content, sent);

    // The restriction ended with the turn.
    harness
        .run_turn(&mut state, "write it", |_| {})
        .await
        .unwrap();
    assert!(workspace.project().join("after.txt").exists());
}

#[tokio::test]
async fn a_skill_turn_without_a_task_says_none_was_given() {
    let server = MockServer::start(vec![sse_text("Which text?", "stop")]).await;
    let workspace = Workspace::new("no-task");
    let skill = workspace.skill("---\nname: pirate-speak\ndescription: Pirate.\n---\nArr.\n");
    let harness = harness(&server, &workspace, &skill);
    let mut state = AgentState::new();
    harness
        .run_skill_turn(&mut state, &skill, "  ", "/pirate-speak", |_| {})
        .await
        .unwrap();
    let sent = last_user(&server.requests()[0].body);
    assert!(
        sent.ends_with(
            "Arr.\n\nRequest: (none given; follow the skill's instructions, and ask the user if it needs more)"
        ),
        "{sent}"
    );
    // No tool restriction note when the skill lists no tools.
    assert!(!sent.contains("you may only call"), "{sent}");
}
