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
    /// `AGENT86_ACP_MODEL_LISTS` for the agent: `off` unless a test asks for lists,
    /// and then only for the providers it names. Never the real providers or the
    /// real keyring.
    model_lists: &'static str,
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
        Self {
            root,
            model_lists: "off",
        }
    }

    fn project(&self) -> PathBuf {
        // The canonical path, in the plain form an editor would send: the session's
        // jail is canonical too, and on Windows `canonicalize` adds a `\\?\` prefix
        // that no editor uses.
        let path = self.root.join("project").canonicalize().unwrap();
        let text = path.display().to_string();
        match text.strip_prefix(r"\\?\") {
            Some(plain) => PathBuf::from(plain),
            None => path,
        }
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
            .env("AGENT86_ACP_MODEL_LISTS", self.model_lists)
            // The real keyring is never read by a test.
            .env("AGENT86_ACP_SECRETS", "off")
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
            buffers: Default::default(),
            refuse_writes: None,
            commands: Vec::new(),
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
    /// The files this editor holds, by absolute path, which it serves to the
    /// agent's `fs/*` requests; writes land here too.
    buffers: std::collections::HashMap<String, String>,
    /// A message to refuse every `fs/write_text_file` with.
    refuse_writes: Option<&'static str>,
    /// The commands the agent last advertised, kept out of every exchange's updates.
    commands: Vec<Value>,
}

