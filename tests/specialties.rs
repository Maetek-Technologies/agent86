//! Specialist sub-agents (beyond Python; `docs/subagents.md`): `delegate`'s
//! `agent` argument, the filtered system prompt and tools, approval rules,
//! nesting, the specialty's own model and budget, project-file approval, attached
//! context, and skills both ways.

#[path = "../crates/agent86-core/tests/support/mod.rs"]
mod support;

use agent86::agent::Harness;
use agent86::agents_def::{parse, Source, Specialties, Specialty};
use agent86::approval::{ApprovalGate, ApprovalPreview, ApprovalPrompt};
use agent86::config::{
    AgentsConfig, CompactionMode, EgressMode, IngressMode, LimitsConfig, ObservabilityConfig,
    RedactMode,
};
use agent86::guardrails::Guardrails;
use agent86::observability::Recorder;
use agent86::provider::openai::{OpenAiConfig, OpenAiProvider};
use agent86::router::{RoutedModel, Router};
use agent86::session::AgentState;
use agent86::skills::Skill;
use agent86::subagent::{ApprovalStore, SPECIALTY_PREAMBLE};
use agent86::tools::{default_registry, ToolContext};
use agent86::types::{ApprovalMode, Message, Role};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use support::{sse_text, sse_tool_calls, MockServer};

struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("agent86-specialties-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("traces")).unwrap();
        std::fs::write(root.join("README.md"), "# Demo\n").unwrap();
        std::fs::write(root.join("notes.txt"), "milk\n").unwrap();
        Self(root)
    }

    fn events(&self, session: &str) -> Vec<Value> {
        std::fs::read_to_string(self.0.join("traces").join("trace.jsonl"))
            .unwrap_or_default()
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

fn provider(server: &MockServer, model: &str) -> OpenAiProvider {
    OpenAiProvider::new(
        model,
        OpenAiConfig {
            base_url: Some(server.base_url.clone()),
            require_key: false,
            max_retries: 0,
            ..OpenAiConfig::default()
        },
        None,
    )
    .unwrap()
}

fn harness(server: &MockServer, workspace: &Workspace, specialties: Vec<Specialty>) -> Harness {
    let mut context = ToolContext::new(&workspace.0).unwrap();
    let mut found = Specialties::default();
    for specialty in specialties {
        found
            .found
            .insert(specialty.name.clone(), Arc::new(specialty));
    }
    context.specialties = Arc::new(found);
    let mut harness = Harness {
        provider: Arc::new(provider(server, "test/model")),
        router: Default::default(),
        tracer: Default::default(),
        registry: default_registry(false, false, true, false),
        context,
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
        agents: AgentsConfig {
            enabled: true,
            max_depth: 2,
            max_steps: 8,
            max_parallel: 3,
        },
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
        gate: ApprovalGate::new(ApprovalMode::Auto, None),
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
        subagents: Default::default(),
    };
    harness.subagents.approvals = ApprovalStore {
        path: Some(workspace.0.join("approved-agents.json")),
        forced: false,
    };
    harness.context.can_delegate = true;
    harness
}

fn specialty(text: &str) -> Specialty {
    let name = text
        .lines()
        .find_map(|line| line.strip_prefix("name: "))
        .unwrap();
    parse(name, text, Source::BuiltIn).unwrap()
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

fn system(request: &support::Recorded) -> String {
    request.body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

const LISTER: &str =
    "---\nname: lister\ndescription: Lists files.\ntools: list_dir\n---\n\nList, then count.";

#[tokio::test]
async fn a_specialty_runs_with_its_own_instructions_and_only_its_tools() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[(
            "d",
            "delegate",
            r#"{"agent": "lister", "role": "ignored", "task": "How many files?"}"#,
        )]),
        sse_tool_calls(&[("l", "list_dir", "{}")]),
        sse_text("3 files.", "stop"),
        sse_text("There are 3.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("basic");
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    let mut state = AgentState::new();
    harness
        .run_turn(&mut state, "count the files", |_| {})
        .await
        .unwrap();
    let requests = server.requests();

    // The main loop's prompt lists the specialty, with its tools.
    let main = system(&requests[0]);
    assert!(
        main.contains(
            "Specialist sub-agents (call delegate with `agent` set to one of these names; each \
             runs with its own instructions and tools):\n- lister: Lists files. [tools: list_dir]"
        ),
        "{main}"
    );

    // The sub-agent: its persona and instructions over the base prompt, only its
    // tools, and the task as the first message.
    let sub = system(&requests[1]);
    let expected = format!(
        "{}\n\nList, then count.\n\nYou are a test.\n\nCurrent date and time: ",
        SPECIALTY_PREAMBLE.replace("{name}", "lister")
    );
    assert!(sub.starts_with(&expected), "{sub}");
    assert!(
        !sub.contains("Specialist sub-agents"),
        "it may not delegate"
    );
    assert_eq!(tool_names(&requests[1]), ["list_dir"]);
    assert_eq!(
        requests[1].body["messages"][1]["content"],
        "How many files?"
    );
    assert_eq!(last_tool_result(&requests[3]), "3 files.");

    // `role` carries the specialty's name in the trace, as Python's readers expect.
    let spawn = workspace.events("sub");
    assert_eq!(spawn[0]["role"], "lister");
    let events = workspace.events("sub:lister");
    assert_eq!(events[0]["agent"], "lister");
    assert_eq!(events.last().unwrap()["kind"], "subagent_done");
}

#[tokio::test]
async fn an_unknown_specialty_or_a_skill_without_one_is_refused_without_a_run() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "delegate", r#"{"agent": "nobody", "task": "t"}"#),
            ("b", "delegate", r#"{"skill": "lint", "task": "t"}"#),
        ]),
        sse_text("ok", "stop"),
    ])
    .await;
    let workspace = Workspace::new("unknown");
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    let mut state = AgentState::new();
    harness.run_turn(&mut state, "go", |_| {}).await.unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "no sub-agent ran");
    let tools: Vec<&str> = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "Unknown specialist sub-agent 'nobody'. Available: lister.",
            "The `skill` argument needs `agent`: name the specialist sub-agent that should load it.",
        ]
    );
}

