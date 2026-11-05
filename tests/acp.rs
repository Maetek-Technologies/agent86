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
                    "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\napi_key_env = \"AGENT86_TEST_MOCK_KEY\"\nmax_retries = 0\n",
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
}

/// What came back for one request: the notifications before its response, and the
/// response itself.
struct Exchange {
    updates: Vec<Value>,
    response: Value,
}

impl Exchange {
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
            if message.get("id") == Some(&json!(id)) {
                return Exchange {
                    updates,
                    response: message,
                };
            }
            assert_eq!(message["method"], "session/update", "{message}");
            assert!(message.get("id").is_none(), "{message}");
            updates.push(message);
        }
    }

    fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .response
    }

    fn new_session(&mut self, cwd: &std::path::Path) -> String {
        let exchange = self.request(
            "session/new",
            json!({"cwd": cwd.to_str().unwrap(), "mcpServers": []}),
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
    let input = format!("{INITIALIZE}\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/load\",\"params\":{{}}}}\n");
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
