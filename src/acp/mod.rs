//! The Agent Client Protocol server, beyond Python (`agent86 acp`).
//!
//! ACP lets an editor launch an agent as a subprocess and talk JSON-RPC 2.0 to it
//! over stdio: one message per line, UTF-8, no embedded newlines. The agent must
//! write nothing to stdout that is not a protocol message, so logging goes to
//! stderr and nothing else in this process may print to stdout in this mode.
//!
//! Built so far (slices 1 and 2 of the plan): the transport, the `initialize`
//! handshake, `session/new`, and `session/prompt` with streamed text and reasoning.
//! Tool calls, permission requests, cancellation, `session/load`, and editor-supplied
//! MCP servers come in later slices, so for now every other method answers "method
//! not found" rather than pretending, and the capabilities advertised are only what
//! is built.
//!
//! The protocol is separate from the agent: [`Server`] speaks JSON-RPC and drives a
//! [`Backend`], so the protocol is testable with a scripted backend and no process.
//! [`harness::HarnessBackend`] is the real one, over agent86's own harness. The
//! server reads one request at a time, so a prompt blocks the connection until its
//! turn ends; slice 4 (cancellation) is where reading moves to its own thread.

pub mod harness;

use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;

/// The ACP major version this server speaks. Version negotiation: an agent that
/// supports the requested version answers with it, and otherwise with the latest
/// it supports, which is this one.
pub const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC's reserved error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Something the agent reports while a prompt runs, sent as a `session/update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// A chunk of the agent's reply (`agent_message_chunk`). The harness's own
    /// notices (`[cancelled]`, `[guardrail] ...`) travel here too.
    Message(String),
    /// A chunk of the model's reasoning (`agent_thought_chunk`).
    Thought(String),
}

/// Why a prompt turn ended: the protocol's `stopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    MaxTurnRequests,
    Cancelled,
}

impl Stop {
    fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTurnRequests => "max_turn_requests",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A failure a [`Backend`] reports, sent as a JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub code: i64,
    pub message: String,
}

impl BackendError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: message.into(),
        }
    }
}

/// The agent behind the protocol.
pub trait Backend {
    /// Open a session jailed to `cwd` (an absolute path) and return its id.
    /// `mcp_servers` is how many servers the client named; none are connected yet.
    fn new_session(&mut self, cwd: &Path, mcp_servers: usize) -> Result<String, BackendError>;

    /// Run one turn on `session_id` with the user's `text`, reporting progress
    /// through `update` as it streams.
    fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        update: &mut dyn FnMut(Update),
    ) -> Result<Stop, BackendError>;
}

