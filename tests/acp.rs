//! End-to-end tests of `agent86 acp`: the real binary, driven over piped stdio in
//! an isolated home and project directory against the local mock provider, with no
//! network or credentials.

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use support::{fixture, sse_reasoning, sse_text, sse_tool_calls, MockServer, Reply};

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// An isolated home and project; `server` is the mock model behind `mock:test/model`.
    fn new(name: &str, server: Option<&MockServer>) -> Self {
        Self::with_config(name, server, "")
    }

    /// [`Workspace::new`] with `extra` appended to the project's config.
    fn with_config(name: &str, server: Option<&MockServer>, extra: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-acp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home").join(".agent86")).unwrap();
        std::fs::create_dir_all(root.join("project").join(".agent86")).unwrap();
        // The default sentence-transformers model is not cached in the throwaway
        // home; the hash embedder keeps the sessions self-contained.
        std::fs::write(
            root.join("home").join(".agent86").join("config.toml"),
            "[memory]\nembeddings = \"hash:256\"\n",
        )
        .unwrap();
        if let Some(server) = server {
            std::fs::write(
                root.join("project").join(".agent86").join("config.toml"),
                format!(
                    "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\napi_key_env = \"AGENT86_TEST_MOCK_KEY\"\nmax_retries = 0\n\n{extra}",
                    server.base_url
                ),
            )
            .unwrap();
        }
        Self { root }
    }

    fn project(&self) -> PathBuf {
        // The canonical path: the session's jail is canonical too.
        self.root.join("project").canonicalize().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent86"));
        command
            .args(args)
            .current_dir(self.root.join("project"))
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("AGENT86_TEST_MOCK_KEY", "sk-test-0123456789abcdefghij")
            .env_remove("AGENT86_MODEL")
            .env_remove("AGENT86_SANDBOX")
            .env_remove("AGENT86_APPROVAL")
            .env("AGENT86_TRUST_PROJECT", "1")
            .env("AGENT86_NO_UPDATE_CHECK", "1")
            .env_remove("NO_COLOR")
            // No test may reach the Hugging Face hub or read the real cache.
            .env_remove("HF_HUB_CACHE")
            .env_remove("HF_HOME")
            .env_remove("HF_ENDPOINT")
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Pipe `input` in, close stdin, and collect everything.
    fn run(&self, args: &[&str], input: &str) -> Output {
        let mut child = self.command(args).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Start `agent86 acp` for a conversation: a session id comes back before the
    /// prompt that needs it, so the test has to answer line by line.
    fn connect(&self) -> Client {
        let mut child = self.command(&["acp"]).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Client {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
            choose: Some("allow-once"),
            cancel_on_update: None,
            cancel_on_request: None,
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An editor's side of the connection.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// How the user answers a permission request: the option to select, or `None`
    /// to cancel.
    choose: Option<&'static str>,
    /// Send `session/cancel` for this session when the first update arrives.
    cancel_on_update: Option<String>,
    /// Send `session/cancel` for this session when a permission request arrives,
    /// just before answering it.
    cancel_on_request: Option<String>,
}

/// What came back for one request: the notifications before its response, and the
/// response itself.
struct Exchange {
    updates: Vec<Value>,
    /// The permission requests the agent made, as received.
    requests: Vec<Value>,
    response: Value,
}

impl Exchange {
    /// The `sessionUpdate` kinds, in order.
    fn kinds(&self) -> Vec<&str> {
        self.updates
            .iter()
            .map(|message| {
                message["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap()
            })
            .collect()
    }

    /// The text of the updates of one kind, joined.
    fn text_of(&self, kind: &str) -> String {
        self.updates
            .iter()
            .filter(|message| message["params"]["update"]["sessionUpdate"] == kind)
            .map(|message| {
                message["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
}

impl Client {
    fn request(&mut self, method: &str, params: Value) -> Exchange {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut updates = Vec::new();
        let mut requests = Vec::new();
        loop {
            let mut text = String::new();
            let read = self.stdout.read_line(&mut text).unwrap();
            assert!(
                read > 0,
                "the agent closed stdout before answering {method}"
            );
            // The protocol forbids any stdout line that is not a message.
            let message: Value = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {text:?}"));
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return Exchange {
                    updates,
                    requests,
                    response: message,
                };
            }
            if message["method"] == "session/request_permission" {
                if let Some(session) = self.cancel_on_request.take() {
                    self.notify("session/cancel", json!({"sessionId": session}));
                }
                let outcome = match self.choose {
                    Some(option) => json!({"outcome": "selected", "optionId": option}),
                    None => json!({"outcome": "cancelled"}),
                };
                let reply =
                    json!({"jsonrpc": "2.0", "id": message["id"], "result": {"outcome": outcome}});
                let stdin = self.stdin.as_mut().unwrap();
                writeln!(stdin, "{reply}").unwrap();
                stdin.flush().unwrap();
                requests.push(message);
                continue;
            }
            assert_eq!(message["method"], "session/update", "{message}");
            assert!(message.get("id").is_none(), "{message}");
            updates.push(message);
            if let Some(session) = self.cancel_on_update.take() {
                self.notify("session/cancel", json!({"sessionId": session}));
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn load_session(&mut self, session: &str, cwd: &std::path::Path) -> Exchange {
        self.request(
            "session/load",
            json!({"sessionId": session, "cwd": cwd.to_str().unwrap(), "mcpServers": []}),
        )
    }

    fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .response
    }

    fn new_session(&mut self, cwd: &std::path::Path) -> String {
        self.new_session_with(cwd, json!([]))
    }

    /// `session/new` naming MCP servers, as an editor does.
    fn new_session_with(&mut self, cwd: &std::path::Path, servers: Value) -> String {
        let exchange = self.request(
            "session/new",
            json!({"cwd": cwd.to_str().unwrap(), "mcpServers": servers}),
        );
        exchange.response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("no session id: {}", exchange.response))
            .to_owned()
    }

    fn prompt(&mut self, session: &str, text: &str) -> Exchange {
        self.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    /// Close stdin, as an editor does when it quits, and wait for the agent.
    fn finish(mut self) -> Output {
        drop(self.stdin.take());
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut self.stdout, &mut rest).unwrap();
        assert_eq!(rest, "", "nothing more on stdout after the last response");
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            std::io::Read::read_to_string(&mut pipe, &mut stderr).unwrap();
        }
        let status = self.child.wait().unwrap();
        Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.into_bytes(),
        }
    }
}

/// Every stdout line, parsed: the protocol forbids any line that is not a message.
fn messages(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"))
        })
        .collect()
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

#[test]
fn the_handshake_works_and_stdout_holds_only_protocol_messages() {
    let workspace = Workspace::new("handshake", None);
    let input = format!("{INITIALIZE}\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/resume\",\"params\":{{}}}}\n");
    let output = workspace.run(&["acp"], &input);
    assert!(output.status.success(), "{output:?}");

    let replies = messages(&output);
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert_eq!(replies[0]["id"], 0);
    assert_eq!(replies[0]["result"]["protocolVersion"], 1);
    assert_eq!(replies[0]["result"]["agentInfo"]["name"], "agent86");
    // Not built yet: said plainly, not faked.
    assert_eq!(replies[1]["id"], 1);
    assert_eq!(replies[1]["error"]["code"], -32601);
}

#[test]
fn it_ends_cleanly_when_the_client_closes_stdin_without_a_word() {
    let workspace = Workspace::new("silent", None);
    let output = workspace.run(&["acp"], "");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
}

#[test]
fn garbage_input_is_answered_and_the_connection_goes_on() {
    let workspace = Workspace::new("garbage", None);
    let input = format!("not json\n{INITIALIZE}\n");
    let output = workspace.run(&["acp"], &input);
    assert!(output.status.success(), "{output:?}");
    let replies = messages(&output);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[1]["result"]["protocolVersion"], 1);
}

#[test]
fn a_broken_config_keeps_stdout_clean() {
    let workspace = Workspace::new("badconfig", None);
    std::fs::write(
        workspace
            .root
            .join("home")
            .join(".agent86")
            .join("config.toml"),
        "[model\n",
    )
    .unwrap();
    let output = workspace.run(&["acp"], &format!("{INITIALIZE}\n"));
    // The error is on stderr as for every command, and nothing reaches stdout.
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!output.stderr.is_empty());
}

#[tokio::test]
async fn a_prompt_streams_the_models_text_and_ends_the_turn() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let workspace = Workspace::new("prompt", Some(&server));
    let (output, exchange, session) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let exchange = client.prompt(&session, "Say hello");
            (client.finish(), exchange, session)
        }
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");

    assert_eq!(exchange.text_of("agent_message_chunk"), "Hello, world");
    assert_eq!(exchange.response["result"]["stopReason"], "end_turn");
    // The model saw the prompt as the user's message.
    let sent = &server.requests()[0];
    assert_eq!(
        sent.body["messages"][1],
        json!({"role": "user", "content": "Say hello"})
    );

    // It is an ordinary agent86 session, saved in the shared memory database.
    let listing = workspace.run(&["memory", "stats"], "");
    let text = String::from_utf8_lossy(&listing.stderr).into_owned()
        + &String::from_utf8_lossy(&listing.stdout);
    assert!(
        text.lines()
            .any(|line| line.contains("sessions") && line.contains('1')),
        "{text}"
    );
    assert!(!session.is_empty());
}

#[tokio::test]
async fn reasoning_is_a_thought_chunk_and_the_answer_a_message_chunk() {
    let server = MockServer::start(vec![sse_reasoning("weighing it up", "Forty-two.")]).await;
    let workspace = Workspace::new("thought", Some(&server));
    let exchange = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let exchange = client.prompt(&session, "Meaning of life?");
            client.finish();
            exchange
        }
    })
    .await
    .unwrap();
    assert_eq!(exchange.text_of("agent_thought_chunk"), "weighing it up");
    assert_eq!(exchange.text_of("agent_message_chunk"), "Forty-two.");
    assert_eq!(exchange.response["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn a_turn_that_calls_a_tool_still_finishes_and_a_session_takes_more_prompts() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("call_1", "list_dir", r#"{"path":"."}"#)]),
        sse_text("Listed it.", "stop"),
        sse_text("Second answer.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("tools", Some(&server));
    let (first, second) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let first = client.prompt(&session, "What is here?");
            let second = client.prompt(&session, "And now?");
            client.finish();
            (first, second)
        }
    })
    .await
    .unwrap();
    assert_eq!(first.text_of("agent_message_chunk"), "Listed it.");
    assert_eq!(first.response["result"]["stopReason"], "end_turn");
    // A read-only call is reported and finished, and nobody is asked.
    assert_eq!(
        first.kinds(),
        ["tool_call", "tool_call_update", "agent_message_chunk"]
    );
    assert!(first.requests.is_empty());
    let call = &first.updates[0]["params"]["update"];
    assert_eq!(call["toolCallId"], "call_1");
    assert_eq!(call["name"], "list_dir");
    assert_eq!(call["kind"], "read");
    assert_eq!(call["status"], "pending");
    assert_eq!(call["rawInput"], json!({"path": "."}));
    let done = &first.updates[1]["params"]["update"];
    assert_eq!(done["toolCallId"], "call_1");
    assert_eq!(done["status"], "completed");
    assert_eq!(second.text_of("agent_message_chunk"), "Second answer.");
    // The second turn carries the first's history to the model.
    let sent = &server.requests()[2];
    assert!(sent.body["messages"].as_array().unwrap().len() > 3);
}