/// What came back for one request: the notifications before its response, and the
/// response itself.
struct Exchange {
    updates: Vec<Value>,
    /// The permission requests the agent made, as received.
    requests: Vec<Value>,
    /// The `fs/*` requests the agent made, as received.
    files: Vec<Value>,
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
        self.send(&line);
        let mut updates = Vec::new();
        let mut requests = Vec::new();
        let mut files = Vec::new();
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
            self.log("A>", &message);
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return Exchange {
                    updates,
                    requests,
                    files,
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
                self.send(&reply);
                requests.push(message);
                continue;
            }
            if let Some(method) = message["method"]
                .as_str()
                .filter(|method| method.starts_with("fs/"))
            {
                let reply = self.serve_file(method, &message);
                self.send(&reply);
                files.push(message);
                continue;
            }
            assert_eq!(message["method"], "session/update", "{message}");
            assert!(message.get("id").is_none(), "{message}");
            if message["params"]["update"]["sessionUpdate"] == "available_commands_update" {
                self.commands = message["params"]["update"]["availableCommands"]
                    .as_array()
                    .unwrap()
                    .clone();
                continue;
            }
            updates.push(message);
            if let Some(session) = self.cancel_on_update.take() {
                self.notify("session/cancel", json!({"sessionId": session}));
            }
        }
    }

    /// Answer an `fs/*` request the way an editor does.
    fn serve_file(&mut self, method: &str, request: &Value) -> Value {
        let id = &request["id"];
        let path = request["params"]["path"].as_str().unwrap().to_owned();
        assert!(
            std::path::Path::new(&path).is_absolute(),
            "paths are absolute: {path}"
        );
        let error = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        match method {
            "fs/read_text_file" => match self.buffers.get(&path) {
                Some(content) => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"content": content}})
                }
                None => error(-32002, "no such file in the editor"),
            },
            "fs/write_text_file" => {
                if let Some(message) = self.refuse_writes {
                    return error(-32603, message);
                }
                let content = request["params"]["content"].as_str().unwrap().to_owned();
                self.buffers.insert(path, content);
                json!({"jsonrpc": "2.0", "id": id, "result": {}})
            }
            other => error(-32601, &format!("unknown method {other}")),
        }
    }

    /// Write one message to the agent, and to the transcript if one is being kept.
    fn send(&mut self, message: &Value) {
        self.log("C>", message);
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// With `AGENT86_ACP_TRANSCRIPT` set to a path, append every message exchanged,
    /// tagged with the agent's process id and the direction (`C>` client to agent,
    /// `A>` agent to client), for checking against the protocol's schema.
    fn log(&self, direction: &str, message: &Value) {
        let Some(path) = std::env::var_os("AGENT86_ACP_TRANSCRIPT") else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            // One write per line, so tests running in parallel do not interleave.
            let _ =
                file.write_all(format!("{} {direction} {message}\n", self.child.id()).as_bytes());
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send(&line);
    }

    fn load_session(&mut self, session: &str, cwd: &std::path::Path) -> Exchange {
        self.request(
            "session/load",
            json!({"sessionId": session, "cwd": cwd.to_str().unwrap(), "mcpServers": []}),
        )
    }

    fn initialize(&mut self) -> Value {
        self.initialize_with(json!({}))
    }

    /// `initialize` advertising `capabilities`, e.g. `{"fs": {"readTextFile": true}}`.
    fn initialize_with(&mut self, capabilities: Value) -> Value {
        self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": capabilities}),
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
        // Only notifications may still be on their way, such as the command list of a
        // thread opened last; anything else is a message nobody asked for.
        for line in rest.lines() {
            let message: Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"));
            assert_eq!(message["method"], "session/update", "{message}");
            assert!(message.get("id").is_none(), "{message}");
        }
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

    assert_eq!(disk_text(&workspace.project().join("out.txt")), "hi\n");
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
    // Nothing but the settings the editor's pickers show.
    assert_eq!(
        loaded.response["result"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["configOptions"]
    );
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

const BOTH_FILE_METHODS: fn() -> Value =
    || json!({"fs": {"readTextFile": true, "writeTextFile": true}});

/// One turn in which the model makes `calls` (each its own step) and says "Done.",
/// against an editor advertising `capabilities` and holding `buffers` (relative
/// paths in the project). `refuse_writes` is how the editor answers a write.
async fn editor_turn(
    name: &str,
    capabilities: Value,
    buffers: &[(&str, &str)],
    disk: &[(&str, &str)],
    refuse_writes: Option<&'static str>,
    calls: Vec<(&'static str, &'static str, &'static str)>,
) -> (
    Exchange,
    Workspace,
    std::collections::HashMap<String, String>,
    MockServer,
) {
    let mut replies: Vec<Reply> = calls.iter().map(|call| sse_tool_calls(&[*call])).collect();
    replies.push(sse_text("Done.", "stop"));
    let server = MockServer::start(replies).await;
    let workspace = Workspace::new(name, Some(&server));
    for (path, content) in disk {
        std::fs::write(workspace.project().join(path), content).unwrap();
    }
    let (exchange, held) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let buffers: Vec<(String, String)> = buffers
            .iter()
            .map(|(path, text)| (project.join(path).display().to_string(), (*text).to_owned()))
            .collect();
        let mut client = workspace.connect();
        move || {
            client.buffers.extend(buffers);
            client.refuse_writes = refuse_writes;
            client.initialize_with(capabilities);
            let session = client.new_session(&project);
            let exchange = client.prompt(&session, "Work on the files");
            let held = client.buffers.clone();
            client.finish();
            (exchange, held)
        }
    })
    .await
    .unwrap();
    (exchange, workspace, held, server)
}

fn tool_result_text(exchange: &Exchange) -> String {
    exchange
        .updates
        .iter()
        .map(|message| &message["params"]["update"])
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("a call finished")["content"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn reads_come_from_the_editors_buffer_not_the_disk() {
    let (exchange, _workspace, _, _) = editor_turn(
        "fs-read",
        BOTH_FILE_METHODS(),
        &[("notes.txt", "unsaved changes\n")],
        &[("notes.txt", "saved\n")],
        None,
        vec![("c1", "read_file", r#"{"path":"notes.txt"}"#)],
    )
    .await;
    assert_eq!(tool_result_text(&exchange), "unsaved changes\n");
    assert_eq!(exchange.files.len(), 1);
    let request = &exchange.files[0];
    assert_eq!(request["method"], "fs/read_text_file");
    assert_eq!(request["params"]["sessionId"].as_str().unwrap().len(), 12);
    assert!(request["params"]["path"]
        .as_str()
        .unwrap()
        .ends_with("notes.txt"));
    assert_eq!(exchange.response["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn a_file_the_editor_does_not_have_is_read_from_the_disk() {
    let (exchange, _workspace, _, _) = editor_turn(
        "fs-read-disk",
        BOTH_FILE_METHODS(),
        &[],
        &[("only-on-disk.txt", "from the disk\n")],
        None,
        vec![("c1", "read_file", r#"{"path":"only-on-disk.txt"}"#)],
    )
    .await;
    // Asked, answered with an error, and the disk read instead.
    assert_eq!(exchange.files.len(), 1);
    assert_eq!(tool_result_text(&exchange), "from the disk\n");
}

#[tokio::test]
async fn writes_land_in_the_editor_and_the_approval_shows_the_change_from_its_buffer() {
    let (exchange, workspace, held, _) = editor_turn(
        "fs-write",
        BOTH_FILE_METHODS(),
        &[("notes.txt", "unsaved\n")],
        &[("notes.txt", "saved\n")],
        None,
        vec![(
            "c1",
            "write_file",
            r#"{"path":"notes.txt","content":"replaced\n"}"#,
        )],
    )
    .await;
    // The user was shown the change from what the editor holds.
    let shown = exchange.requests[0]["params"]["toolCall"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(
        shown.contains("-unsaved") && shown.contains("+replaced"),
        "{shown}"
    );
    // The write went to the editor, not behind its back to the disk.
    let writes: Vec<&Value> = exchange
        .files
        .iter()
        .filter(|request| request["method"] == "fs/write_text_file")
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["params"]["content"], "replaced\n");
    let path = workspace.project().join("notes.txt").display().to_string();
    assert_eq!(held[&path], "replaced\n");
    assert_eq!(disk_text(&workspace.project().join("notes.txt")), "saved\n");
    assert!(tool_result_text(&exchange).contains("Wrote"));
}

#[tokio::test]
async fn edit_file_works_on_the_editors_buffer() {
    let (exchange, workspace, held, _) = editor_turn(
        "fs-edit",
        BOTH_FILE_METHODS(),
        &[("a.rs", "fn one() {}\nfn two() {}\n")],
        &[("a.rs", "stale\n")],
        None,
        vec![(
            "c1",
            "edit_file",
            r#"{"path":"a.rs","old_string":"two","new_string":"deux"}"#,
        )],
    )
    .await;
    let path = workspace.project().join("a.rs").display().to_string();
    assert_eq!(held[&path], "fn one() {}\nfn deux() {}\n");
    assert!(
        tool_result_text(&exchange).contains("Edited"),
        "{}",
        tool_result_text(&exchange)
    );
}

#[tokio::test]
async fn a_write_the_editor_refuses_fails_the_call_and_does_not_touch_the_disk() {
    let (exchange, workspace, _, _) = editor_turn(
        "fs-refuse",
        BOTH_FILE_METHODS(),
        &[("notes.txt", "unsaved\n")],
        &[("notes.txt", "saved\n")],
        Some("the buffer is read-only"),
        vec![(
            "c1",
            "write_file",
            r#"{"path":"notes.txt","content":"replaced\n"}"#,
        )],
    )
    .await;
    let done = exchange
        .updates
        .iter()
        .map(|message| &message["params"]["update"])
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .unwrap();
    assert_eq!(done["status"], "failed");
    assert!(tool_result_text(&exchange).contains("the buffer is read-only"));
    assert_eq!(disk_text(&workspace.project().join("notes.txt")), "saved\n");
}

#[tokio::test]
async fn only_what_the_editor_advertises_is_used() {
    // Reads offered, writes not: the read comes from the editor, the write goes to
    // the disk, and the editor is never sent a method it did not advertise.
    let (exchange, workspace, held, _) = editor_turn(
        "fs-read-only",
        json!({"fs": {"readTextFile": true}}),
        &[("notes.txt", "unsaved\n")],
        &[("notes.txt", "saved\n")],
        None,
        vec![(
            "c1",
            "write_file",
            r#"{"path":"notes.txt","content":"replaced\n"}"#,
        )],
    )
    .await;
    assert!(exchange
        .files
        .iter()
        .all(|request| request["method"] == "fs/read_text_file"));
    assert!(!exchange.files.is_empty());
    assert_eq!(
        disk_text(&workspace.project().join("notes.txt")),
        "replaced\n"
    );
    assert_eq!(held.values().cloned().collect::<Vec<_>>(), ["unsaved\n"]);

    // Nothing advertised: no `fs/*` request at all, as in every earlier test.
    let (exchange, _workspace, _, _) = editor_turn(
        "fs-none",
        json!({}),
        &[("notes.txt", "unsaved\n")],
        &[("notes.txt", "saved\n")],
        None,
        vec![("c1", "read_file", r#"{"path":"notes.txt"}"#)],
    )
    .await;
    assert!(exchange.files.is_empty());
    assert_eq!(tool_result_text(&exchange), "saved\n");
}

/// A file's text with Windows line endings normalised: the write tools translate `\n`
/// to `\r\n` there, as Python does.
fn disk_text(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

fn command_names(client: &Client) -> Vec<&str> {
    client
        .commands
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn the_commands_are_advertised_and_run_without_the_model() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("commands", Some(&server));
    let (names, hints, mode, bad, cost, tools, model_help, bad_model) =
        tokio::task::spawn_blocking({
            let project = workspace.project();
            let mut client = workspace.connect();
            move || {
                client.initialize();
                let session = client.new_session(&project);
                // The list arrives after the response, so the next read picks it up.
                let mode = client.prompt(&session, "/mode");
                let names: Vec<String> = command_names(&client)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let hints: Vec<Option<String>> = client
                    .commands
                    .iter()
                    .map(|c| c["input"]["hint"].as_str().map(str::to_owned))
                    .collect();
                let bad = client.prompt(&session, "/mode sideways");
                let cost = client.prompt(&session, "/cost");
                let tools = client.prompt(&session, "/tools");
                let model_help = client.prompt(&session, "/model");
                let bad_model = client.prompt(&session, "/model nonsense");
                client.finish();
                (names, hints, mode, bad, cost, tools, model_help, bad_model)
            }
        })
        .await
        .unwrap();
    assert_eq!(
        names,
        ["mode", "model", "thinking", "toolview", "resume", "secret", "cost", "tools"]
    );
    assert_eq!(hints.len(), 8);
    assert_eq!(hints[0].as_deref(), Some("ask, auto, or deny"));
    assert_eq!(hints[1].as_deref(), Some("provider:model"));
    assert_eq!(hints[2].as_deref(), Some("full, summary, or hidden"));
    assert_eq!(hints[3], hints[2]);
    assert!(hints[4].is_some());
    assert_eq!(hints[5].as_deref(), Some("forget NAME"));
    assert_eq!((&hints[6], &hints[7]), (&None, &None));
    for exchange in [&mode, &bad, &cost, &tools, &model_help, &bad_model] {
        assert_eq!(exchange.response["result"]["stopReason"], "end_turn");
        // The answer, then the settings as they now stand (a command may change one).
        assert_eq!(
            exchange.kinds(),
            ["agent_message_chunk", "config_option_update"]
        );
    }
    assert_eq!(mode.text_of("agent_message_chunk"), "approval mode: ask");
    assert!(bad
        .text_of("agent_message_chunk")
        .contains("unknown mode 'sideways'"));
    assert!(cost
        .text_of("agent_message_chunk")
        .starts_with("steps 0  in 0  out 0 tok"));
    assert!(tools.text_of("agent_message_chunk").contains("read_file"));
    let shown = model_help.text_of("agent_message_chunk");
    assert!(
        shown.contains("current model: ") && shown.contains(":test/model"),
        "{shown}"
    );
    // A refused switch leaves things as they were, and none of it reached the model.
    assert!(!bad_model.text_of("agent_message_chunk").is_empty());
    assert!(server.requests().is_empty());
}

/// `/cost` prices the session's usage under the ref the user wrote. The mock is an
/// OpenAI-compatible section, whose adapter calls itself `openai`, so a ref read back
/// from the provider (`openai:test/model`) would find no price.
#[tokio::test]
async fn cost_prices_an_openai_compatible_model_under_its_configured_ref() {
    let server = MockServer::start(vec![sse_text("Hello.", "stop")]).await;
    let workspace = Workspace::with_config(
        "cost-priced",
        Some(&server),
        "[pricing.models.\"mock:test/model\"]\ninput_per_mtok = 1000.0\noutput_per_mtok = 2000.0\n",
    );
    let cost = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            client.prompt(&session, "hello");
            let cost = client.prompt(&session, "/cost");
            client.finish();
            cost
        }
    })
    .await
    .unwrap();
    let shown = cost.text_of("agent_message_chunk");
    assert!(shown.contains("in 10  out 5 tok"), "{shown}");
    assert!(shown.contains("cost $"), "{shown}");
    assert!(!shown.contains("unpriced"), "{shown}");
}

#[tokio::test]
async fn setting_the_mode_changes_what_is_asked_and_unknown_slashes_reach_the_model() {
    let server = MockServer::start(vec![
        sse_tool_calls(&[WRITE]),
        sse_text("Written.", "stop"),
        sse_text("The model saw it.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("commands-mode", Some(&server));
    let (set, write, other) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let set = client.prompt(&session, "/mode auto");
            let write = client.prompt(&session, "Write the file");
            let other = client.prompt(&session, "/frobnicate now");
            client.finish();
            (set, write, other)
        }
    })
    .await
    .unwrap();
    assert_eq!(set.text_of("agent_message_chunk"), "approval mode: auto");
    // In `auto` nothing is asked, so the write just happened.
    assert!(write.requests.is_empty());
    assert!(workspace.project().join("out.txt").exists());
    // A slash the agent does not own is an ordinary prompt.
    assert_eq!(other.text_of("agent_message_chunk"), "The model saw it.");
    let sent = server.requests().last().unwrap().body["messages"].to_string();
    assert!(sent.contains("/frobnicate now"), "{sent}");
}

/// The option with this id from a `configOptions` array.
fn option<'a>(options: &'a Value, id: &str) -> &'a Value {
    options
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == id)
        .unwrap_or_else(|| panic!("no option {id} in {options}"))
}

fn choice_values(option: &Value) -> Vec<&str> {
    option["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|choice| choice["value"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn the_editors_pickers_get_the_mode_and_the_models_and_can_change_them() {
    let server = MockServer::start(vec![sse_text("From the cheap model.", "stop")]).await;
    let workspace = Workspace::with_config(
        "config-options",
        Some(&server),
        "[model.route]\ncheap = \"mock:cheap/model\"\nfrontier = \"mock:big/model\"\n",
    );
    let (created, set_model, set_mode, bad_model, bad_mode, answer, after_command) =
        tokio::task::spawn_blocking({
            let project = workspace.project();
            let mut client = workspace.connect();
            move || {
                client.initialize();
                let created = client.request(
                    "session/new",
                    json!({"cwd": project.to_str().unwrap(), "mcpServers": []}),
                );
                let session = created.response["result"]["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let set = |client: &mut Client, id: &str, value: Value| {
                    client.request(
                        "session/set_config_option",
                        json!({"sessionId": session, "configId": id, "value": value}),
                    )
                };
                let set_model = set(&mut client, "model", json!("mock:cheap/model"));
                let set_mode = set(&mut client, "mode", json!("deny"));
                let bad_model = set(&mut client, "model", json!("mock:unlisted/model"));
                let bad_mode = set(&mut client, "mode", json!("sideways"));
                let answer = client.prompt(&session, "Which model?");
                // `/mode` from the prompt box moves the picker too.
                let after_command = client.prompt(&session, "/mode auto");
                client.finish();
                (
                    created,
                    set_model,
                    set_mode,
                    bad_model,
                    bad_mode,
                    answer,
                    after_command,
                )
            }
        })
        .await
        .unwrap();

    let options = &created.response["result"]["configOptions"];
    let mode = option(options, "mode");
    assert_eq!(mode["category"], "mode");
    assert_eq!(mode["type"], "select");
    assert_eq!(mode["currentValue"], "ask");
    assert_eq!(choice_values(mode), ["ask", "auto", "deny"]);
    let model = option(options, "model");
    assert_eq!(model["category"], "model");
    let models = choice_values(model);
    // The config's own models, each once, and the one in use among them.
    assert_eq!(models[0], "mock:test/model");
    assert!(models.contains(&"mock:cheap/model") && models.contains(&"mock:big/model"));
    assert!(models.contains(&model["currentValue"].as_str().unwrap()));
    assert_eq!(model["currentValue"], "mock:test/model");

    // Picking a listed model switches the session to it, and answers with all options.
    let options = &set_model.response["result"]["configOptions"];
    assert_eq!(option(options, "model")["currentValue"], "mock:cheap/model");
    assert_eq!(option(options, "mode")["currentValue"], "ask");
    assert_eq!(
        option(&set_mode.response["result"]["configOptions"], "mode")["currentValue"],
        "deny"
    );
    // Refusals are parameter errors and change nothing.
    assert_eq!(bad_model.response["error"]["code"], -32602);
    assert_eq!(bad_mode.response["error"]["code"], -32602);
    // The next turn really ran on the chosen model.
    assert_eq!(
        answer.text_of("agent_message_chunk"),
        "From the cheap model."
    );
    assert_eq!(server.requests()[0].body["model"], "cheap/model");

    // And the slash command told the editor the picker's new position.
    let update = after_command
        .updates
        .iter()
        .map(|m| &m["params"]["update"])
        .find(|u| u["sessionUpdate"] == "config_option_update")
        .expect("a config_option_update after /mode");
    assert_eq!(
        option(&update["configOptions"], "mode")["currentValue"],
        "auto"
    );
    assert_eq!(
        option(&update["configOptions"], "model")["currentValue"],
        "mock:cheap/model"
    );
}

/// Read messages until a `session/update` of `kind` arrives; returns its update.
fn next_update(client: &mut Client, kind: &str) -> Value {
    loop {
        let mut text = String::new();
        let read = client.stdout.read_line(&mut text).unwrap();
        assert!(read > 0, "the agent closed stdout waiting for {kind}");
        let message: Value = serde_json::from_str(&text).unwrap();
        client.log("A>", &message);
        if message["params"]["update"]["sessionUpdate"] == kind {
            return message["params"]["update"].clone();
        }
    }
}

fn group_names(model: &Value) -> Vec<String> {
    model["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|group| group["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn the_models_a_provider_lists_arrive_in_the_picker_under_their_own_heading() {
    let server = MockServer::start(vec![Reply::Status {
        status: 200,
        headers: vec![("content-type", "application/json".to_owned())],
        body: r#"{"data":[{"id":"vendor/alpha"},{"id":"vendor/beta","name":"Beta, the second"}]}"#
            .to_owned(),
    }])
    .await;
    let mut workspace = Workspace::new("model-lists", Some(&server));
    workspace.model_lists = "mock";
    let (first, last, set) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let created = client.request(
                "session/new",
                json!({"cwd": project.to_str().unwrap(), "mcpServers": []}),
            );
            let session = created.response["result"]["sessionId"]
                .as_str()
                .unwrap()
                .to_owned();
            let first = option(&created.response["result"]["configOptions"], "model").clone();
            // The list is fetched behind the session, and an update always says when
            // it is in, even if it won the race with the session's own response.
            let update = next_update(&mut client, "config_option_update");
            let last = option(&update["configOptions"], "model").clone();
            let set = client.request(
                "session/set_config_option",
                json!({"sessionId": session, "configId": "model", "value": "mock:vendor/beta"}),
            );
            client.finish();
            (first, last, set)
        }
    })
    .await
    .unwrap();

    // The session never waited for the network: its own list had the configured models.
    assert!(choice_values_flat(&first).contains(&"mock:test/model".to_owned()));
    assert_eq!(group_names(&last), ["Configured", "mock"]);
    let groups = last["options"].as_array().unwrap();
    assert_eq!(groups[0]["group"], "configured");
    assert!(groups[0]["options"][0]["value"] == "mock:test/model");
    assert_eq!(
        groups[1]["options"],
        json!([
            {"value": "mock:vendor/alpha", "name": "mock:vendor/alpha"},
            {"value": "mock:vendor/beta", "name": "Beta, the second"}
        ])
    );
    // And one of the fetched models can be picked.
    let chosen = option(&set.response["result"]["configOptions"], "model");
    assert_eq!(chosen["currentValue"], "mock:vendor/beta");
}

/// Every value in an option, whether its choices are flat or grouped.
fn choice_values_flat(option: &Value) -> Vec<String> {
    let mut values = Vec::new();
    for entry in option["options"].as_array().unwrap() {
        match entry["options"].as_array() {
            Some(members) => values.extend(
                members
                    .iter()
                    .map(|m| m["value"].as_str().unwrap().to_owned()),
            ),
            None => values.push(entry["value"].as_str().unwrap().to_owned()),
        }
    }
    values
}

#[tokio::test]
async fn saved_sessions_are_listed_with_their_directory_and_a_title_across_processes() {
    let server = MockServer::start(vec![sse_text("Noted.", "stop")]).await;
    let workspace = Workspace::new("list", Some(&server));
    let (session, empty_one) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let used = client.new_session(&project);
            client.prompt(&used, "Alpha question about the build");
            // A thread opened and never used has nothing to come back to.
            let unused = client.new_session(&project);
            client.finish();
            (used, unused)
        }
    })
    .await
    .unwrap();

    // A new process lists before it has any session of its own.
    let (all, here, elsewhere, relative) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let elsewhere = workspace.root.join("elsewhere");
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let all = client.request("session/list", json!({}));
            let here = client.request("session/list", json!({"cwd": project.to_str().unwrap()}));
            let elsewhere =
                client.request("session/list", json!({"cwd": elsewhere.to_str().unwrap()}));
            let relative = client.request("session/list", json!({"cwd": "nope"}));
            client.finish();
            (all, here, elsewhere, relative)
        }
    })
    .await
    .unwrap();

    let sessions = all.response["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let listed = &sessions[0];
    assert_eq!(listed["sessionId"], session);
    assert_ne!(listed["sessionId"], empty_one);
    assert!(listed["title"].as_str().unwrap().contains("Alpha question"));
    assert_eq!(
        listed["cwd"].as_str().unwrap().to_lowercase(),
        workspace.project().display().to_string().to_lowercase()
    );
    let stamp = listed["updatedAt"].as_str().unwrap();
    assert!(stamp.ends_with('Z') && stamp.len() == 20, "{stamp}");
    assert_eq!(
        here.response["result"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(elsewhere.response["result"]["sessions"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(relative.response["error"]["code"], -32602);
}

#[tokio::test]
async fn resume_lists_saved_sessions_and_continues_one_in_this_thread() {
    let server = MockServer::start(vec![
        sse_text("First answer.", "stop"),
        sse_text("Second answer.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("resume", Some(&server));
    let old = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            client.prompt(&session, "Alpha question about caching");
            client.finish();
            session
        }
    })
    .await
    .unwrap();
    let short: String = old.chars().take(8).collect();

    let (listing, bad, resumed, answer) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        let short = short.clone();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let listing = client.prompt(&session, "/resume");
            let bad = client.prompt(&session, "/resume zzzzzzzz");
            let resumed = client.prompt(&session, &format!("/resume {short}"));
            let answer = client.prompt(&session, "And a follow up?");
            client.finish();
            (listing, bad, resumed, answer)
        }
    })
    .await
    .unwrap();

    let shown = listing.text_of("agent_message_chunk");
    assert!(
        shown.contains(&short) && shown.contains("Alpha question"),
        "{shown}"
    );
    assert!(shown.contains("usage: /resume <id>"));
    assert!(bad
        .text_of("agent_message_chunk")
        .contains("no session 'zzzzzzzz'"));
    let note = resumed.text_of("agent_message_chunk");
    assert!(
        note.starts_with(&format!("resumed session {old}")),
        "{note}"
    );
    assert!(note.contains("Alpha question"), "{note}");
    // Nothing reached the model for those three commands; the follow-up carried the
    // resumed conversation with it.
    assert_eq!(answer.text_of("agent_message_chunk"), "Second answer.");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let sent = requests[1].body["messages"].to_string();
    assert!(sent.contains("Alpha question about caching"), "{sent}");
    assert!(sent.contains("First answer."), "{sent}");
    assert!(sent.contains("And a follow up?"), "{sent}");
}

/// A turn in which the model reasons, calls `list_dir`, and answers, at the
/// `/thinking` and `/toolview` levels given.
async fn display_turn(name: &str, thinking: &str, toolview: &str, path: &'static str) -> Exchange {
    let server = MockServer::start(vec![
        Reply::Sse(format!(
            "{}{}",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"weighing the options carefully\"},\"finish_reason\":null}]}\n\n",
            match sse_tool_calls(&[("c1", "read_file", path)]) {
                Reply::Sse(body) => body,
                _ => unreachable!(),
            }
        )),
        sse_text("All done.", "stop"),
    ])
    .await;
    let workspace = Workspace::new(name, Some(&server));
    std::fs::write(workspace.project().join("here.txt"), "hello\n").unwrap();
    let exchange = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        let (thinking, toolview) = (thinking.to_owned(), toolview.to_owned());
        move || {
            client.initialize();
            let session = client.new_session(&project);
            if !thinking.is_empty() {
                client.prompt(&session, &format!("/thinking {thinking}"));
            }
            if !toolview.is_empty() {
                client.prompt(&session, &format!("/toolview {toolview}"));
            }
            let exchange = client.prompt(&session, "Look at it");
            client.finish();
            exchange
        }
    })
    .await
    .unwrap();
    exchange
}

#[tokio::test]
async fn thinking_and_toolview_change_what_the_editor_is_shown() {
    let read = r#"{"path":"here.txt"}"#;

    // The default is everything.
    let full = display_turn("view-full", "", "", read).await;
    assert_eq!(
        full.kinds(),
        [
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk"
        ]
    );
    assert_eq!(
        full.updates[1]["params"]["update"]["rawInput"],
        json!({"path": "here.txt"})
    );

    // Summary: one reasoning line (before the call it led to), the call without its
    // arguments, and a one-line result.
    let summary = display_turn("view-summary", "summary", "summary", read).await;
    assert_eq!(
        summary.kinds(),
        [
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk"
        ]
    );
    let line = summary.updates[0]["params"]["update"]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(
        line.starts_with("∴ thought for ") && line.ends_with("30 chars"),
        "{line}"
    );
    assert!(summary.updates[1]["params"]["update"]["rawInput"].is_null());
    assert_eq!(
        summary.updates[2]["params"]["update"]["status"],
        "completed"
    );

    // Hidden: only the answer.
    let hidden = display_turn("view-hidden", "hidden", "hidden", read).await;
    assert_eq!(hidden.kinds(), ["agent_message_chunk"]);
    assert_eq!(hidden.text_of("agent_message_chunk"), "All done.");

    // But a call that failed is shown even then.
    let failed = display_turn(
        "view-failed",
        "hidden",
        "hidden",
        r#"{"path":"missing.txt"}"#,
    )
    .await;
    assert_eq!(
        failed.kinds(),
        ["tool_call", "tool_call_update", "agent_message_chunk"]
    );
    assert_eq!(failed.updates[1]["params"]["update"]["status"], "failed");
}

#[tokio::test]
async fn the_level_commands_report_and_refuse_like_the_terminals() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("levels", Some(&server));
    let (a, b, c, d) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let a = client.prompt(&session, "/thinking");
            let b = client.prompt(&session, "/thinking summary");
            let c = client.prompt(&session, "/toolview loud");
            let d = client.prompt(&session, "/thinking");
            client.finish();
            (a, b, c, d)
        }
    })
    .await
    .unwrap();
    assert_eq!(a.text_of("agent_message_chunk"), "thinking: full");
    assert!(b
        .text_of("agent_message_chunk")
        .starts_with("thinking: summary"));
    assert!(c
        .text_of("agent_message_chunk")
        .contains("unknown level 'loud'"));
    assert_eq!(d.text_of("agent_message_chunk"), "thinking: summary");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn the_secret_command_never_takes_or_repeats_a_value() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("secret-command", Some(&server));
    let (list, typed, forget_bad, forget_none) = tokio::task::spawn_blocking({
        let project = workspace.project();
        let mut client = workspace.connect();
        move || {
            client.initialize();
            let session = client.new_session(&project);
            let list = client.prompt(&session, "/secret");
            let typed = client.prompt(&session, "/secret API_KEY sk-live-0123456789abcdef");
            let forget_bad = client.prompt(&session, "/secret forget sk-live-0123456789!");
            let forget_none = client.prompt(&session, "/secret forget NOT_HELD");
            client.finish();
            (list, typed, forget_bad, forget_none)
        }
    })
    .await
    .unwrap();
    let shown = list.text_of("agent_message_chunk");
    assert!(shown.starts_with("no secrets for this project"), "{shown}");
    assert!(shown.contains("/secret NAME"), "{shown}");
    let shown = typed.text_of("agent_message_chunk");
    assert!(shown.starts_with("nothing was stored."), "{shown}");
    assert!(
        !shown.contains("sk-live") && !shown.contains("API_KEY"),
        "{shown}"
    );
    assert!(forget_bad
        .text_of("agent_message_chunk")
        .contains("not a valid secret name"));
    assert!(!forget_bad
        .text_of("agent_message_chunk")
        .contains("sk-live"));
    assert_eq!(
        forget_none.text_of("agent_message_chunk"),
        "no secret named NOT_HELD"
    );
    // None of it reached the model.
    assert!(server.requests().is_empty());
}