/// The protocol state of one connection.
pub struct Server<B: Backend> {
    backend: B,
    initialized: bool,
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            initialized: false,
        }
    }

    /// Whether the client has completed `initialize`.
    pub fn initialized(&self) -> bool {
        self.initialized
    }

    /// Handle one inbound line, passing each message to send to `emit` as it is
    /// ready (a prompt streams its updates before its response). A blank line, a
    /// notification, and a response from the client produce none.
    pub fn handle_line(&mut self, line: &str, emit: &mut dyn FnMut(Value)) {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            return;
        }
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => return emit(error_response(Value::Null, PARSE_ERROR, "Parse error")),
        };
        // ACP does not use JSON-RPC batches, so an array is not a request.
        let Value::Object(object) = message else {
            return emit(error_response(
                Value::Null,
                INVALID_REQUEST,
                "Invalid request",
            ));
        };
        self.handle_object(object, emit);
    }

    fn handle_object(&mut self, object: Map<String, Value>, emit: &mut dyn FnMut(Value)) {
        let id = object.get("id").cloned();
        let valid_id = matches!(
            id,
            None | Some(Value::Null) | Some(Value::String(_)) | Some(Value::Number(_))
        );
        let method = object.get("method").and_then(Value::as_str);
        match (method, id) {
            (Some(method), Some(id)) if valid_id => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                let response = match self.request(method, &params, emit) {
                    Ok(result) => success_response(id, result),
                    Err(error) => error_response(id, error.code, &error.message),
                };
                emit(response);
            }
            // A notification has no id and gets no reply, even when unknown.
            (Some(_), None) => {}
            // A response to a request this server sent. None are sent yet.
            (None, Some(_)) if object.contains_key("result") || object.contains_key("error") => {}
            _ => emit(error_response(
                Value::Null,
                INVALID_REQUEST,
                "Invalid request",
            )),
        }
    }

    fn request(
        &mut self,
        method: &str,
        params: &Value,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Value, BackendError> {
        match method {
            "initialize" => {
                let result = initialize(params).map_err(BackendError::invalid_params)?;
                self.initialized = true;
                Ok(result)
            }
            "session/new" | "session/prompt" if !self.initialized => Err(BackendError {
                code: INVALID_REQUEST,
                message: "Not initialized: call initialize first".into(),
            }),
            "session/new" => self.session_new(params),
            "session/prompt" => self.session_prompt(params, emit),
            other => Err(BackendError {
                code: METHOD_NOT_FOUND,
                message: format!("Method not found: {other}"),
            }),
        }
    }

    fn session_new(&mut self, params: &Value) -> Result<Value, BackendError> {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError::invalid_params("Invalid params: cwd must be a string"))?;
        let cwd = Path::new(cwd);
        if !cwd.is_absolute() {
            return Err(BackendError::invalid_params(
                "Invalid params: cwd must be an absolute path",
            ));
        }
        let servers = match params.get("mcpServers") {
            None | Some(Value::Null) => 0,
            Some(Value::Array(servers)) => servers.len(),
            Some(_) => {
                return Err(BackendError::invalid_params(
                    "Invalid params: mcpServers must be an array",
                ))
            }
        };
        let session_id = self.backend.new_session(cwd, servers)?;
        Ok(json!({ "sessionId": session_id }))
    }

    fn session_prompt(
        &mut self,
        params: &Value,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Value, BackendError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BackendError::invalid_params("Invalid params: sessionId must be a string")
            })?;
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                BackendError::invalid_params("Invalid params: prompt must be an array")
            })?;
        let text = prompt_text(blocks).map_err(BackendError::invalid_params)?;

        let mut update = |update: Update| emit(session_update(session_id, &update));
        let stop = self.backend.prompt(session_id, &text, &mut update)?;
        Ok(json!({ "stopReason": stop.as_str() }))
    }
}

/// The user's text from a prompt's content blocks. Every agent must accept `text`
/// and `resource_link`; the richer kinds are not advertised, so a client that sends
/// one anyway is told so rather than having it dropped without a word.
fn prompt_text(blocks: &[Value]) -> Result<String, String> {
    let mut parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => match block.get("text").and_then(Value::as_str) {
                Some(text) => parts.push(text.to_owned()),
                None => return Err("Invalid params: a text block needs a text string".into()),
            },
            Some("resource_link") => {
                let uri = block.get("uri").and_then(Value::as_str);
                let name = block.get("name").and_then(Value::as_str);
                match (uri, name) {
                    (Some(uri), Some(name)) => parts.push(format!("{name} ({uri})")),
                    (Some(uri), None) => parts.push(uri.to_owned()),
                    _ => return Err("Invalid params: a resource_link block needs a uri".into()),
                }
            }
            Some(other) => {
                return Err(format!(
                    "Invalid params: content block type '{other}' is not supported \
                     (this agent accepts text and resource_link)"
                ))
            }
            None => return Err("Invalid params: a content block needs a type".into()),
        }
    }
    if parts.iter().all(|part| part.trim().is_empty()) {
        return Err("Invalid params: the prompt is empty".into());
    }
    Ok(parts.join("\n"))
}

/// The `session/update` notification for `update`.
fn session_update(session_id: &str, update: &Update) -> Value {
    let (kind, text) = match update {
        Update::Message(text) => ("agent_message_chunk", text),
        Update::Thought(text) => ("agent_thought_chunk", text),
    };
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": kind,
                "content": { "type": "text", "text": text }
            }
        }
    })
}