#[tokio::test]
async fn a_failing_model_is_an_error_response_and_the_connection_survives() {
    let server = MockServer::start(vec![
        Reply::Status {
            status: 500,
            headers: vec![],
            body: "{\"error\":{\"message\":\"boom\"}}".into(),
        },
        sse_text("Back again.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("failing", Some(&server));
    let (failed, retried) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let failed = client.prompt(&session, "First");
            let retried = client.prompt(&session, "Second");
            client.finish();
            (failed, retried)
        }
    })
    .await
    .unwrap();
    assert_eq!(
        failed.response["error"]["code"],
        -32603,
        "{failed:?}",
        failed = failed.response
    );
    assert!(failed.response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("500"));
    assert_eq!(retried.text_of("agent_message_chunk"), "Back again.");
}

#[tokio::test]
async fn bad_session_requests_are_refused_without_running_anything() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("refused", Some(&server));
    let (unknown, relative, missing, image) = tokio::task::spawn_blocking({
        let mut client = workspace.connect();
        let gone = workspace.root.join("not-a-directory");
        move || {
            // Before the handshake, sessions are refused.
            let early = client.request("session/new", json!({"cwd": "/", "mcpServers": []}));
            assert_eq!(early.response["error"]["code"], -32600);
            client.initialize();
            let unknown = client.prompt("no-such-session", "hi");
            let relative = client.request(
                "session/new",
                json!({"cwd": "relative", "mcpServers": []}),
            );
            let missing = client.request(
                "session/new",
                json!({"cwd": gone.to_str().unwrap(), "mcpServers": []}),
            );
            let image = client.request(
                "session/prompt",
                json!({"sessionId": "x", "prompt": [{"type": "image", "data": "", "mimeType": "image/png"}]}),
            );
            client.finish();
            (unknown, relative, missing, image)
        }
    })
    .await
    .unwrap();
    for exchange in [&unknown, &relative, &missing, &image] {
        assert_eq!(
            exchange.response["error"]["code"], -32602,
            "{}",
            exchange.response
        );
    }
    assert!(unknown.response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no-such-session"));
    assert!(server.requests().is_empty());
}

