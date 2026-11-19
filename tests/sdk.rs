//! The embedding API (`agent86::sdk`, beyond Python): an agent built from a
//! provider and tools the caller writes, with no config file, home directory,
//! keyring, or network.

use agent86::agent::TurnEvent;
use agent86::provider::{DeltaStream, Provider, ProviderError};
use agent86::sdk::{Agent, BuildError, Builtins, FnTool};
use agent86::types::{
    ApprovalMode, Completion, CompletionDelta, CompletionRequest, Role, ToolCall, Usage,
};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A provider written against the public trait: it answers each request with
/// the next scripted completion and keeps every request it was sent.
#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<Completion>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl Scripted {
    fn new(replies: Vec<Completion>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::default(),
        })
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
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
        self.requests.lock().unwrap().push(request);
        let completion = self
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
        unreachable!("the embedding API runs no sub-agents")
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
    let last = requests.last().unwrap();
    last.messages
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

    // The state is the session database's `state_json`, and resumes.
    let saved = serde_json::to_string(agent.state()).unwrap();
    let mut resumed = Agent::builder()
        .provider(provider.clone())
        .workspace(&workspace.0)
        .build()
        .unwrap()
        .with_state(serde_json::from_str(&saved).unwrap());
    assert_eq!(resumed.run("three").await.unwrap().answer, "third");
    assert_eq!(resumed.state().session_id, agent.state().session_id);
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
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("yes.txt")).unwrap(),
        "hi\n"
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