/// The `initialize` result. The capabilities are exactly what is implemented, and
/// a client treats an omitted capability as unsupported.
fn initialize(params: &Value) -> Result<Value, String> {
    let Some(params) = params.as_object() else {
        return Err("Invalid params: expected an object".into());
    };
    // The requested version is "the latest the client supports". Whatever it is,
    // the answer is the version this server speaks: the same one if it matches,
    // else the latest supported. It still has to be a whole number.
    if params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err("Invalid params: protocolVersion must be an integer".into());
    }
    Ok(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": false
            },
            "mcpCapabilities": {
                "http": false,
                "sse": false
            }
        },
        "agentInfo": {
            "name": "agent86",
            "title": "agent86",
            "version": env!("CARGO_PKG_VERSION")
        },
        "authMethods": []
    }))
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Read newline-delimited messages from `input` until it ends, writing each message
/// to `output` and flushing it as it is produced, since the client is waiting on
/// it and a prompt's updates should reach the editor while the turn runs. End of
/// input is the client closing the connection, so it ends cleanly.
pub fn serve<B: Backend, R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    backend: B,
) -> io::Result<()> {
    let mut server = Server::new(backend);
    let mut buffer = Vec::new();
    // The first write failure (the client went away) stops everything after it.
    let mut failed: Option<io::Error> = None;
    let mut emit = |message: Value| {
        if failed.is_some() {
            return;
        }
        // `to_string` is compact and escapes newlines inside strings, so a
        // message is always exactly one line.
        let result = serde_json::to_string(&message)
            .map_err(io::Error::from)
            .and_then(|line| writeln!(output, "{line}"))
            .and_then(|()| output.flush());
        if let Err(error) = result {
            failed = Some(error);
        }
    };
    loop {
        buffer.clear();
        if input.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        match std::str::from_utf8(&buffer) {
            Ok(line) => server.handle_line(line, &mut emit),
            // Messages must be UTF-8; anything else is not valid JSON.
            Err(_) => emit(error_response(Value::Null, PARSE_ERROR, "Parse error")),
        }
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted backend: sessions are numbered, and a prompt streams what the
    /// test queued, then ends the way it says.
    #[derive(Default)]
    struct Script {
        sessions: Vec<(String, usize)>,
        prompts: Vec<(String, String)>,
        updates: Vec<Update>,
        stop: Option<Result<Stop, BackendError>>,
    }

    impl Backend for Script {
        fn new_session(&mut self, cwd: &Path, mcp_servers: usize) -> Result<String, BackendError> {
            self.sessions
                .push((cwd.to_string_lossy().into_owned(), mcp_servers));
            Ok(format!("sess_{}", self.sessions.len()))
        }

        fn prompt(
            &mut self,
            session_id: &str,
            text: &str,
            update: &mut dyn FnMut(Update),
        ) -> Result<Stop, BackendError> {
            self.prompts.push((session_id.to_owned(), text.to_owned()));
            for item in self.updates.drain(..) {
                update(item);
            }
            self.stop.take().unwrap_or(Ok(Stop::EndTurn))
        }
    }

    fn replies<B: Backend>(server: &mut Server<B>, line: &str) -> Vec<Value> {
        let mut out = Vec::new();
        server.handle_line(line, &mut |message| out.push(message));
        out
    }

    fn one<B: Backend>(server: &mut Server<B>, line: &str) -> Value {
        let mut out = replies(server, line);
        assert_eq!(out.len(), 1, "expected one reply to {line}: {out:?}");
        out.remove(0)
    }

    fn ready() -> Server<Script> {
        let mut server = Server::new(Script::default());
        one(&mut server, INITIALIZE);
        server
    }

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true}},"clientInfo":{"name":"zed","version":"1"}}}"#;

    #[test]
    fn initialize_answers_with_the_version_and_only_what_is_built() {
        let mut server = Server::new(Script::default());
        let reply = one(&mut server, INITIALIZE);
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 0);
        let result = &reply["result"];
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["authMethods"], json!([]));
        assert_eq!(result["agentInfo"]["name"], "agent86");
        assert_eq!(result["agentInfo"]["version"], env!("CARGO_PKG_VERSION"));
        let capabilities = &result["agentCapabilities"];
        assert_eq!(capabilities["loadSession"], false);
        assert_eq!(capabilities["promptCapabilities"]["image"], false);
        assert_eq!(capabilities["promptCapabilities"]["audio"], false);
        assert_eq!(capabilities["promptCapabilities"]["embeddedContext"], false);
        assert_eq!(capabilities["mcpCapabilities"]["http"], false);
        assert_eq!(capabilities["mcpCapabilities"]["sse"], false);
        assert!(server.initialized());
    }

    #[test]
    fn a_newer_client_version_gets_the_latest_this_server_speaks() {
        let mut server = Server::new(Script::default());
        let reply = one(
            &mut server,
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":7}}"#,
        );
        assert_eq!(reply["id"], "a");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_needs_an_integer_version() {
        for params in [
            r#""params":{}"#,
            r#""params":{"protocolVersion":"1"}"#,
            r#""params":{"protocolVersion":1.5}"#,
            r#""params":[1]"#,
            r#""other":1"#,
        ] {
            let mut server = Server::new(Script::default());
            let line = format!(r#"{{"jsonrpc":"2.0","id":3,"method":"initialize",{params}}}"#);
            let reply = one(&mut server, &line);
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{params}");
            assert_eq!(reply["id"], 3);
            assert!(!server.initialized(), "{params}");
        }
    }

    #[test]
    fn methods_not_built_yet_say_so() {
        let mut server = ready();
        for method in ["session/load", "session/set_mode", "nonsense"] {
            let line = format!(r#"{{"jsonrpc":"2.0","id":9,"method":"{method}","params":{{}}}}"#);
            let reply = one(&mut server, &line);
            assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
            assert_eq!(
                reply["error"]["message"],
                format!("Method not found: {method}")
            );
        }
    }

    #[test]
    fn notifications_and_client_responses_get_no_reply() {
        let mut server = ready();
        for line in [
            r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#,
            r#"{"jsonrpc":"2.0","method":"unknown"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#,
            "",
            "   \r\n",
        ] {
            assert!(replies(&mut server, line).is_empty(), "{line:?}");
        }
    }

    #[test]
    fn bad_input_gets_the_json_rpc_errors_with_a_null_id() {
        let mut server = ready();
        let reply = one(&mut server, "{not json");
        assert_eq!(reply["error"]["code"], PARSE_ERROR);
        assert_eq!(reply["id"], Value::Null);
        for line in [
            "[]",
            "42",
            r#"{"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","id":{"x":1},"method":"initialize"}"#,
        ] {
            let reply = one(&mut server, line);
            assert_eq!(reply["error"]["code"], INVALID_REQUEST, "{line}");
            assert_eq!(reply["id"], Value::Null);
        }
    }

    #[test]
    fn sessions_need_the_handshake_first() {
        let mut server = Server::new(Script::default());
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/w","mcpServers":[]}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}"#,
        ] {
            let reply = one(&mut server, line);
            assert_eq!(reply["error"]["code"], INVALID_REQUEST, "{line}");
        }
        assert!(server.backend.sessions.is_empty());
    }

    #[test]
    fn session_new_passes_the_directory_and_returns_the_id() {
        let mut server = ready();
        let cwd = std::env::temp_dir();
        let line = json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd": cwd, "mcpServers":[{"name":"a"},{"name":"b"}]}})
        .to_string();
        let reply = one(&mut server, &line);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["sessionId"], "sess_1");
        assert_eq!(
            server.backend.sessions,
            vec![(cwd.to_string_lossy().into_owned(), 2)]
        );
    }

    #[test]
    fn session_new_validates_its_parameters() {
        let mut server = ready();
        for params in [
            r#"{"mcpServers":[]}"#,
            r#"{"cwd":7,"mcpServers":[]}"#,
            r#"{"cwd":"relative/dir","mcpServers":[]}"#,
            r#"null"#,
        ] {
            let line =
                format!(r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{params}}}"#);
            let reply = one(&mut server, &line);
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{params}");
        }
        let cwd = std::env::temp_dir();
        let line = json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd": cwd, "mcpServers": "no"}})
        .to_string();
        assert_eq!(one(&mut server, &line)["error"]["code"], INVALID_PARAMS);
        assert!(server.backend.sessions.is_empty());
    }

    fn prompt_line(blocks: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{{"sessionId":"sess_1","prompt":{blocks}}}}}"#
        )
    }

    #[test]
    fn a_prompt_streams_its_updates_before_the_response() {
        let mut server = ready();
        server.backend.updates = vec![
            Update::Thought("hmm".into()),
            Update::Message("Hel".into()),
            Update::Message("lo".into()),
        ];
        let out = replies(
            &mut server,
            &prompt_line(r#"[{"type":"text","text":"hi"},{"type":"text","text":"there"}]"#),
        );
        assert_eq!(out.len(), 4, "{out:?}");
        for (message, (kind, text)) in out.iter().zip([
            ("agent_thought_chunk", "hmm"),
            ("agent_message_chunk", "Hel"),
            ("agent_message_chunk", "lo"),
        ]) {
            assert_eq!(message["method"], "session/update");
            assert!(message.get("id").is_none(), "an update is a notification");
            assert_eq!(message["params"]["sessionId"], "sess_1");
            assert_eq!(message["params"]["update"]["sessionUpdate"], kind);
            assert_eq!(message["params"]["update"]["content"]["type"], "text");
            assert_eq!(message["params"]["update"]["content"]["text"], text);
        }
        assert_eq!(out[3]["id"], 5);
        assert_eq!(out[3]["result"]["stopReason"], "end_turn");
        assert_eq!(
            server.backend.prompts,
            vec![("sess_1".to_owned(), "hi\nthere".to_owned())]
        );
    }

    #[test]
    fn stop_reasons_and_backend_errors_come_through() {
        let mut server = ready();
        for (stop, name) in [
            (Stop::MaxTurnRequests, "max_turn_requests"),
            (Stop::Cancelled, "cancelled"),
        ] {
            server.backend.stop = Some(Ok(stop));
            let reply = one(&mut server, &prompt_line(r#"[{"type":"text","text":"x"}]"#));
            assert_eq!(reply["result"]["stopReason"], name);
        }
        server.backend.stop = Some(Err(BackendError::internal("provider down")));
        let reply = one(&mut server, &prompt_line(r#"[{"type":"text","text":"x"}]"#));
        assert_eq!(reply["error"]["code"], INTERNAL_ERROR);
        assert_eq!(reply["error"]["message"], "provider down");
    }

    #[test]
    fn resource_links_become_text_and_richer_blocks_are_refused() {
        let mut server = ready();
        replies(
            &mut server,
            &prompt_line(
                r#"[{"type":"text","text":"read"},{"type":"resource_link","uri":"file:///a.rs","name":"a.rs"},{"type":"resource_link","uri":"file:///b"}]"#,
            ),
        );
        assert_eq!(
            server.backend.prompts[0].1,
            "read\na.rs (file:///a.rs)\nfile:///b"
        );

        for blocks in [
            r#"[{"type":"image","data":"x","mimeType":"image/png"}]"#,
            r#"[{"type":"resource","resource":{"uri":"file:///a","text":"x"}}]"#,
            r#"[{"type":"text"}]"#,
            r#"[{"text":"no type"}]"#,
            r#"[{"type":"resource_link","name":"n"}]"#,
            r#"[]"#,
            r#"[{"type":"text","text":"  "}]"#,
            r#""not an array""#,
        ] {
            let reply = one(&mut server, &prompt_line(blocks));
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{blocks}");
        }
        assert_eq!(server.backend.prompts.len(), 1, "no refused prompt ran");
    }

    #[test]
    fn a_prompt_needs_a_session_id() {
        let mut server = ready();
        let reply = one(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"x"}]}}"#,
        );
        assert_eq!(reply["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn serve_writes_one_flushed_line_per_message_and_ends_at_end_of_input() {
        let input =
            format!("{INITIALIZE}\n\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}}\r\n");
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, Script::default()).unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));
        for line in lines {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }

    #[test]
    fn serve_survives_a_last_line_without_a_newline_and_invalid_utf8() {
        let mut input = b"\xff\xfe\n".to_vec();
        input.extend_from_slice(INITIALIZE.as_bytes());
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output, Script::default()).unwrap();
        let text = String::from_utf8(output).unwrap();
        let out: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(out[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(out[1]["result"]["protocolVersion"], 1);
    }

    #[test]
    fn serve_reports_a_closed_output() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let input = format!("{INITIALIZE}\n");
        let error = serve(input.as_bytes(), Closed, Script::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn a_string_with_a_newline_stays_on_one_line() {
        let mut server = ready();
        // The method name is echoed in the error message.
        let reply = one(&mut server, r#"{"jsonrpc":"2.0","id":1,"method":"a\nb"}"#);
        let line = serde_json::to_string(&reply).unwrap();
        assert!(!line.contains('\n'));
    }
}