const WRITE: (&str, &str, &str) = (
    "call_w",
    "write_file",
    r#"{"path":"out.txt","content":"hi\n"}"#,
);

/// One turn in which the model asks to write `out.txt`, and then says it is done;
/// `choose` is how the user answers the permission request (`None` cancels).
async fn write_turn(
    name: &str,
    config: &str,
    choose: Option<&'static str>,
) -> (Exchange, Workspace, MockServer) {
    let server = MockServer::start(vec![
        sse_tool_calls(&[WRITE]),
        sse_text("All done.", "stop"),
    ])
    .await;
    let workspace = Workspace::with_config(name, Some(&server), config);
    let exchange = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        client.choose = choose;
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let exchange = client.prompt(&session, "Write the file");
            client.finish();
            exchange
        }
    })
    .await
    .unwrap();
    (exchange, workspace, server)
}

#[tokio::test]
async fn an_approved_write_is_asked_about_then_done() {
    let (exchange, workspace, _server) = write_turn("approve", "", Some("allow-once")).await;

    // The call is reported, then asked about by id, then finished.
    assert_eq!(
        exchange.kinds(),
        ["tool_call", "tool_call_update", "agent_message_chunk"]
    );
    assert_eq!(exchange.requests.len(), 1);
    let request = &exchange.requests[0]["params"];
    assert_eq!(request["toolCall"]["toolCallId"], "call_w");
    assert_eq!(request["toolCall"]["kind"], "edit");
    // The user sees what would be written: the tool's own preview.
    let shown = request["toolCall"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(shown.contains("new file: out.txt"), "{shown}");
    assert!(shown.contains("hi"), "{shown}");
    let ids: Vec<&str> = request["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["allow-once", "reject-once"]);

    assert_eq!(
        std::fs::read_to_string(workspace.project().join("out.txt")).unwrap(),
        "hi\n"
    );
    let done = &exchange.updates[1]["params"]["update"];
    assert_eq!(done["toolCallId"], "call_w");
    assert_eq!(done["status"], "completed");
    assert_eq!(exchange.text_of("agent_message_chunk"), "All done.");
    assert_eq!(exchange.response["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn a_rejected_or_cancelled_write_does_not_happen_and_the_model_is_told() {
    for (name, choose) in [("reject", Some("reject-once")), ("cancel", None)] {
        let (exchange, workspace, server) = write_turn(name, "", choose).await;
        assert_eq!(exchange.requests.len(), 1, "{name}");
        assert!(
            !workspace.project().join("out.txt").exists(),
            "{name}: the file was written anyway"
        );
        let done = &exchange.updates[1]["params"]["update"];
        assert_eq!(done["status"], "failed", "{name}");
        // The turn went on: the model saw the refusal and answered.
        assert_eq!(
            exchange.text_of("agent_message_chunk"),
            "All done.",
            "{name}"
        );
        assert_eq!(
            exchange.response["result"]["stopReason"], "end_turn",
            "{name}"
        );
        let second = server.requests()[1].body["messages"].to_string();
        assert!(second.contains("declined by user"), "{name}: {second}");
    }
}

#[tokio::test]
async fn auto_mode_asks_nobody_and_deny_mode_refuses() {
    let (auto, workspace, _server) =
        write_turn("auto", "[guardrails]\napproval = \"auto\"\n", None).await;
    assert!(auto.requests.is_empty());
    assert!(workspace.project().join("out.txt").exists());
    assert_eq!(auto.updates[1]["params"]["update"]["status"], "completed");

    let (deny, workspace, server) = write_turn(
        "deny",
        "[guardrails]\napproval = \"deny\"\n",
        Some("allow-once"),
    )
    .await;
    assert!(deny.requests.is_empty());
    assert!(!workspace.project().join("out.txt").exists());
    assert_eq!(deny.updates[1]["params"]["update"]["status"], "failed");
    let second = server.requests()[1].body["messages"].to_string();
    assert!(second.contains("blocked by approval policy"), "{second}");
}

/// An SSE reply that says `text` and then goes quiet, as a slow model does.
fn stalling(text: &str) -> Reply {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]});
    Reply::StallAfter(format!("data: {chunk}\n\n"))
}

#[tokio::test]
async fn cancelling_stops_a_turn_that_is_streaming_and_the_session_goes_on() {
    let server = MockServer::start(vec![
        stalling("Thinking about"),
        sse_text("Fresh start.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("cancel-stream", Some(&server));
    let (cancelled, next) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            client.cancel_on_update = Some(session.clone());
            let cancelled = client.prompt(&session, "Go on and on");
            let next = client.prompt(&session, "Again");
            client.finish();
            (cancelled, next)
        }
    })
    .await
    .unwrap();
    // What streamed before the cancel arrived, then `cancelled`: not an error.
    assert!(cancelled
        .text_of("agent_message_chunk")
        .starts_with("Thinking about"));
    assert_eq!(
        cancelled.response["result"]["stopReason"], "cancelled",
        "{}",
        cancelled.response
    );
    // The next turn is not poisoned by the cancel.
    assert_eq!(next.text_of("agent_message_chunk"), "Fresh start.");
    assert_eq!(next.response["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn cancelling_while_a_permission_request_is_open_declines_it_and_ends_the_turn() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[WRITE]),
        sse_text("Should not be asked.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("cancel-permission", Some(&server));
    let exchange = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            client.cancel_on_request = Some(session.clone());
            client.choose = None;
            let exchange = client.prompt(&session, "Write the file");
            client.finish();
            exchange
        }
    })
    .await
    .unwrap();
    assert_eq!(exchange.requests.len(), 1);
    assert_eq!(
        exchange.response["result"]["stopReason"], "cancelled",
        "{}",
        exchange.response
    );
    assert!(!workspace.project().join("out.txt").exists());
    // The turn stopped there: the model was not asked again.
    assert_eq!(server.requests().len(), 1);
    // The harness closes a cancelled turn without a result event for the call it
    // never ran (the history still answers it), so the editor, which marks the
    // turn's unfinished calls cancelled itself, sees the call and the notice.
    assert_eq!(exchange.kinds()[0], "tool_call");
    assert!(!exchange
        .updates
        .iter()
        .any(|message| message["params"]["update"]["status"] == "completed"));
    assert!(exchange
        .text_of("agent_message_chunk")
        .contains("[cancelled]"));
}

#[tokio::test]
async fn a_saved_session_loads_with_its_history_replayed_and_goes_on() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[("call_1", "list_dir", r#"{"path":"."}"#)]),
        sse_text("One directory.", "stop"),
        sse_text("Same one.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("load", Some(&server));
    let project = workspace.project();

    // One process makes a session and leaves.
    let session = tokio::task::spawn_blocking({
        let project = project.clone();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            client.prompt(&session, "What is here?");
            client.finish();
            session
        }
    })
    .await
    .unwrap();

    // Another loads it.
    let (missing, loaded, next) = tokio::task::spawn_blocking({
        let session = session.clone();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let missing = client.load_session("0123456789ab", &project);
            let loaded = client.load_session(&session, &project);
            let next = client.prompt(&session, "And again?");
            client.finish();
            (missing, loaded, next)
        }
    })
    .await
    .unwrap();

    assert_eq!(
        missing.response["error"]["code"], -32002,
        "{}",
        missing.response
    );
    assert_eq!(
        loaded.kinds(),
        [
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk"
        ]
    );
    assert_eq!(loaded.response["result"], json!({}));
    assert_eq!(loaded.text_of("user_message_chunk"), "What is here?");
    assert_eq!(loaded.text_of("agent_message_chunk"), "One directory.");
    let call = &loaded.updates[1]["params"]["update"];
    assert_eq!(call["toolCallId"], "call_1");
    assert_eq!(call["name"], "list_dir");
    assert_eq!(call["kind"], "read");
    assert_eq!(
        loaded.updates[2]["params"]["update"]["toolCallId"],
        "call_1"
    );

    // A prompt in the loaded session carries the earlier conversation to the model.
    assert_eq!(next.text_of("agent_message_chunk"), "Same one.");
    let sent = server.requests()[2].body["messages"].to_string();
    assert!(sent.contains("What is here?"), "{sent}");
    assert!(sent.contains("One directory."), "{sent}");
    assert!(sent.contains("And again?"), "{sent}");
}

/// `python3` or `python`, whichever runs; the live-server tests skip without one.
fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

fn mcp_script() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp/server.py")
        .display()
        .to_string()
}

