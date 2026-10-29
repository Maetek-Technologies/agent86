//! `delegate` and sub-agents against Python's `agents/subagent.py` and the
//! harness's `spawn_subagent`: the sub-agent's request, its tools and depth,
//! its step budget, failures, the trace, and usage flowing into the turn.

mod support;

use agent86::agent::{Harness, CONTINUE_PROMPT};
use agent86::approval::ApprovalGate;
use agent86::config::{
    AgentsConfig, CompactionMode, EgressMode, IngressMode, LimitsConfig, ObservabilityConfig,
    RedactMode,
};
use agent86::guardrails::Guardrails;
use agent86::limits::{LimitHit, LimitKind};
use agent86::observability::Recorder;
use agent86::provider::openai::{OpenAiConfig, OpenAiProvider};
use agent86::session::AgentState;
use agent86::subagent::ROLE_PREAMBLE;
use agent86::tools::{default_registry, ToolContext};
use agent86::types::{ApprovalMode, Message, Role};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use support::{sse_text, sse_tool_calls, MockServer, Reply};

struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("agent86-delegation-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("traces")).unwrap();
        std::fs::write(root.join("README.md"), "# Demo\n").unwrap();
        std::fs::write(root.join("notes.txt"), "milk\n").unwrap();
        Self(root)
    }

    fn events(&self, session: &str) -> Vec<Value> {
        std::fs::read_to_string(self.0.join("traces").join("trace.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["session"] == session)
            .collect()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn harness(server: &MockServer, workspace: &Workspace, agents: AgentsConfig) -> Harness {
    let provider = OpenAiProvider::new(
        "test/model",
        OpenAiConfig {
            base_url: Some(server.base_url.clone()),
            require_key: false,
            max_retries: 0,
            ..OpenAiConfig::default()
        },
        None,
    )
    .unwrap();
    Harness {
        provider: std::sync::Arc::new(provider),
        router: Default::default(),
        tracer: Default::default(),
        registry: default_registry(false, false, true, false),
        context: ToolContext::new(&workspace.0).unwrap(),
        limits: LimitsConfig {
            max_steps: 40,
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
        agents,
        system: Message {
            role: Role::System,
            content: "You are a test.".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        },
        max_tokens: 1024,
        session_limits: Default::default(),
        context_window: 128_000,
        gate: ApprovalGate::new(ApprovalMode::Ask, None),
        guardrails: Guardrails {
            ingress: IngressMode::Warn,
            egress: EgressMode::Warn,
            scan_observations: true,
        },
        recorder: Recorder::from_config(&ObservabilityConfig {
            trace: true,
            path: workspace.0.join("traces").display().to_string(),
            otel: false,
            otel_exporter: Default::default(),
            otel_endpoint: None,
            redact: RedactMode::Secrets,
            max_field_chars: 2000,
            max_trace_bytes: 0,
            keep_traces: 5,
        }),
        mcp: None,
        memory: None,
        cancel: Default::default(),
    }
}

fn agents(max_depth: u32, max_steps: u32) -> AgentsConfig {
    AgentsConfig {
        enabled: true,
        max_depth,
        max_steps,
    }
}

fn tool_names(request: &support::Recorded) -> Vec<String> {
    request.body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The content of the last tool message in a request.
fn last_tool_result(request: &support::Recorded) -> String {
    request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap().to_owned())
        .unwrap()
}

#[tokio::test]
async fn a_delegated_task_runs_on_a_sub_agent_whose_answer_is_the_result() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[(
            "d",
            "delegate",
            r#"{"role": "researcher", "task": "List the files and say how many there are."}"#,
        )]),
        // The sub-agent: one tool call, then its answer.
        sse_tool_calls(&[("l", "list_dir", "{}")]),
        sse_text("There are 3 entries.", "stop"),
        sse_text("The researcher found 3 entries.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("basic");
    let harness = harness(&server, &workspace, agents(2, 8));
    let mut state = AgentState::new();
    let output = harness
        .run_turn(&mut state, "how many files?", |_| {})
        .await
        .unwrap();

    assert!(
        output.contains("[tool] delegate -> There are 3 entries.\n"),
        "{output}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 4);

    // The sub-agent's request: its persona over the parent's system content, the
    // task as the only user message, and every tool (depth 1 may still delegate).
    let sub = &requests[1].body["messages"];
    assert_eq!(
        sub[0]["content"],
        format!(
            "{}\n\nYou are a test.",
            ROLE_PREAMBLE.replace("{role}", "researcher")
        )
    );
    assert_eq!(sub[1]["role"], "user");
    assert_eq!(
        sub[1]["content"],
        "List the files and say how many there are."
    );
    assert_eq!(sub.as_array().unwrap().len(), 2);
    assert!(tool_names(&requests[1]).contains(&"delegate".to_owned()));
    assert!(last_tool_result(&requests[2]).contains("notes.txt"));

    // Its answer is the delegate call's result in the parent's history.
    assert_eq!(last_tool_result(&requests[3]), "There are 3 entries.");

    // What it spent is the turn's: two parent calls and two sub-agent calls, with
    // the sub-agent's added to the delegating step, not counted as steps.
    let turn = state.last_turn.clone().unwrap();
    assert_eq!(turn.steps, 2);
    assert_eq!(turn.tool_calls, 1);
    assert_eq!(turn.input_tokens, 20 + 20 + 10 + 10);
    assert_eq!(state.usage.input_tokens, 60);
    assert_eq!(state.steps[0].usage.input_tokens, 20 + 20 + 10);

    // The trace: the spawn, the sub-agent's model and tool calls by role and depth,
    // its completion, and the parent's delegate call.
    let spawn = workspace.events("sub");
    assert_eq!(spawn.len(), 1);
    assert_eq!(spawn[0]["kind"], "spawn");
    assert_eq!(spawn[0]["role"], "researcher");
    assert_eq!(spawn[0]["depth"], 1);
    let events = workspace.events("sub:researcher");
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["model_call", "tool_call", "model_call", "subagent_done"]
    );
    assert_eq!(events[0]["agent"], "researcher");
    assert_eq!(events[0]["depth"], 1);
    assert_eq!(events[0]["tool_calls"], serde_json::json!(["list_dir"]));
    assert_eq!(events[1]["tool"], "list_dir");
    assert_eq!(events[1]["ok"], true);
    assert_eq!(events[3]["steps"], 2);
    let parent = workspace.events(&state.session_id);
    assert!(parent.iter().any(|event| event["kind"] == "tool_call"
        && event["tool"] == "delegate"
        && event["ok"] == true));
}

#[tokio::test]
async fn a_sub_agent_answer_cut_off_at_the_output_cap_continues_or_says_so() {
    // A deliberate fix (Python returns the cut-off text as it is): a partial answer
    // continues as the main loop's does, and the pieces are joined.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"role": "reviewer", "task": "review"}"#)]),
        sse_text("Part one, ", "length"),
        sse_text("part two.", "stop"),
        sse_text("Reviewed.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("continue");
    let harness = harness(&server, &workspace, agents(2, 8));
    let mut state = AgentState::new();
    harness
        .run_turn(&mut state, "review", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    let sub = requests[2].body["messages"].as_array().unwrap();
    assert_eq!(sub[sub.len() - 2]["content"], "Part one, ");
    assert_eq!(sub[sub.len() - 1]["content"], CONTINUE_PROMPT);
    assert_eq!(last_tool_result(&requests[3]), "Part one, part two.");
    let kinds: Vec<String> = workspace
        .events("sub:reviewer")
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        ["model_call", "continuation", "model_call", "subagent_done"]
    );

    // A thinking model can spend the whole cap before writing any text: there is
    // nothing to continue, and the parent is told why the answer is missing.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"role": "reviewer", "task": "review"}"#)]),
        sse_text("", "length"),
        sse_text("No report.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("cut-off");
    let mut state = AgentState::new();
    self::harness(&server, &workspace, agents(2, 8))
        .run_turn(&mut state, "review", |_| {})
        .await
        .unwrap();
    assert_eq!(
        last_tool_result(&server.requests()[2]),
        "[sub-agent 'reviewer' wrote no answer: its last reply reached the output limit (5 tokens) before any text. Raise [limits] max_output_tokens or delegate a smaller task.]"
    );
}

#[tokio::test]
async fn delegation_stops_at_the_configured_depth() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"task": "go deeper"}"#)]),
        // The depth-1 sub-agent is at the limit: it is not offered `delegate`, and a
        // call to it anyway is refused.
        sse_tool_calls(&[("d2", "delegate", r#"{"task": "deeper still"}"#)]),
        sse_text("Could not go deeper.", "stop"),
        sse_text("Done.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("depth");
    let harness = harness(&server, &workspace, agents(1, 8));
    let mut state = AgentState::new();
    harness.run_turn(&mut state, "nest", |_| {}).await.unwrap();

    let requests = server.requests();
    assert!(tool_names(&requests[0]).contains(&"delegate".to_owned()));
    assert!(!tool_names(&requests[1]).contains(&"delegate".to_owned()));
    assert_eq!(
        last_tool_result(&requests[2]),
        "[delegation refused: max agent depth 1 reached]"
    );
    // The default role, when none is given.
    assert!(requests[1].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You are a 'assistant' sub-agent"));
}

#[tokio::test]
async fn a_sub_agent_halts_at_its_own_step_budget() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"role": "coder", "task": "loop"}"#)]),
        sse_tool_calls(&[("l", "list_dir", "{}")]),
        sse_text("Gave up.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("halt");
    let harness = harness(&server, &workspace, agents(2, 1));
    let mut state = AgentState::new();
    harness.run_turn(&mut state, "go", |_| {}).await.unwrap();
    assert_eq!(
        last_tool_result(&server.requests()[2]),
        "[sub-agent 'coder' halted: step budget reached (1 steps)]"
    );
}

#[tokio::test]
async fn bad_arguments_failures_and_disabled_delegation_are_tool_errors() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"role": "x"}"#)]),
        sse_tool_calls(&[("d", "delegate", r#"{"task": "fail"}"#)]),
        Reply::Status {
            status: 400,
            headers: Vec::new(),
            body: r#"{"error": {"message": "bad request"}}"#.into(),
        },
        sse_text("Both failed.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("errors");
    let harness = harness(&server, &workspace, agents(2, 8));
    let mut state = AgentState::new();
    harness.run_turn(&mut state, "try", |_| {}).await.unwrap();
    let requests = server.requests();
    assert_eq!(
        last_tool_result(&requests[1]),
        "Invalid arguments for 'delegate': task: Field required"
    );
    // A model call that fails inside the sub-agent fails the delegate call.
    assert!(
        last_tool_result(&requests[3]).starts_with("ProviderError: "),
        "{}",
        last_tool_result(&requests[3])
    );

    // With `[agents]` off the tool is not offered; registered anyway, it says so.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"task": "t"}"#)]),
        sse_text("ok", "stop"),
    ])
    .await;
    let off = AgentsConfig {
        enabled: false,
        ..agents(2, 8)
    };
    let harness = self::harness(&server, &workspace, off);
    harness
        .run_turn(&mut AgentState::new(), "try", |_| {})
        .await
        .unwrap();
    assert_eq!(
        last_tool_result(&server.requests()[1]),
        "Delegation is disabled."
    );
}

#[test]
fn delegate_is_registered_only_when_agents_are_on_and_never_runs_in_parallel() {
    use agent86::tools::Tool;
    assert!(!default_registry(false, false, false, false)
        .names()
        .contains(&"delegate".to_owned()));
    let schemas: Value = serde_json::from_str(&support::fixture("skills/schemas.json")).unwrap();
    let tool = agent86::tools::delegate::DelegateTool;
    assert_eq!(tool.parameters(), schemas["delegate"]);
    assert!(!tool.side_effecting());
    assert!(!tool.parallel_safe());
    let python: Value =
        serde_json::from_str(&support::fixture("skills/python_strings.json")).unwrap();
    assert_eq!(
        ROLE_PREAMBLE.replace("{role}", "x"),
        python["ROLE_PREAMBLE"]
            .as_str()
            .unwrap()
            .replace("{role}", "x")
    );
    assert_eq!(tool.description(), python["DelegateTool"]);
    assert_eq!(
        agent86::tools::skills::UseSkillTool.description(),
        python["UseSkillTool"]
    );
}

#[tokio::test]
async fn sub_agents_get_the_main_loops_output_cap() {
    // A deliberate fix: Python sends no cap, so a sub-agent gets the provider's
    // default, which `[limits] max_output_tokens` and `/limits` cannot change.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"task": "one"}"#)]),
        sse_text("First.", "stop"),
        sse_text("Done.", "stop"),
        sse_tool_calls(&[("d", "delegate", r#"{"task": "two"}"#)]),
        sse_text("Second.", "stop"),
        sse_text("Done.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("cap");
    let harness = harness(&server, &workspace, agents(2, 8));
    let mut state = AgentState::new();
    harness.run_turn(&mut state, "one", |_| {}).await.unwrap();
    // A session override reaches the sub-agents too.
    harness.session_limits.set(LimitKind::Output, Some(4096));
    harness.run_turn(&mut state, "two", |_| {}).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests[1].body["max_completion_tokens"], 1024);
    assert_eq!(requests[4].body["max_completion_tokens"], 4096);
}

#[tokio::test]
async fn a_sub_agents_spent_budget_or_empty_cap_can_be_raised() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"role": "reviewer", "task": "review"}"#)]),
        // The sub-agent's one step, then its budget is raised...
        sse_tool_calls(&[("l", "list_dir", "{}")]),
        // ...it reaches the cap before any text, and the cap is raised...
        sse_text("", "length"),
        // ...and it answers.
        sse_text("Report.", "stop"),
        sse_text("Done.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("raise");
    let mut harness = harness(&server, &workspace, agents(2, 1));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&hits);
    harness.session_limits.prompt = Some(Box::new(move |hit: &LimitHit| {
        seen.lock().unwrap().push(hit.clone());
        hit.offers.last().copied()
    }));
    let mut state = AgentState::new();
    harness.run_turn(&mut state, "go", |_| {}).await.unwrap();

    let reviewer = Some("reviewer".to_owned());
    assert_eq!(
        *hits.lock().unwrap(),
        [
            LimitHit {
                kind: LimitKind::AgentSteps,
                agent: reviewer.clone(),
                current: 1,
                offers: vec![2, 4],
            },
            LimitHit {
                kind: LimitKind::Output,
                agent: reviewer,
                current: 1024,
                offers: vec![2048, 4096],
            },
        ]
    );
    let requests = server.requests();
    assert_eq!(requests[3].body["max_completion_tokens"], 4096);
    // The empty reply was left out of the sub-agent's history.
    let sub = requests[3].body["messages"].as_array().unwrap();
    assert_eq!(sub.last().unwrap()["role"], "tool");
    assert_eq!(last_tool_result(&requests[4]), "Report.");
    let raised: Vec<String> = workspace
        .events("sub:reviewer")
        .iter()
        .filter(|event| event["kind"] == "limit_raised")
        .map(|event| format!("{} {} {}", event["limit"], event["from"], event["to"]))
        .collect();
    assert_eq!(raised, [r#""agent_steps" 1 4"#, r#""output" 1024 4096"#]);
}

#[test]
fn a_sub_agents_offer_stays_under_the_turns_budget() {
    let budgets = AgentsConfig {
        enabled: true,
        max_depth: 2,
        max_steps: 30,
    };
    let workspace = Workspace::new("ceiling");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let server = runtime.block_on(MockServer::start(Vec::new()));
    let harness = harness(&server, &workspace, budgets);
    assert_eq!(harness.agent_max_steps(), 30);
    harness.session_limits.set(LimitKind::AgentSteps, Some(100));
    assert_eq!(
        harness.agent_max_steps(),
        40,
        "never above [limits] max_steps"
    );
    harness.session_limits.set(LimitKind::Steps, Some(120));
    assert_eq!(harness.agent_max_steps(), 100);
}