#[tokio::test]
async fn a_call_outside_the_allowlist_is_refused_even_if_invented() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"agent": "lister", "task": "t"}"#)]),
        sse_tool_calls(&[("w", "write_file", r#"{"path": "x.txt", "content": "no"}"#)]),
        sse_text("could not", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("allowlist");
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    assert_eq!(
        last_tool_result(&server.requests()[2]),
        "Tool 'write_file' is not permitted for the 'lister' sub-agent. Use only the tools you \
         were given."
    );
    assert!(!workspace.0.join("x.txt").exists());
}

#[tokio::test]
async fn approval_only_narrows_and_confirm_asks_even_for_reads() {
    let strict = specialty(
        "---\nname: strict\ndescription: d\napproval: deny\nconfirm: list_dir\n---\nBe strict.",
    );
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"agent": "strict", "task": "t"}"#)]),
        sse_tool_calls(&[
            ("w", "write_file", r#"{"path": "x.txt", "content": "no"}"#),
            ("l", "list_dir", "{}"),
        ]),
        sse_text("blocked", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("approval");
    // The session is in auto mode, the specialty says deny: deny wins, and its
    // always-asked read is denied too.
    let harness = harness(&server, &workspace, vec![strict]);
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let results: Vec<String> = server.requests()[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        results,
        [
            "Not executed: blocked by approval policy (deny).",
            "Not executed: blocked by approval policy (deny).",
        ]
    );

    // A looser specialty in an ask session still asks, and says who is asking; an
    // always-asked read-only tool is put to the user.
    let loose =
        specialty("---\nname: loose\ndescription: d\napproval: auto\nconfirm: list_dir\n---\nGo.");
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"agent": "loose", "task": "t"}"#)]),
        sse_tool_calls(&[
            ("w", "write_file", r#"{"path": "x.txt", "content": "no"}"#),
            ("l", "list_dir", "{}"),
        ]),
        sse_text("asked", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let mut harness = self::harness(&server, &workspace, vec![loose]);
    let asked = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let seen = Arc::clone(&asked);
    let prompt: ApprovalPrompt = Box::new(move |tool: &str, preview: &ApprovalPreview| {
        seen.lock()
            .unwrap()
            .push((tool.to_owned(), preview.warning.clone()));
        false
    });
    harness.gate = ApprovalGate::new(ApprovalMode::Ask, Some(prompt));
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    assert_eq!(
        *asked.lock().unwrap(),
        [
            (
                "write_file".to_owned(),
                Some("Asked by the 'loose' sub-agent.".to_owned())
            ),
            (
                "list_dir".to_owned(),
                Some("The 'loose' sub-agent always asks before running list_dir.".to_owned())
            ),
        ]
    );
}

// Adjacent specialty delegations run on threads of their own while this
// runtime waits, so the mock server needs another worker; one at a time keeps
// the scripted replies in order.
#[tokio::test(flavor = "multi_thread")]
async fn no_tools_line_means_everything_but_delegate_and_nesting_narrows() {
    let open = specialty("---\nname: open\ndescription: d\n---\nAnything.");
    let nester = specialty(
        "---\nname: nester\ndescription: d\ntools: delegate, list_dir\n---\nDelegate it.",
    );
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "delegate", r#"{"agent": "open", "task": "t"}"#),
            ("b", "delegate", r#"{"agent": "nester", "task": "t"}"#),
        ]),
        // `open`: answers at once.
        sse_text("opened", "stop"),
        // `nester` hands a plain role on; the child keeps nester's narrowing.
        sse_tool_calls(&[("c", "delegate", r#"{"role": "helper", "task": "deeper"}"#)]),
        sse_text("helped", "stop"),
        sse_text("nested", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("nesting");
    let mut harness = harness(&server, &workspace, vec![open, nester]);
    harness.agents.max_parallel = 1;
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    let open_tools = tool_names(&requests[1]);
    assert!(
        !open_tools.contains(&"delegate".to_owned()),
        "{open_tools:?}"
    );
    assert!(
        open_tools.contains(&"write_file".to_owned()),
        "{open_tools:?}"
    );
    assert_eq!(tool_names(&requests[2]), ["list_dir", "delegate"]);
    // The depth-2 child: nester's list, without delegate at the last level, and a
    // filtered prompt that does not advertise what it may not use.
    assert_eq!(tool_names(&requests[3]), ["list_dir"]);
    assert!(system(&requests[3]).starts_with("You are a 'helper' sub-agent"));
    assert_eq!(last_tool_result(&requests[4]), "helped");
}

// Adjacent specialty delegations run on threads of their own while this
// runtime waits, so the mock server needs another worker; one at a time keeps
// the scripted replies in order.
#[tokio::test(flavor = "multi_thread")]
async fn a_specialty_runs_on_its_own_model_or_falls_back_with_a_notice() {
    let special = specialty(
        "---\nname: special\ndescription: d\nmodel: mock:special/model\nmax_steps: 1\n---\nBe special.",
    );
    let broken = specialty("---\nname: broken\ndescription: d\nmodel: nope:model\n---\nBe broken.");
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "delegate", r#"{"agent": "special", "task": "t"}"#),
            ("b", "delegate", r#"{"agent": "broken", "task": "t"}"#),
        ]),
        sse_tool_calls(&[("l", "list_dir", "{}")]),
        sse_text("fallback", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("model");
    let mut harness = harness(&server, &workspace, vec![special, broken]);
    harness.agents.max_parallel = 1;
    let base_url = server.base_url.clone();
    harness.router = Router::new(
        false,
        "",
        "",
        Box::new(move |reference| {
            let model = reference.strip_prefix("mock:").ok_or("unknown provider")?;
            let config = OpenAiConfig {
                base_url: Some(base_url.clone()),
                require_key: false,
                max_retries: 0,
                ..OpenAiConfig::default()
            };
            Ok(RoutedModel {
                reference: reference.to_owned(),
                provider: Arc::new(OpenAiProvider::new(model, config, None).unwrap()),
                max_tokens: 333,
                context_window: 32_000,
                output_setting: "[providers.mock] max_tokens".into(),
            })
        }),
    );
    let mut state = AgentState::new();
    let output = harness.run_turn(&mut state, "go", |_| {}).await.unwrap();
    let requests = server.requests();
    // Its own model, with that model's output cap; its one step spent, it halts.
    assert_eq!(requests[1].body["model"], "special/model");
    assert_eq!(requests[1].body["max_completion_tokens"], 333);
    // The model that cannot be built: the turn's, and the user is told.
    assert_eq!(requests[2].body["model"], "test/model");
    assert!(
        output.contains(
            "[agent] specialty 'broken' could not use its model nope:model (unknown provider); \
             it runs on test/model instead."
        ),
        "{output}"
    );
    let messages = requests[3].body["messages"].as_array().unwrap();
    let results: Vec<&str> = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        results,
        [
            "[sub-agent 'special' halted: step budget reached (1 steps)]",
            "fallback"
        ]
    );
    let route: Vec<Value> = workspace
        .events("sub:broken")
        .into_iter()
        .filter(|e| e["kind"] == "route")
        .collect();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0]["model"], "test/model");
    assert_eq!(route[0]["agent"], "broken");
    assert_eq!(route[0]["wanted"], "nope:model");
}

fn project_specialty(workspace: &Workspace, body: &str) -> Specialty {
    let dir = workspace.0.join(".agent86").join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("repo.md");
    let text = format!("---\nname: repo\ndescription: Repo text.\ntools: list_dir\n---\n{body}");
    std::fs::write(&path, &text).unwrap();
    parse("repo", &text, Source::Project(path)).unwrap()
}

#[tokio::test]
async fn a_project_specialty_runs_only_once_this_exact_file_is_approved() {
    let workspace = Workspace::new("project");
    let call = r#"{"agent": "repo", "task": "t"}"#;

    // Nobody to ask: refused, and the model saw only its name.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", call)]),
        sse_text("ok", "stop"),
    ])
    .await;
    let harness = harness(
        &server,
        &workspace,
        vec![project_specialty(&workspace, "Version one.")],
    );
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    let main = system(&requests[0]);
    assert!(
        main.contains("\n- repo: (a project file the user has not approved yet;"),
        "{main}"
    );
    assert!(!main.contains("Repo text."), "{main}");
    let refusal = last_tool_result(&requests[1]);
    assert!(
        refusal.starts_with("The project specialty 'repo' (")
            && refusal.contains("needs the user's approval, and nobody can be asked"),
        "{refusal}"
    );

    // Asked, with the whole file; a yes runs it and is remembered.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", call)]),
        sse_text("ran", "stop"),
        sse_text("ok", "stop"),
        sse_tool_calls(&[("d", "delegate", call)]),
        sse_text("ran again", "stop"),
        sse_text("ok", "stop"),
    ])
    .await;
    let mut harness = self::harness(
        &server,
        &workspace,
        vec![project_specialty(&workspace, "Version one.")],
    );
    let asked = Arc::new(Mutex::new(Vec::<ApprovalPreview>::new()));
    let seen = Arc::clone(&asked);
    harness.gate = ApprovalGate::new(
        ApprovalMode::Auto,
        Some(Box::new(move |_: &str, preview: &ApprovalPreview| {
            seen.lock().unwrap().push(preview.clone());
            true
        })),
    );
    let mut state = AgentState::new();
    harness.run_turn(&mut state, "go", |_| {}).await.unwrap();
    harness.run_turn(&mut state, "go", |_| {}).await.unwrap();
    {
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1, "asked once, in auto mode too");
        assert!(asked[0].full);
        assert!(asked[0]
            .detail
            .as_deref()
            .unwrap()
            .ends_with("Version one."));
    }
    let requests = server.requests();
    assert_eq!(last_tool_result(&requests[2]), "ran");
    assert_eq!(last_tool_result(&requests[5]), "ran again");
    assert!(system(&requests[3]).contains("\n- repo: Repo text. [tools: list_dir]"));

    // An edited file is asked about again; a no refuses it, with no fallback.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", call)]),
        sse_text("ok", "stop"),
    ])
    .await;
    let mut harness = self::harness(
        &server,
        &workspace,
        vec![project_specialty(&workspace, "Version two.")],
    );
    harness.gate = ApprovalGate::new(ApprovalMode::Auto, Some(Box::new(|_, _| false)));
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    assert!(last_tool_result(&server.requests()[1])
        .starts_with("The user declined to run the project specialty 'repo'"));

    // `--trust-project` approves it for the run, without asking or remembering.
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", call)]),
        sse_text("trusted", "stop"),
        sse_text("ok", "stop"),
    ])
    .await;
    let mut harness = self::harness(
        &server,
        &workspace,
        vec![project_specialty(&workspace, "Version three.")],
    );
    harness.subagents.approvals.forced = true;
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    assert_eq!(last_tool_result(&server.requests()[2]), "trusted");
}

