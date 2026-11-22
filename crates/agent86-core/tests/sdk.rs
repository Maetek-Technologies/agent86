//! The embedding API (`agent86_core::sdk`, beyond Python): an agent built from a
//! provider and tools the caller writes, with no config file, home directory,
//! keyring, or network.

mod support;

use agent86_core::sdk::providers::{
    Completion, CompletionDelta, CompletionRequest, DeltaStream, Provider, ProviderError,
};
use agent86_core::sdk::{
    Agent, ApprovalMode, BuildError, Builtins, Conversation, FnTool, LimitKind, McpServer, Memory,
    Model, Otel, Role, RunError, SubAgents, ToolCall, Turn, TurnEvent, Usage,
};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

/// A provider written against the public trait: it answers each request with
/// the next scripted completion and keeps every request it was sent. Copies made
/// by `fresh` (for sub-agents on threads of their own) share the script.
#[derive(Default, Clone)]
struct Scripted {
    shared: Arc<Script>,
}

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Completion>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl Scripted {
    fn new(replies: Vec<Completion>) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Script {
                replies: Mutex::new(replies.into()),
                requests: Mutex::default(),
            }),
        })
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.shared.requests.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> &str {
        "scripted-1"
    }

    fn stream(&self, request: CompletionRequest) -> DeltaStream<'_> {
        self.shared.requests.lock().unwrap().push(request);
        let completion = self
            .shared
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script has a reply for every request");
        let text = CompletionDelta {
            text: completion.text.clone(),
            ..CompletionDelta::default()
        };
        let done = CompletionDelta {
            done: true,
            completion: Some(completion),
            ..CompletionDelta::default()
        };
        Box::pin(futures_util::stream::iter(vec![Ok(text), Ok(done)]))
    }

    fn fresh(&self) -> Result<Arc<dyn Provider>, ProviderError> {
        Ok(Arc::new(self.clone()))
    }
}

fn text(answer: &str) -> Completion {
    Completion {
        text: answer.to_owned(),
        stop_reason: Some("end_turn".to_owned()),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        ..Completion::default()
    }
}

fn calls(calls: &[(&str, &str, Value)]) -> Completion {
    Completion {
        tool_calls: calls
            .iter()
            .map(|(id, name, args)| ToolCall {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                arguments: args.as_object().cloned().unwrap_or_default(),
            })
            .collect(),
        stop_reason: Some("tool_use".to_owned()),
        ..Completion::default()
    }
}

struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-sdk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn weather() -> FnTool {
    FnTool::new(
        "get_weather",
        "The current weather in a city.",
        json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        |args: &Map<String, Value>, _ctx| match args.get("city").and_then(Value::as_str) {
            Some(city) => Ok(format!("Sunny, 18C in {city}")),
            None => Err("city is required".to_owned()),
        },
    )
}

/// The content of the tool message answering `id` in the last request.
fn observation(provider: &Scripted, id: &str) -> String {
    let requests = provider.requests();
    answer_in(requests.last().unwrap(), id)
}

/// The content of the tool message answering `id` in `request`.
fn answer_in(request: &CompletionRequest, id: &str) -> String {
    request
        .messages
        .iter()
        .find(|message| message.role == Role::Tool && message.tool_call_id.as_deref() == Some(id))
        .map(|message| message.content.clone())
        .unwrap_or_else(|| panic!("no answer to {id}"))
}