/// One turn against a session that names `servers`; the model makes `calls` (each its
/// own step) and then says "Done.". The user allows what is asked. Returns the
/// exchange, the requests the model received, and what the agent said on stderr.
async fn mcp_turn(
    name: &str,
    servers: Value,
    calls: Vec<(&'static str, &'static str, String)>,
) -> (Exchange, Vec<support::Recorded>, String) {
    let mut replies: Vec<Reply> = calls
        .iter()
        .map(|(id, tool, arguments)| sse_tool_calls(&[(id, tool, arguments.as_str())]))
        .collect();
    replies.push(sse_text("Done.", "stop"));
    let server = MockServer::start(replies).await;
    let workspace = Workspace::new(name, Some(&server));
    let (exchange, output) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session_with(&project, servers);
            let exchange = client.prompt(&session, "Use the tools");
            (exchange, client.finish())
        }
    })
    .await
    .unwrap();
    (
        exchange,
        server.requests(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn a_stdio_server_the_editor_names_is_started_and_its_tools_are_used() {
    let Some(python) = python() else { return };
    let servers = json!([{
        "name": "fx", "command": python, "args": [mcp_script(), "normal"], "env": []
    }]);
    let (exchange, requests, _) = mcp_turn(
        "mcp-stdio",
        servers,
        vec![
            ("c_add", "mcp__fx__add", r#"{"a":2,"b":3}"#.to_owned()),
            ("c_echo", "mcp__fx__echo", r#"{"text":"hello"}"#.to_owned()),
        ],
    )
    .await;

    // The model was offered the editor's server's tools.
    assert!(requests[0].body["tools"]
        .to_string()
        .contains("mcp__fx__add"));
    // The read-only one ran unasked; the other was put to the user.
    assert_eq!(exchange.requests.len(), 1);
    assert_eq!(
        exchange.requests[0]["params"]["toolCall"]["toolCallId"],
        "c_echo"
    );
    assert_eq!(exchange.requests[0]["params"]["toolCall"]["kind"], "other");
    let results: Vec<&Value> = exchange
        .updates
        .iter()
        .map(|message| &message["params"]["update"])
        .filter(|update| update["sessionUpdate"] == "tool_call_update")
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["status"], "completed");
    assert!(results[0]["content"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains('5'));
    assert!(results[1]["content"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("hello"));
    assert_eq!(exchange.response["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn values_the_editor_sends_are_literal_and_never_expanded_from_the_agents_environment() {
    let Some(python) = python() else { return };
    // The test's environment holds AGENT86_TEST_MOCK_KEY; a `${...}` reference to it
    // in a value from the editor must reach the server as the text it is.
    let servers = json!([{
        "name": "fx", "command": python, "args": [mcp_script(), "normal"],
        "env": [{"name": "MCP_TEST_TOKEN", "value": "${AGENT86_TEST_MOCK_KEY}"}]
    }]);
    let (exchange, _, _) = mcp_turn(
        "mcp-literal",
        servers,
        vec![("c1", "mcp__fx__secret", "{}".to_owned())],
    )
    .await;
    let result = exchange
        .updates
        .iter()
        .map(|message| &message["params"]["update"])
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("the call finished");
    let text = result["content"][0]["content"]["text"].as_str().unwrap();
    assert!(text.contains("token=${AGENT86_TEST_MOCK_KEY}"), "{text}");
    assert!(!text.contains("sk-test-"), "{text}");
}

/// The fixture server speaking streamable HTTP, stopped when dropped.
struct HttpServer {
    child: Child,
    port: u16,
}

impl HttpServer {
    fn start(python: &str) -> Self {
        let mut child = Command::new(python)
            .args([mcp_script().as_str(), "http"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line.trim().strip_prefix("PORT ").unwrap().parse().unwrap();
        Self { child, port }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn an_http_server_the_editor_names_gets_the_editors_headers() {
    let Some(python) = python() else { return };
    let http = HttpServer::start(python);
    let servers = json!([{
        "type": "http", "name": "web", "url": format!("http://127.0.0.1:{}/mcp", http.port),
        "headers": [{"name": "Authorization", "value": "Bearer from-the-editor"}]
    }]);
    let (exchange, _, _) = mcp_turn(
        "mcp-http",
        servers,
        vec![("c1", "mcp__web__headers", "{}".to_owned())],
    )
    .await;
    let result = exchange
        .updates
        .iter()
        .map(|message| &message["params"]["update"])
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("the call finished");
    let text = result["content"][0]["content"]["text"].as_str().unwrap();
    assert!(text.contains("Bearer from-the-editor"), "{text}");
}

#[tokio::test]
async fn a_server_that_will_not_start_does_not_stop_the_session() {
    let servers = json!([
        {"name": "broken", "command": "/definitely/not/a/program", "args": [], "env": []}
    ]);
    let (exchange, _, stderr) = mcp_turn("mcp-broken", servers, vec![]).await;
    assert_eq!(exchange.text_of("agent_message_chunk"), "Done.");
    assert_eq!(exchange.response["result"]["stopReason"], "end_turn");
    assert!(stderr.contains("MCP server 'broken'"), "{stderr}");
    assert!(stderr.contains("failed to start"), "{stderr}");
}