#[tokio::test]
async fn files_and_a_summary_go_into_the_first_message() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[(
            "d",
            "delegate",
            r#"{"agent": "lister", "task": "Check it.", "summary": "We chose milk.", "files": ["notes.txt"]}"#,
        )]),
        sse_text("checked", "stop"),
        sse_tool_calls(&[(
            "e",
            "delegate",
            r#"{"role": "x", "task": "t", "files": ["../outside.txt"]}"#,
        )]),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("files");
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    let first = requests[1].body["messages"][1]["content"].as_str().unwrap();
    assert!(
        first.starts_with(
            "Check it.\n\n[Context from the supervising agent]\nWe chose milk.\n\n[Attached file: notes.txt]\n"
        ),
        "{first}"
    );
    assert!(first.contains("milk"), "{first}");
    // A path outside the workspace is not attached, and nothing runs.
    let refusal = last_tool_result(&requests[3]);
    assert!(
        refusal.starts_with("The file ../outside.txt could not be attached:"),
        "{refusal}"
    );
}

#[tokio::test]
async fn a_specialty_preloads_skills_and_a_skill_can_hand_off_to_one() {
    let workspace = Workspace::new("skills");
    let skill_dir = workspace.0.join("skills").join("lint");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: lint\ndescription: Lint it.\nallowed-tools: list_dir\nagent: linter\n---\nRun the linter rules.",
    )
    .unwrap();
    let skill = Arc::new(agent86::skills::load_skill(&skill_dir.join("SKILL.md")).unwrap());
    assert_eq!(skill.agent.as_deref(), Some("linter"));
    let linter = specialty(
        "---\nname: linter\ndescription: Lints.\ntools: list_dir, read_file\nskills: lint\n---\nLint.",
    );
    let server = MockServer::start(vec![
        // The main loop loads the skill; it hands off instead.
        sse_tool_calls(&[("u", "use_skill", r#"{"name": "lint"}"#)]),
        sse_tool_calls(&[("d", "delegate", r#"{"agent": "linter", "task": "lint it"}"#)]),
        // The linter has the skill preloaded; its tools are narrowed to the skill's.
        sse_text("linted", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let mut harness = self::harness(&server, &workspace, vec![linter]);
    harness.registry = default_registry(false, true, true, false);
    let mut skills = agent86::skills::Skills::new();
    skills.insert("lint".into(), Arc::clone(&skill));
    harness.context.skills = Arc::new(skills);
    harness
        .run_turn(&mut AgentState::new(), "lint the code", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    assert_eq!(
        last_tool_result(&requests[1]),
        "The skill 'lint' runs on the specialist sub-agent 'linter'. Do not follow it here: \
         call delegate with agent \"linter\", skill \"lint\", and the task as the user put it."
    );
    let sub = system(&requests[2]);
    assert!(
        sub.contains("Preloaded skill 'lint' (follow it for this task):\nRun the linter rules."),
        "{sub}"
    );
    assert_eq!(tool_names(&requests[2]), ["list_dir"]);
    // Not loaded in the main loop, so nothing there was restricted.
    assert!(harness.context.active_skill().is_none());
    let _ = Skill::new("x", "", PathBuf::new(), Vec::new());
}

#[tokio::test]
async fn slash_agent_runs_a_specialty_directly_and_keeps_the_exchange() {
    let server = MockServer::start(vec![
        // `--with-context`: the compaction summarizer first...
        sse_text("GOAL: count files.", "stop"),
        // ...then the specialty, with no main-agent call.
        sse_tool_calls(&[("l", "list_dir", "{}")]),
        sse_text("3 files.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("slash-agent");
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    let mut state = AgentState::new();
    state.messages.push(Message {
        role: Role::User,
        content: "earlier".into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    });
    let output = harness
        .run_agent_turn(
            &mut state,
            "lister",
            "How many?",
            "/agent --with-context lister How many?",
            true,
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(output, "[agent] lister is working on it\n3 files.");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(system(&requests[0]).starts_with("You are compacting"));
    assert!(system(&requests[1]).starts_with(&SPECIALTY_PREAMBLE.replace("{name}", "lister")));
    assert_eq!(
        requests[1].body["messages"][1]["content"],
        "How many?\n\n[Context from the supervising agent]\nGOAL: count files."
    );
    // The exchange is ordinary messages, so replay and resume work unchanged.
    let contents: Vec<&str> = state.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        [
            "earlier",
            "[The user ran the specialist sub-agent 'lister' with this task]\nHow many?",
            "3 files.",
        ]
    );
    assert_eq!(state.phase, agent86::types::AgentPhase::Done);
    // Every call it made, the summary included, is the session's.
    assert_eq!(state.usage.input_tokens, 10 + 20 + 10);
    assert!(state.last_turn.is_some());
    let kinds: Vec<String> = workspace
        .events(&state.session_id)
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds.first().map(String::as_str), Some("turn_start"));
    assert_eq!(kinds.last().map(String::as_str), Some("turn_end"));

    // An unknown or refused specialty is answered with no model call and nothing
    // added to the conversation.
    let before = state.messages.len();
    let output = harness
        .run_agent_turn(&mut state, "nobody", "x", "/agent nobody x", false, |_| {})
        .await
        .unwrap();
    assert_eq!(
        output,
        "[agent] Unknown specialist sub-agent 'nobody'. Available: lister.\n"
    );
    assert_eq!(state.messages.len(), before);
    assert_eq!(server.requests().len(), 3);
}

#[test]
fn slash_agent_arguments_are_checked_before_any_turn() {
    use agent86::repl::parse_agent_command;
    let specialties = Specialties::built_in();
    assert_eq!(
        parse_agent_command(&specialties, "reviewer check auth"),
        Ok(("reviewer".to_owned(), "check auth".to_owned(), false))
    );
    assert_eq!(
        parse_agent_command(&specialties, "--with-context planner  do it "),
        Ok(("planner".to_owned(), "do it".to_owned(), true))
    );
    assert!(parse_agent_command(&specialties, "")
        .unwrap_err()
        .starts_with("usage: /agent"));
    assert!(parse_agent_command(&specialties, "reviewer")
        .unwrap_err()
        .contains("needs a task"));
    assert_eq!(
        parse_agent_command(&specialties, "nobody go").unwrap_err(),
        "no specialist sub-agent named 'nobody' (known: researcher, reviewer, planner)"
    );
}

// ---------------------------------------------------------------------------
// Slice 8: parallel fan-out. Sub-agents run on threads of their own while the
// turn's runtime waits on them, so the mock server needs a multi-threaded one.

fn delayed(reply: support::Reply, millis: u64) -> support::Reply {
    support::Reply::Delayed(std::time::Duration::from_millis(millis), Box::new(reply))
}

#[tokio::test(flavor = "multi_thread")]
async fn adjacent_specialty_delegations_run_at_once_and_answer_in_call_order() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "delegate", r#"{"agent": "lister", "task": "first"}"#),
            ("b", "delegate", r#"{"agent": "lister", "task": "second"}"#),
        ]),
        delayed(sse_text("answer", "stop"), 700),
        delayed(sse_text("answer", "stop"), 700),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("fanout");
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    let started = std::time::Instant::now();
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(1300),
        "the two 700ms sub-agents should overlap: {elapsed:?}"
    );
    let requests = server.requests();
    let tasks: Vec<String> = requests[1..3]
        .iter()
        .map(|r| {
            r.body["messages"][1]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(tasks.contains(&"first".to_owned()) && tasks.contains(&"second".to_owned()));
    let answered: Vec<&Value> = requests[3].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(answered[0]["tool_call_id"], "a");
    assert_eq!(answered[1]["tool_call_id"], "b");
    // Concurrent runs of one specialty share its session, told apart by `run`.
    let runs: std::collections::BTreeSet<String> = workspace
        .events("sub:lister")
        .iter()
        .filter(|e| e["kind"] == "subagent_done")
        .map(|e| e["run"].to_string())
        .collect();
    assert_eq!(runs.len(), 2);

    // With `parallel_tools = false`, they run one after the other.
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "delegate", r#"{"agent": "lister", "task": "first"}"#),
            ("b", "delegate", r#"{"agent": "lister", "task": "second"}"#),
        ]),
        delayed(sse_text("answer", "stop"), 400),
        delayed(sse_text("answer", "stop"), 400),
        sse_text("done", "stop"),
    ])
    .await;
    let mut harness = self::harness(&server, &workspace, vec![specialty(LISTER)]);
    harness.limits.parallel_tools = false;
    let started = std::time::Instant::now();
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(800));
    assert_eq!(server.requests()[1].body["messages"][1]["content"], "first");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_listed_first_happens_before_the_delegations_after_it() {
    let reader =
        specialty("---\nname: reader\ndescription: Reads.\ntools: read_file\n---\nRead x.txt.");
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            (
                "w",
                "write_file",
                r#"{"path": "x.txt", "content": "fresh"}"#,
            ),
            ("a", "delegate", r#"{"agent": "reader", "task": "one"}"#),
            ("b", "delegate", r#"{"agent": "reader", "task": "two"}"#),
        ]),
        sse_tool_calls(&[("r", "read_file", r#"{"path": "x.txt"}"#)]),
        sse_text("read one", "stop"),
        sse_tool_calls(&[("r", "read_file", r#"{"path": "x.txt"}"#)]),
        sse_text("read two", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("fanout-order");
    let mut harness = harness(&server, &workspace, vec![reader]);
    // One at a time keeps the scripted replies in order; the grouping is the same.
    harness.agents.max_parallel = 1;
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    assert!(last_tool_result(&requests[2]).contains("fresh"));
    assert!(last_tool_result(&requests[4]).contains("fresh"));
    let results: Vec<&str> = requests[5].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(&results[1..], ["read one", "read two"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_stops_every_sibling() {
    let partial = "data: {\"choices\": [{\"index\": 0, \"delta\": {\"content\": \"Hel\"}, \"finish_reason\": null}]}\n\n";
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "delegate", r#"{"agent": "lister", "task": "one"}"#),
            ("b", "delegate", r#"{"agent": "lister", "task": "two"}"#),
        ]),
        support::Reply::StallAfter(partial.to_owned()),
        support::Reply::StallAfter(partial.to_owned()),
    ])
    .await;
    let workspace = Workspace::new("fanout-cancel");
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    let canceller = harness.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        canceller.cancel();
    });
    let mut state = AgentState::new();
    let output = harness.run_turn(&mut state, "go", |_| {}).await.unwrap();
    assert_eq!(
        output
            .matches("[tool] delegate -> [sub-agent 'lister' cancelled]")
            .count(),
        2,
        "{output}"
    );
    assert!(output.ends_with("[cancelled]\n"), "{output}");
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn siblings_ask_one_question_at_a_time() {
    let writer =
        specialty("---\nname: writer\ndescription: Writes.\ntools: write_file\n---\nWrite.");
    let write = || {
        delayed(
            sse_tool_calls(&[("w", "write_file", r#"{"path": "x.txt", "content": "x"}"#)]),
            300,
        )
    };
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            ("a", "delegate", r#"{"agent": "writer", "task": "one"}"#),
            ("b", "delegate", r#"{"agent": "writer", "task": "two"}"#),
        ]),
        write(),
        write(),
        sse_text("declined", "stop"),
        sse_text("declined", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let workspace = Workspace::new("fanout-prompts");
    let mut harness = harness(&server, &workspace, vec![writer]);
    let asking = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let most = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (now, peak, count) = (Arc::clone(&asking), Arc::clone(&most), Arc::clone(&asked));
    harness.gate = ApprovalGate::new(
        ApprovalMode::Ask,
        Some(Box::new(move |_: &str, _: &ApprovalPreview| {
            use std::sync::atomic::Ordering::SeqCst;
            let inside = now.fetch_add(1, SeqCst) + 1;
            peak.fetch_max(inside, SeqCst);
            count.fetch_add(1, SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(150));
            now.fetch_sub(1, SeqCst);
            false
        })),
    );
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    use std::sync::atomic::Ordering::SeqCst;
    assert_eq!(asked.load(SeqCst), 2);
    assert_eq!(most.load(SeqCst), 1, "two questions were open at once");
}

#[test]
fn a_fan_out_trips_on_the_turns_cost_cap() {
    let fanout = agent86::subagent::FanOut::new(0.75, 1.0);
    fanout.add(0.2);
    assert!(fanout.tripped().is_none() && !fanout.cancel.is_cancelled());
    fanout.add(0.1);
    assert_eq!(
        fanout.tripped().as_deref(),
        Some("cost cap reached ($1.0500 >= $1.0)")
    );
    assert!(fanout.cancel.is_cancelled());
    // No cap, no trip.
    let open = agent86::subagent::FanOut::new(0.0, 0.0);
    open.add(100.0);
    assert!(open.tripped().is_none());
}

#[test]
fn max_parallel_is_clamped() {
    let agents = |max_parallel| AgentsConfig {
        enabled: true,
        max_depth: 2,
        max_steps: 8,
        max_parallel,
    };
    assert_eq!(agents(0).parallel_limit(), 1);
    assert_eq!(agents(3).parallel_limit(), 3);
    assert_eq!(agents(100).parallel_limit(), 8);
}

// ---------------------------------------------------------------------------
// Slice 9: worktree isolation. These need `git` on PATH and skip without it.

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A workspace that is a git repository with one commit, or `None` without git.
fn repository(name: &str) -> Option<Workspace> {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let workspace = Workspace::new(name);
    std::fs::remove_dir_all(workspace.0.join("traces")).unwrap();
    git(&workspace.0, &["init", "-q"]);
    git(&workspace.0, &["config", "user.email", "test@example.com"]);
    git(&workspace.0, &["config", "user.name", "Test"]);
    git(&workspace.0, &["config", "commit.gpgsign", "false"]);
    git(&workspace.0, &["add", "-A"]);
    git(&workspace.0, &["commit", "-q", "-m", "base"]);
    std::fs::create_dir_all(workspace.0.join("traces")).unwrap();
    // The traces stay out of the diffs.
    std::fs::write(
        workspace.0.join(".git").join("info").join("exclude"),
        "traces/\n",
    )
    .unwrap();
    Some(workspace)
}

const ISOLATED: &str = "---\nname: writer\ndescription: Writes.\ntools: write_file, read_file\nisolation: worktree\n---\nWrite.";

#[tokio::test]
async fn an_isolated_specialty_works_in_a_worktree_and_hands_back_its_diff() {
    let Some(workspace) = repository("worktree") else {
        return;
    };
    // An uncommitted change the worktree will not have.
    std::fs::write(workspace.0.join("notes.txt"), "eggs\n").unwrap();
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"agent": "writer", "task": "write"}"#)]),
        sse_tool_calls(&[
            ("r", "read_file", r#"{"path": "notes.txt"}"#),
            (
                "w",
                "write_file",
                r#"{"path": "new.txt", "content": "hello from the worktree\n"}"#,
            ),
        ]),
        sse_text("wrote", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let harness = harness(&server, &workspace, vec![specialty(ISOLATED)]);
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    // It read HEAD's notes, not the uncommitted ones.
    let read = requests[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["tool_call_id"] == "r")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(read.contains("milk") && !read.contains("eggs"), "{read}");
    // Nothing reached the real workspace; the diff came back, and the worktree is kept.
    assert!(!workspace.0.join("new.txt").exists());
    let result = last_tool_result(&requests[3]);
    assert!(
        result.starts_with("wrote\n\n[worktree] This ran in a git worktree of HEAD ("),
        "{result}"
    );
    assert!(
        result.contains("The workspace had uncommitted changes"),
        "{result}"
    );
    assert!(result.contains("+hello from the worktree"), "{result}");
    assert!(result.contains("new.txt"), "{result}");
    let kept = result
        .split("kept at ")
        .nth(1)
        .and_then(|rest| rest.split(" on branch ").next())
        .unwrap()
        .to_owned();
    assert!(
        std::path::Path::new(&kept).join("new.txt").exists(),
        "{kept}"
    );
    // Clean up what the test made.
    git(&workspace.0, &["worktree", "remove", "--force", &kept]);
    let _ = std::fs::remove_file(format!("{kept}.kept"));
}

#[tokio::test]
async fn an_unchanged_worktree_is_removed_and_isolation_needs_a_repository_and_a_specialty() {
    let Some(workspace) = repository("worktree-clean") else {
        return;
    };
    let server = MockServer::start(vec![
        sse_tool_calls(&[
            (
                "d",
                "delegate",
                r#"{"agent": "lister", "isolate": true, "task": "look"}"#,
            ),
            (
                "e",
                "delegate",
                r#"{"role": "x", "isolate": true, "task": "look"}"#,
            ),
        ]),
        sse_text("looked", "stop"),
        sse_text("done", "stop"),
    ])
    .await;
    let harness = harness(&server, &workspace, vec![specialty(LISTER)]);
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    let requests = server.requests();
    let results: Vec<&str> = requests[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(
        results[0].ends_with("It changed nothing, and the worktree was removed."),
        "{}",
        results[0]
    );
    assert!(results[1].starts_with("The `isolate` argument needs `agent`"));
    assert_eq!(git(&workspace.0, &["worktree", "list"]).lines().count(), 1);

    // Outside a repository: refused, never run in the shared workspace.
    let plain = Workspace::new("worktree-none");
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"agent": "writer", "task": "write"}"#)]),
        sse_text("done", "stop"),
    ])
    .await;
    let harness = self::harness(&server, &plain, vec![specialty(ISOLATED)]);
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    // The temporary directory could sit inside someone's repository; then this
    // check does not apply.
    let requests = server.requests();
    let result = last_tool_result(requests.last().unwrap());
    if !result.contains("[worktree]") {
        assert!(
            result.starts_with("Isolation needs a git repository"),
            "{result}"
        );
    }
}

// ---------------------------------------------------------------------------
// The sub-agent observer, namespaced call ids, and attributed approvals.

fn observe(harness: &Harness) -> Arc<Mutex<Vec<String>>> {
    use agent86::subagent::SubagentEvent;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    harness
        .subagents
        .set_observer(Some(Arc::new(move |event: &SubagentEvent<'_>| {
            let line = match event {
                SubagentEvent::Started {
                    card, name, depth, ..
                } => format!("started {} {name} {depth} {}", card.id, card.announced),
                SubagentEvent::ToolCall { card, call } => {
                    format!("call {} {} {}", card.id, call.id, call.name)
                }
                SubagentEvent::ToolResult { card, call, result } => {
                    format!("result {} {} {}", card.id, call.id, result.ok)
                }
                SubagentEvent::Notice { card, text } => format!("notice {} {text}", card.id),
                SubagentEvent::Finished { card, name, ok, .. } => {
                    format!("finished {} {name} {ok}", card.id)
                }
            };
            log.lock().unwrap().push(line);
        })));
    seen
}

#[tokio::test]
async fn sub_agents_report_on_their_delegations_card_with_namespaced_call_ids() {
    let asking =
        specialty("---\nname: asker\ndescription: d\ntools: write_file, list_dir\n---\nWrite.");
    let server = MockServer::start(vec![
        sse_tool_calls(&[("d", "delegate", r#"{"agent": "asker", "task": "go"}"#)]),
        sse_tool_calls(&[
            ("ollama-0", "list_dir", "{}"),
            (
                "ollama-1",
                "write_file",
                r#"{"path": "x.txt", "content": "x"}"#,
            ),
        ]),
        sse_text("done", "stop"),
        sse_text("ok", "stop"),
    ])
    .await;
    let workspace = Workspace::new("observer");
    let mut harness = harness(&server, &workspace, vec![asking]);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&asked);
    harness.gate = ApprovalGate::new(ApprovalMode::Ask, None);
    harness.gate.call_prompt = Some(Box::new(
        move |call: &agent86::types::ToolCall, preview: &ApprovalPreview| {
            record
                .lock()
                .unwrap()
                .push((call.id.clone(), preview.agent.clone()));
            false
        },
    ));
    let seen = observe(&harness);
    harness
        .run_turn(&mut AgentState::new(), "go", |_| {})
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "started d asker 1 true",
            "call d d/1 list_dir",
            "result d d/1 true",
            "call d d/2 write_file",
            "result d d/2 false",
            "finished d asker true",
        ]
    );
    // The gate was asked about the call under its namespaced id, and told who asks.
    assert_eq!(
        *asked.lock().unwrap(),
        [("d/2".to_owned(), Some("asker".to_owned()))]
    );
}

#[tokio::test]
async fn slash_agent_reports_on_a_card_of_its_own_and_a_project_file_uses_the_specialty_hook() {
    let workspace = Workspace::new("observer-agent");
    let server = MockServer::start(vec![sse_text("ran", "stop")]).await;
    let mut harness = harness(
        &server,
        &workspace,
        vec![project_specialty(&workspace, "Body.")],
    );
    let hooked = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&hooked);
    harness.gate.specialty_prompt = Some(Box::new(
        move |call: &agent86::types::ToolCall, preview: &ApprovalPreview| {
            record
                .lock()
                .unwrap()
                .push((call.id.clone(), preview.agent.clone(), preview.full));
            true
        },
    ));
    // The ordinary prompts are not asked about a project file when the hook is set.
    harness.gate.prompt = Some(Box::new(|_, _| panic!("asked the wrong prompt")));
    let seen = observe(&harness);
    let mut state = AgentState::new();
    harness
        .run_agent_turn(
            &mut state,
            "repo",
            "look",
            "/agent repo look",
            false,
            |_| {},
        )
        .await
        .unwrap();
    let hooked = hooked.lock().unwrap();
    assert_eq!(hooked.len(), 1);
    let (id, agent, full) = &hooked[0];
    assert!(id.starts_with("agent-repo-"), "{id}");
    assert_eq!(agent.as_deref(), Some("repo"));
    assert!(*full);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0], format!("started {id} repo 1 false"));
    assert_eq!(seen[1], format!("finished {id} repo true"));
}