#[tokio::test]
async fn a_custom_tool_and_provider_run_a_turn() {
    let workspace = Workspace::new("turn");
    let provider = Scripted::new(vec![
        calls(&[("c1", "get_weather", json!({"city": "Oslo"}))]),
        text("It is sunny in Oslo."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .system_prompt("You are a weather bot.")
        .tool(weather())
        .build()
        .unwrap();
    assert_eq!(agent.tool_names(), ["get_weather"]);

    let mut seen = Vec::new();
    let result = agent
        .run_with_events("Weather in Oslo?", |event| match event {
            TurnEvent::ToolCall { call, .. } => seen.push(format!("call {}", call.name)),
            TurnEvent::ToolResult { result, .. } => seen.push(format!("result {}", result.ok)),
            _ => {}
        })
        .await
        .unwrap();

    assert_eq!(result.answer, "It is sunny in Oslo.");
    assert_eq!(seen, ["call get_weather", "result true"]);
    assert!(observation(&provider, "c1").contains("Sunny, 18C in Oslo"));
    let requests = provider.requests();
    assert!(requests[0].messages[0]
        .content
        .starts_with("You are a weather bot."));
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(result.turn.unwrap().tool_calls, 1);
    assert_eq!(result.usage.input_tokens, 10);
}

#[tokio::test]
async fn turns_continue_the_conversation_and_state_round_trips() {
    let workspace = Workspace::new("state");
    let provider = Scripted::new(vec![text("first"), text("second"), text("third")]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .build()
        .unwrap();
    agent.run("one").await.unwrap();
    agent.run("two").await.unwrap();
    // The second request carried the first exchange.
    let contents: Vec<_> = provider.requests()[1]
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    assert!(contents.contains(&"one".to_owned()) && contents.contains(&"first".to_owned()));

    // The conversation saves as the session database's `state_json`, and resumes.
    let conversation = agent.conversation();
    let saved = conversation.to_json();
    let state: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(state["session_id"], conversation.id());
    assert_eq!(state["messages"][0]["role"], "user");
    let roles: Vec<_> = conversation.messages().iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::User, Role::Assistant]
    );
    assert_eq!(conversation.messages()[1].text, "first");
    assert_eq!(conversation.steps(), 2);
    assert_eq!(conversation.usage().input_tokens, 20);
    assert_eq!(conversation.last_turn().unwrap().steps, 1);

    let mut resumed = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .build()
        .unwrap()
        .with_conversation(Conversation::from_json(&saved).unwrap());
    assert_eq!(resumed.run("three").await.unwrap().answer, "third");
    assert_eq!(resumed.session_id(), agent.session_id());
    assert!(Conversation::from_json("not json").is_err());
    assert_ne!(Conversation::new().id(), Conversation::new().id());
}

#[tokio::test]
async fn side_effects_with_nobody_to_ask_are_declined() {
    let workspace = Workspace::new("declined");
    let provider = Scripted::new(vec![
        calls(&[(
            "c1",
            "write_file",
            json!({"path": "a.txt", "content": "hi"}),
        )]),
        text("could not write"),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::Files)
        .build()
        .unwrap();
    agent.run("write a.txt").await.unwrap();
    assert!(!workspace.0.join("a.txt").exists());
    assert_eq!(
        observation(&provider, "c1"),
        "Not executed: approval required but no prompt available."
    );
}

#[tokio::test]
async fn the_approver_decides_and_sees_the_diff() {
    let workspace = Workspace::new("approver");
    std::fs::write(workspace.0.join("yes.txt"), "old\n").unwrap();
    let provider = Scripted::new(vec![
        calls(&[
            (
                "c1",
                "write_file",
                json!({"path": "yes.txt", "content": "hi\n"}),
            ),
            (
                "c2",
                "write_file",
                json!({"path": "no.txt", "content": "hi\n"}),
            ),
        ]),
        text("done"),
    ]);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::Files)
        .approval(ApprovalMode::Ask)
        .approver(move |tool, preview| {
            let detail = preview.detail.clone().unwrap_or_default();
            log.lock().unwrap().push((tool.to_owned(), detail.clone()));
            detail.contains("yes.txt")
        })
        .build()
        .unwrap();
    agent.run("write both").await.unwrap();
    // `write_file` writes text as Python's text mode does: `\n` becomes `\r\n` on
    // Windows.
    let written = if cfg!(windows) { "hi\r\n" } else { "hi\n" };
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("yes.txt")).unwrap(),
        written
    );
    assert!(!workspace.0.join("no.txt").exists());
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    // An overwrite is shown as a unified diff, a new file as its content.
    assert_eq!(asked[0].0, "write_file");
    assert!(
        asked[0].1.contains("-old") && asked[0].1.contains("+hi"),
        "{asked:?}"
    );
    assert!(asked[1].1.starts_with("new file: no.txt"), "{asked:?}");
}

#[tokio::test]
async fn built_in_tools_stay_inside_the_workspace() {
    let workspace = Workspace::new("jail");
    std::fs::write(workspace.0.join("notes.txt"), "inside").unwrap();
    let provider = Scripted::new(vec![
        calls(&[
            ("c1", "read_file", json!({"path": "notes.txt"})),
            (
                "c2",
                "read_file",
                json!({"path": "../../../../etc/hostname"}),
            ),
        ]),
        text("ok"),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::ReadOnly)
        .build()
        .unwrap();
    assert_eq!(
        agent.tool_names(),
        ["find_files", "list_dir", "read_file", "search_files"]
    );
    agent.run("read").await.unwrap();
    assert!(observation(&provider, "c1").contains("inside"));
    let outside = observation(&provider, "c2");
    assert!(outside.contains("outside the workspace"), "{outside}");
}

#[tokio::test]
async fn a_failing_custom_tool_is_reported_to_the_model() {
    let workspace = Workspace::new("failing");
    let provider = Scripted::new(vec![
        calls(&[("c1", "get_weather", json!({}))]),
        text("sorry"),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .tool(weather())
        .build()
        .unwrap();
    agent.run("weather?").await.unwrap();
    assert!(observation(&provider, "c1").contains("city is required"));
}

#[tokio::test]
async fn the_step_limit_stops_a_runaway_turn() {
    let workspace = Workspace::new("limit");
    let provider = Scripted::new(vec![
        calls(&[("c1", "get_weather", json!({"city": "A"}))]),
        calls(&[("c2", "get_weather", json!({"city": "B"}))]),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .tool(weather())
        .max_steps(2)
        .build()
        .unwrap();
    let error = agent.run("loop").await.unwrap_err();
    assert!(error.to_string().contains("Circuit tripped"), "{error}");
    assert_eq!(provider.requests().len(), 2);
}

#[test]
fn build_refuses_what_is_missing_or_taken() {
    let workspace = Workspace::new("refusals");
    let provider = Scripted::new(Vec::new());
    assert_eq!(
        Agent::builder().workspace(&workspace.0).build().err(),
        Some(BuildError::MissingProvider)
    );
    assert_eq!(
        Agent::builder().provider(provider.clone()).build().err(),
        Some(BuildError::MissingWorkspace)
    );
    let taken = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::ReadOnly)
        .tool(FnTool::new(
            "read_file",
            "dup",
            json!({"type": "object"}),
            |_, _| Ok(String::new()),
        ))
        .build();
    assert!(matches!(taken, Err(BuildError::Tool(_))));
}

// ---- MCP ------------------------------------------------------------------

/// `python3` or `python`, or `None` (the MCP tests then skip, as `tests/mcp.rs`'s do).
fn python() -> Option<&'static str> {
    static FOUND: OnceLock<Option<&'static str>> = OnceLock::new();
    *FOUND.get_or_init(|| {
        ["python3", "python"].into_iter().find(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
    })
}

/// The stdlib-only fixture server in `mode`.
fn fixture(mode: &str) -> Option<McpServer> {
    let script = support::fixtures_dir()
        .join("mcp/server.py")
        .display()
        .to_string();
    Some(McpServer::stdio(python()?, [script, mode.to_owned()]))
}

#[tokio::test]
async fn mcp_tools_join_the_agent_and_go_through_the_gate() {
    let Some(server) = fixture("normal") else {
        return;
    };
    let workspace = Workspace::new("mcp");
    let provider = Scripted::new(vec![
        calls(&[
            ("c1", "mcp__test__add", json!({"a": 2, "b": 3})),
            ("c2", "mcp__test__echo", json!({"text": "hi"})),
        ]),
        text("five"),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .mcp_server("test", server)
        .build()
        .unwrap();
    let names = agent.tool_names();
    assert!(names.contains(&"mcp__test__add".to_owned()), "{names:?}");
    agent.run("add").await.unwrap();
    // `add` is marked read-only by the server; `echo` is not, and nobody can be asked.
    assert_eq!(observation(&provider, "c1"), "5");
    assert!(observation(&provider, "c2").starts_with("Not executed"));
    // The MCP section of the system message names the server.
    assert!(provider.requests()[0].messages[0].content.contains("test"));
}

#[tokio::test]
async fn mcp_values_are_literal() {
    let Some(server) = fixture("normal") else {
        return;
    };
    let workspace = Workspace::new("mcp-literal");
    let provider = Scripted::new(vec![
        calls(&[("c1", "mcp__test__secret", json!({}))]),
        text("ok"),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .approval(ApprovalMode::Auto)
        .mcp_server("test", server.env("MCP_TEST_TOKEN", "${HOME}"))
        .build()
        .unwrap();
    agent.run("show").await.unwrap();
    let seen = observation(&provider, "c1");
    assert!(seen.contains("token=${HOME}"), "{seen}");
}

#[test]
fn an_mcp_server_that_fails_to_start_fails_the_build() {
    let Some(server) = fixture("crash") else {
        return;
    };
    let workspace = Workspace::new("mcp-crash");
    let error = Agent::builder()
        .provider(Scripted::new(Vec::new()))
        .workspace(&workspace.0)
        .mcp_server("broken", server)
        .build()
        .err()
        .unwrap();
    let BuildError::Mcp { name, reason } = error else {
        panic!("{error:?}");
    };
    assert_eq!(name, "broken");
    assert!(reason.contains("imaginary_server"), "{reason}");

    let twice = Agent::builder()
        .provider(Scripted::new(Vec::new()))
        .workspace(&workspace.0)
        .mcp_server("same", fixture("normal").unwrap())
        .mcp_server("same", fixture("normal").unwrap())
        .build();
    assert!(matches!(twice, Err(BuildError::Mcp { .. })));
}

// ---- memory ---------------------------------------------------------------

#[tokio::test]
async fn memory_saves_resumes_and_recalls() {
    let workspace = Workspace::new("memory");
    let memory = Memory::open(workspace.0.join("memory.db")).unwrap();
    let provider = Scripted::new(vec![
        calls(&[(
            "c1",
            "remember",
            json!({"text": "The user prefers metric units."}),
        )]),
        text("Noted."),
        text("You prefer metric."),
        text("Converted."),
    ]);
    let build = || {
        Agent::builder()
            .provider(provider.clone())
            .workspace(&workspace.0)
            .memory(memory.clone())
    };

    let mut first = build().build().unwrap();
    assert!(first.tool_names().contains(&"remember".to_owned()));
    first
        .run("Remember that I prefer metric units")
        .await
        .unwrap();
    let id = first.session_id().to_owned();
    drop(first);

    // The session was saved, and resumes with its history.
    let sessions = memory.sessions(10).unwrap();
    assert_eq!(sessions[0].0, id);
    let mut resumed = build().session(&id).build().unwrap();
    resumed.run("What units do I prefer?").await.unwrap();
    let contents: Vec<_> = provider.requests()[2]
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    assert!(contents.contains(&"Noted.".to_owned()), "{contents:?}");

    // A new session gets the finished turn recalled into its system message.
    let mut fresh = build().build().unwrap();
    assert_ne!(fresh.session_id(), id);
    fresh
        .run("Remember that I prefer metric units")
        .await
        .unwrap();
    let system = &provider.requests()[3].messages[0].content;
    assert!(system.contains("Relevant past experience"), "{system}");
}

#[test]
fn a_session_needs_memory_and_must_exist() {
    let workspace = Workspace::new("memory-refusals");
    let provider = Scripted::new(Vec::new());
    assert_eq!(
        Agent::builder()
            .provider(provider.clone())
            .workspace(&workspace.0)
            .session("abc")
            .build()
            .err(),
        Some(BuildError::SessionWithoutMemory)
    );
    assert_eq!(
        Agent::builder()
            .provider(provider)
            .workspace(&workspace.0)
            .memory(Memory::in_memory().unwrap())
            .session("abc")
            .build()
            .err(),
        Some(BuildError::SessionNotFound("abc".to_owned()))
    );
}

// ---- skills ---------------------------------------------------------------

/// A skills directory holding one skill, `weather-report`, limited to
/// `get_weather`.
fn skills_dir(workspace: &Workspace) -> PathBuf {
    let dir = workspace.0.join("skills");
    let skill = dir.join("weather-report");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: weather-report\ndescription: Writes a weather report.\nallowed-tools: get_weather\n---\n\nCall get_weather for each city, then write two lines.\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn skills_are_offered_and_run_directly_under_their_allowed_tools() {
    let workspace = Workspace::new("skills");
    let provider = Scripted::new(vec![
        calls(&[
            ("c1", "get_weather", json!({"city": "Oslo"})),
            ("c2", "read_file", json!({"path": "x"})),
        ]),
        text("Oslo: sunny."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::ReadOnly)
        .tool(weather())
        .skills_dir(skills_dir(&workspace))
        .build()
        .unwrap();
    assert_eq!(agent.skills(), ["weather-report"]);
    assert!(agent.tool_names().contains(&"use_skill".to_owned()));

    let result = agent
        .run(Turn::skill("weather-report", "Oslo"))
        .await
        .unwrap();
    assert_eq!(result.answer, "Oslo: sunny.");
    let first = &provider.requests()[0];
    // The prompt lists the skill; the user's message leads with its instructions.
    assert!(first.messages[0].content.contains("weather-report"));
    let user = first
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    assert!(
        user.content.contains("Call get_weather for each city"),
        "{}",
        user.content
    );
    // Its allowed-tools hold from the first step.
    assert!(observation(&provider, "c1").contains("Sunny"));
    let refused = observation(&provider, "c2");
    assert!(
        refused.contains("not permitted while the skill"),
        "{refused}"
    );

    let unknown = agent.run(Turn::skill("nope", "x")).await.unwrap_err();
    assert_eq!(unknown, RunError::UnknownSkill("nope".to_owned()));
}

// ---- sub-agents -----------------------------------------------------------

/// A specialists directory holding `forecaster`, limited to `get_weather` and
/// running on the model named `cheap`.
fn specialists_dir(workspace: &Workspace, model: Option<&str>) -> PathBuf {
    let dir = workspace.0.join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let model = model.map(|m| format!("model: {m}\n")).unwrap_or_default();
    std::fs::write(
        dir.join("forecaster.md"),
        format!(
            "---\nname: forecaster\ndescription: Forecasts the weather.\ntools: get_weather\n{model}---\n\nYou forecast the weather, briefly.\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("broken.md"), "---\nname: other\n---\n").unwrap();
    dir
}

#[tokio::test]
async fn delegate_runs_a_sub_agent_and_hands_back_its_answer() {
    let workspace = Workspace::new("delegate");
    let provider = Scripted::new(vec![
        calls(&[("c1", "delegate", json!({"task": "Weather in Oslo?"}))]),
        calls(&[("s1", "get_weather", json!({"city": "Oslo"}))]),
        text("Sunny in Oslo."),
        text("The sub-agent says it is sunny."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .tool(weather())
        .subagents(SubAgents::new().without_built_ins())
        .build()
        .unwrap();
    assert!(agent.tool_names().contains(&"delegate".to_owned()));
    let result = agent.run("Ask a sub-agent about Oslo").await.unwrap();
    assert_eq!(result.answer, "The sub-agent says it is sunny.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    // The sub-agent got the task, and the main loop got its answer.
    assert!(requests[1]
        .messages
        .iter()
        .any(|m| m.role == Role::User && m.content.contains("Weather in Oslo?")));
    assert!(answer_in(&requests[3], "c1").contains("Sunny in Oslo."));
}

#[tokio::test(flavor = "multi_thread")]
async fn specialists_run_directly_and_only_narrow_authority() {
    let workspace = Workspace::new("specialists");
    let provider = Scripted::new(vec![
        calls(&[
            ("s1", "get_weather", json!({"city": "Oslo"})),
            ("s2", "read_file", json!({"path": "x"})),
        ]),
        text("Sunny."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::ReadOnly)
        .tool(weather())
        .subagents(SubAgents::new().specialists_dir(specialists_dir(&workspace, None)))
        .build()
        .unwrap();
    let names = agent.specialists();
    for name in ["researcher", "reviewer", "planner", "forecaster"] {
        assert!(names.contains(&name.to_owned()), "{names:?}");
    }
    // The file that does not parse was skipped, and said so.
    assert!(
        agent.notes().iter().any(|n| n.contains("broken.md")),
        "{:?}",
        agent.notes()
    );

    let result = agent
        .run(Turn::specialist("forecaster", "Oslo tomorrow?"))
        .await
        .unwrap();
    assert_eq!(result.answer, "Sunny.");
    let first = &provider.requests()[0];
    assert!(first.messages[0]
        .content
        .contains("You forecast the weather"));
    let offered: Vec<_> = first.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(offered, ["get_weather"]);
    let refused = observation(&provider, "s2");
    assert!(
        refused.contains("not permitted for the 'forecaster' sub-agent"),
        "{refused}"
    );
    // The exchange is kept as an ordinary user and assistant message.
    let last = agent.conversation().messages().pop().unwrap();
    assert_eq!((last.role, last.text.as_str()), (Role::Assistant, "Sunny."));

    let unknown = agent.run(Turn::specialist("nope", "x")).await.unwrap_err();
    assert_eq!(unknown, RunError::UnknownSpecialist("nope".to_owned()));
}

// ---- named models and triage ----------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_specialist_runs_on_its_named_model() {
    let workspace = Workspace::new("named-model");
    let main = Scripted::new(Vec::new());
    let cheap = Scripted::new(vec![text("Cheap and sunny.")]);
    let mut agent = Agent::builder()
        .provider(main.clone())
        .workspace(&workspace.0)
        .tool(weather())
        .model("cheap", Model::new(cheap.clone()).context_window(8_000))
        .subagents(
            SubAgents::new()
                .without_built_ins()
                .specialists_dir(specialists_dir(&workspace, Some("cheap"))),
        )
        .build()
        .unwrap();
    let result = agent
        .run(Turn::specialist("forecaster", "Oslo?"))
        .await
        .unwrap();
    assert_eq!(result.answer, "Cheap and sunny.");
    assert_eq!((main.requests().len(), cheap.requests().len()), (0, 1));
}

#[tokio::test]
async fn triage_sends_simple_asks_to_cheap_and_the_rest_to_frontier() {
    let workspace = Workspace::new("triage");
    let cheap = Scripted::new(vec![text("Paris."), text("Summary.")]);
    let frontier = Scripted::new(vec![text("Here is the plan.")]);
    let mut agent = Agent::builder()
        .provider(Scripted::new(Vec::new()))
        .workspace(&workspace.0)
        .model("small", Model::new(cheap.clone()))
        .model("big", Model::new(frontier.clone()))
        .triage("small", "big")
        .build()
        .unwrap();
    assert_eq!(
        agent.run("Capital of France?").await.unwrap().answer,
        "Paris."
    );
    assert_eq!(
        agent.run("Design a migration plan").await.unwrap().answer,
        "Here is the plan."
    );
    assert_eq!((cheap.requests().len(), frontier.requests().len()), (1, 1));

    let missing = Agent::builder()
        .provider(Scripted::new(Vec::new()))
        .workspace(&workspace.0)
        .model("small", Model::new(cheap.clone()))
        .triage("small", "huge")
        .build()
        .err();
    assert_eq!(missing, Some(BuildError::UnknownModel("huge".to_owned())));
}

// ---- OpenTelemetry --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn spans_reach_the_collector() {
    let workspace = Workspace::new("otel");
    let collector = support::MockServer::start(vec![support::Reply::Status {
        status: 200,
        headers: Vec::new(),
        body: "{}".into(),
    }])
    .await;
    let provider = Scripted::new(vec![
        calls(&[("c1", "get_weather", json!({"city": "Oslo"}))]),
        text("Sunny."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .tool(weather())
        .otel(Otel::otlp(format!("{}/v1/traces", collector.base_url)).header("x-team", "sdk"))
        .build()
        .unwrap();
    agent.run("Weather?").await.unwrap();
    drop(agent);
    let requests = collector.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].request_line.starts_with("POST /v1/traces "));
    assert_eq!(requests[0].header("x-team"), Some("sdk"));
    let mut names: Vec<&str> = requests[0].body["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|span| span["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["model_call", "model_call", "tool_call", "turn"]);
}

// ---- limits and secrets ---------------------------------------------------

#[tokio::test]
async fn on_limit_raises_a_reached_step_budget_and_the_turn_goes_on() {
    let workspace = Workspace::new("on-limit");
    let provider = Scripted::new(vec![
        calls(&[("c1", "get_weather", json!({"city": "A"}))]),
        calls(&[("c2", "get_weather", json!({"city": "B"}))]),
        text("Both sunny."),
    ]);
    let hits = Arc::new(Mutex::new(Vec::new()));
    let seen = hits.clone();
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .tool(weather())
        .max_steps(2)
        .on_limit(move |hit| {
            seen.lock()
                .unwrap()
                .push((hit.kind, hit.current, hit.offers.clone()));
            hit.offers.first().copied()
        })
        .build()
        .unwrap();
    let result = agent.run("Weather in A and B").await.unwrap();
    assert_eq!(result.answer, "Both sunny.");
    assert_eq!(provider.requests().len(), 3);
    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 1);
    let (kind, current, offers) = &hits[0];
    assert_eq!((*kind, *current), (LimitKind::Steps, 2));
    assert!(offers.iter().all(|offer| *offer > 2), "{offers:?}");
}

/// A command that prints the variable `name`, in the platform's shell.
fn print_variable(name: &str) -> String {
    if cfg!(windows) {
        format!("echo got %{name}%")
    } else {
        format!("echo got ${name}")
    }
}

const TOKEN: &str = "tok_9f8e7d6c5b4a3921";

#[tokio::test]
async fn a_given_secret_reaches_commands_but_never_the_model() {
    let workspace = Workspace::new("secret-given");
    let provider = Scripted::new(vec![
        calls(&[(
            "c1",
            "run_command",
            json!({"command": print_variable("API_TOKEN")}),
        )]),
        text("done"),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::All)
        .approval(ApprovalMode::Auto)
        .secret("API_TOKEN", TOKEN)
        .build()
        .unwrap();
    assert_eq!(agent.secret_names(), ["API_TOKEN"]);
    // Not offered: nobody answers a request.
    assert!(!agent.tool_names().contains(&"request_secret".to_owned()));
    agent.run("use the token").await.unwrap();
    let seen = observation(&provider, "c1");
    assert!(seen.contains("got ${secret:API_TOKEN}"), "{seen}");
    for request in provider.requests() {
        assert!(!format!("{request:?}").contains(TOKEN));
    }
    assert!(!agent.conversation().to_json().contains(TOKEN));
}

#[test]
fn a_secret_that_is_not_valid_fails_the_build_without_its_value() {
    let workspace = Workspace::new("secret-refused");
    let build = |name: &str, value: &str| {
        Agent::builder()
            .provider(Scripted::new(Vec::new()))
            .workspace(&workspace.0)
            .secret(name, value)
            .build()
            .err()
            .unwrap()
    };
    let short = build("API_TOKEN", "abc");
    assert!(matches!(&short, BuildError::Secret { name, .. } if name == "API_TOKEN"));
    assert!(!short.to_string().contains("abc"), "{short}");
    let named = build("not a name", TOKEN);
    assert!(named.to_string().contains("not a valid"), "{named}");
    assert!(!named.to_string().contains(TOKEN));
}

#[tokio::test]
async fn on_secret_request_answers_request_secret() {
    let workspace = Workspace::new("secret-asked");
    let provider = Scripted::new(vec![
        calls(&[(
            "c1",
            "request_secret",
            json!({"name": "PATH", "reason": "to change the path"}),
        )]),
        calls(&[(
            "c2",
            "request_secret",
            json!({"name": "SERVICE_KEY", "reason": "to call the service"}),
        )]),
        calls(&[(
            "c3",
            "run_command",
            json!({"command": print_variable("SERVICE_KEY")}),
        )]),
        text("done"),
    ]);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .builtins(Builtins::All)
        .approval(ApprovalMode::Auto)
        .on_secret_request(move |request| {
            log.lock()
                .unwrap()
                .push((request.name.clone(), request.reason.clone()));
            Some(TOKEN.to_owned())
        })
        .build()
        .unwrap();
    assert!(agent.tool_names().contains(&"request_secret".to_owned()));
    agent.run("call the service").await.unwrap();
    // `PATH` was refused before anyone was asked; `SERVICE_KEY` was answered.
    assert_eq!(
        *asked.lock().unwrap(),
        [("SERVICE_KEY".to_owned(), "to call the service".to_owned())]
    );
    assert_eq!(agent.secret_names(), ["SERVICE_KEY"]);
    let seen = observation(&provider, "c3");
    assert!(seen.contains("got ${secret:SERVICE_KEY}"), "{seen}");
    for request in provider.requests() {
        assert!(!format!("{request:?}").contains(TOKEN));
    }
}

// agent86UI (ADR 0005): the model draws through `render_ui` where the embedder
// declared a renderer.

#[cfg(feature = "agent86ui")]
fn form_surface(components: Value) -> Value {
    json!({"messages": [{"version": "1.0", "createSurface": {
        "surfaceId": "form",
        "catalogId": agent86_core::sdk::agent86_ui::BASIC_CATALOG_ID,
        "components": components,
        "dataModel": {"name": ""}
    }}]})
}

#[cfg(feature = "agent86ui")]
fn name_form() -> Value {
    form_surface(json!([
        {"id": "root", "component": "Column", "children": ["name", "ok"]},
        {"id": "name", "component": "TextField", "label": "Name", "value": {"@path": "/name"}},
        {"id": "ok", "component": "Button", "child": "ok_label",
         "action": {"event": {"name": "submit", "context": {"name": {"@path": "/name"}}}}},
        {"id": "ok_label", "component": "Text", "text": "OK"}
    ]))
}

#[cfg(feature = "agent86ui")]
#[tokio::test]
async fn render_ui_draws_and_each_message_reaches_the_surface() {
    use agent86_core::sdk::{AgentMessageKind, Ui};
    let workspace = Workspace::new("ui-draw");
    let provider = Scripted::new(vec![
        calls(&[("c1", "render_ui", name_form())]),
        text("I put up a form for your name."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .ui(Ui::basic())
        .build()
        .unwrap();
    assert_eq!(agent.tool_names(), ["render_ui"]);

    let mut seen = Vec::new();
    let result = agent
        .run_with_events("Ask me my name", |event| match event {
            TurnEvent::ToolCall { call, .. } => seen.push(format!("call {}", call.name)),
            TurnEvent::Ui(message) => {
                let kind = match &message.kind {
                    AgentMessageKind::CreateSurface(create) => {
                        format!(
                            "create {} ({} components)",
                            create.surface_id,
                            create.components.as_ref().map_or(0, Vec::len)
                        )
                    }
                    other => format!("{other:?}"),
                };
                seen.push(format!("ui {kind}"));
            }
            TurnEvent::ToolResult { result, .. } => seen.push(format!("result {}", result.ok)),
            _ => {}
        })
        .await
        .unwrap();
    assert_eq!(result.answer, "I put up a form for your name.");
    assert_eq!(
        seen,
        [
            "call render_ui",
            "ui create form (4 components)",
            "result true"
        ]
    );
    assert_eq!(observation(&provider, "c1"), "Rendered surface form.");

    // The prompt teaches the catalog on every request; the drawn messages stay
    // out of the conversation, whose tool call already holds them.
    let requests = provider.requests();
    let system = &requests[0].messages[0].content;
    assert!(system.contains("## agent86UI: drawing UI with render_ui"));
    assert!(system.contains("- TextField {"));
    assert!(!agent
        .conversation()
        .to_json()
        .contains("agent86ui_messages"));
}

#[cfg(feature = "agent86ui")]
#[tokio::test]
async fn an_invalid_batch_draws_nothing_and_tells_the_model_what_to_fix() {
    use agent86_core::sdk::Ui;
    let workspace = Workspace::new("ui-invalid");
    let broken = form_surface(json!([
        {"id": "root", "component": "Column", "children": ["name"]},
        {"id": "extra", "component": "Txt", "text": "hi"}
    ]));
    let provider = Scripted::new(vec![
        calls(&[("c1", "render_ui", broken)]),
        calls(&[("c2", "render_ui", name_form())]),
        text("Done."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .ui(Ui::basic())
        .build()
        .unwrap();
    let mut drawn = 0;
    let mut results = Vec::new();
    agent
        .run_with_events("Ask me my name", |event| match event {
            TurnEvent::Ui(_) => drawn += 1,
            TurnEvent::ToolResult { result, .. } => results.push(result.ok),
            _ => {}
        })
        .await
        .unwrap();
    assert_eq!(results, [false, true]);
    assert_eq!(drawn, 1, "only the corrected batch is drawn");
    let requests = provider.requests();
    let verdict = answer_in(&requests[1], "c1");
    assert!(verdict.contains("nothing was drawn"), "{verdict}");
    assert!(
        verdict.contains("Unknown component type 'Txt'"),
        "{verdict}"
    );
    // The batch failed as a whole, so the surface was never created and the
    // corrected call could create it.
    assert_eq!(answer_in(&requests[2], "c2"), "Rendered surface form.");
}

#[cfg(feature = "agent86ui")]
#[tokio::test]
async fn without_a_renderer_render_ui_is_not_offered() {
    let workspace = Workspace::new("ui-none");
    let provider = Scripted::new(vec![text("Hello.")]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .build()
        .unwrap();
    agent.run("Hi").await.unwrap();
    assert!(!agent.tool_names().contains(&"render_ui".to_owned()));
    let requests = provider.requests();
    assert!(!requests[0].messages[0].content.contains("agent86UI"));
}

#[cfg(feature = "agent86ui")]
#[tokio::test]
async fn sub_agents_never_draw() {
    use agent86_core::sdk::Ui;
    let workspace = Workspace::new("ui-subagent");
    let provider = Scripted::new(vec![
        calls(&[("c1", "delegate", json!({"task": "Draw a form"}))]),
        calls(&[("s1", "render_ui", name_form())]),
        text("I could not draw."),
        text("The sub-agent could not draw."),
    ]);
    let mut agent = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .ui(Ui::basic())
        .subagents(SubAgents::new().without_built_ins())
        .build()
        .unwrap();
    let mut drawn = 0;
    agent
        .run_with_events("Delegate a form", |event| {
            if let TurnEvent::Ui(_) = event {
                drawn += 1;
            }
        })
        .await
        .unwrap();
    let requests = provider.requests();
    // The main loop is offered the tool and its section; the sub-agent neither.
    assert!(requests[0].tools.iter().any(|t| t.name == "render_ui"));
    assert!(requests[1].tools.iter().all(|t| t.name != "render_ui"));
    assert!(!requests[1].messages[0].content.contains("agent86UI"));
    // A call it makes anyway draws nothing.
    assert_eq!(drawn, 0);
    assert!(answer_in(&requests[2], "s1").contains("No screen that can draw UI"));
}

#[cfg(feature = "agent86ui")]
#[test]
fn a_renderer_needs_a_catalog() {
    use agent86_core::sdk::Ui;
    let workspace = Workspace::new("ui-empty");
    let error = Agent::builder()
        .provider(Scripted::new(vec![]))
        .workspace(&workspace.0)
        .ui(Ui::new([]))
        .build()
        .err()
        .unwrap();
    assert!(matches!(error, BuildError::Ui(_)), "{error}");
}
