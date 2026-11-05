//! The Agent Client Protocol server, beyond Python (`agent86 acp`).
//!
//! ACP lets an editor launch an agent as a subprocess and talk JSON-RPC 2.0 to it
//! over stdio: one message per line, UTF-8, no embedded newlines. The agent must
//! write nothing to stdout that is not a protocol message, so logging goes to
//! stderr and nothing else in this process may print to stdout in this mode.
//!
//! Built so far (slices 1 to 3 of the plan): the transport, the `initialize`
//! handshake, `session/new`, `session/prompt` with streamed text and reasoning, tool
//! calls reported as they happen, and `session/request_permission` for the calls the
//! approval mode asks about. Cancellation, `session/load`, and editor-supplied MCP
//! servers come in later slices, so for now every other method answers "method not
//! found" rather than pretending, and the capabilities advertised are only what is
//! built.
//!
//! Threads, as in the interactive interface: a thread of its own reads stdin and
//! hands every line over a channel; a prompt's turn runs on a worker thread; and the
//! connection's thread pumps both, writing what the turn reports and passing the
//! editor's replies to whoever waits for them. A permission request is the case that
//! needs it: the turn blocks on the editor's answer while the connection keeps
//! reading. While a turn runs, a second request is refused (one turn at a time).
//!
//! The protocol is separate from the agent: [`Server`] speaks JSON-RPC and drives a
//! [`Backend`], so the protocol is testable with a scripted backend and no process.
//! [`harness::HarnessBackend`] is the real one, over agent86's own harness.

pub mod harness;

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

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

/// The ids of the two options every permission request offers. Only `allow-once`
/// approves; the agent keeps no remembered decisions yet, so there is no
/// `allow_always` to offer.
pub const ALLOW_ONCE: &str = "allow-once";
pub const REJECT_ONCE: &str = "reject-once";

/// The category of a tool call, which an editor uses to pick an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Execute,
    Fetch,
    Other,
}

impl ToolKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Execute => "execute",
            Self::Fetch => "fetch",
            Self::Other => "other",
        }
    }
}

/// Something the agent reports while a prompt runs, sent as a `session/update`.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// A chunk of the agent's reply (`agent_message_chunk`). The harness's own
    /// notices (`[cancelled]`, `[guardrail] ...`) travel here too.
    Message(String),
    /// A chunk of the model's reasoning (`agent_thought_chunk`).
    Thought(String),
    /// The model asked for a tool call (`tool_call`, status `pending`).
    ToolCall {
        id: String,
        title: String,
        name: String,
        kind: ToolKind,
        raw_input: Value,
    },
    /// A tool call's outcome (`tool_call_update`, status `completed` or `failed`),
    /// with the text the model is shown.
    ToolResult { id: String, ok: bool, text: String },
}

/// What the agent asks the editor to approve.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    /// The call, as already reported by [`Update::ToolCall`].
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub raw_input: Value,
    /// What the call would do, in full: a diff, a command, or the arguments.
    pub text: String,
}

/// The editor's answer. Anything but the user choosing `allow-once` is a refusal:
/// a rejection, a cancelled turn, an option this agent did not offer, an error
/// reply, or a connection that is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Allowed,
    Declined,
}

/// How a running turn reaches the editor. Called from the turn's worker thread.
pub trait TurnSink: Send + Sync {
    /// Report progress; it is sent as the turn runs.
    fn update(&self, update: Update);

    /// Ask the editor and wait for the answer.
    fn ask(&self, request: PermissionRequest) -> Permission;
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

/// The agent behind the protocol. `Send`, because a turn runs on a worker thread.
pub trait Backend: Send {
    /// Open a session jailed to `cwd` (an absolute path) and return its id.
    /// `mcp_servers` is how many servers the client named; none are connected yet.
    fn new_session(&mut self, cwd: &Path, mcp_servers: usize) -> Result<String, BackendError>;

    /// Run one turn on `session_id` with the user's `text`, reporting through
    /// `sink` as it goes. Runs on a worker thread, and may block on `sink.ask`.
    fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        sink: Arc<dyn TurnSink>,
    ) -> Result<Stop, BackendError>;
}

/// What the connection's thread waits on: the reader's lines and the turn's reports.
#[derive(Debug)]
pub enum Event {
    /// One line from the client.
    Line(String),
    /// A line that is not UTF-8.
    Invalid,
    /// The client closed its end.
    Eof,
    /// Reading failed.
    ReadError(String),
    /// From the running turn.
    Turn(TurnMsg),
}

/// What a running turn tells the connection.
#[derive(Debug)]
pub enum TurnMsg {
    Update(Update),
    Ask {
        request: PermissionRequest,
        reply: Sender<Permission>,
    },
    Done(Result<Stop, BackendError>),
}

/// The [`TurnSink`] that hands everything to the connection's thread.
struct ChannelSink {
    events: Sender<Event>,
}

impl TurnSink for ChannelSink {
    fn update(&self, update: Update) {
        let _ = self.events.send(Event::Turn(TurnMsg::Update(update)));
    }

    fn ask(&self, request: PermissionRequest) -> Permission {
        let (reply, answer) = channel();
        if self
            .events
            .send(Event::Turn(TurnMsg::Ask { request, reply }))
            .is_err()
        {
            return Permission::Declined;
        }
        // A dropped reply (the connection closed) declines.
        answer.recv().unwrap_or(Permission::Declined)
    }
}

/// The protocol state of one connection.
pub struct Server<B: Backend> {
    backend: B,
    initialized: bool,
    events: Receiver<Event>,
    sender: Sender<Event>,
    /// The permission requests sent and not yet answered, by request id.
    pending: HashMap<u64, Sender<Permission>>,
    next_request: u64,
    /// The client is gone: the connection ends once the current line is handled.
    closed: bool,
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B) -> Self {
        let (sender, events) = channel();
        Self {
            backend,
            initialized: false,
            events,
            sender,
            pending: HashMap::new(),
            next_request: 0,
            closed: false,
        }
    }

    /// A handle for feeding the connection's events, for the reader thread.
    pub fn sender(&self) -> Sender<Event> {
        self.sender.clone()
    }

    /// Whether the client has completed `initialize`.
    pub fn initialized(&self) -> bool {
        self.initialized
    }

    /// Handle one inbound line, passing each message to send to `emit` as it is
    /// ready (a prompt streams its updates, and may ask for permission, before its
    /// response). A blank line, a notification, and a response from the client
    /// outside a turn produce none. A prompt does not return until its turn ends.
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
        match classify(&object) {
            Inbound::Request { id, method } => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                let response = match self.request(&method, &params, emit) {
                    Ok(result) => success_response(id, result),
                    Err(error) => error_response(id, error.code, &error.message),
                };
                emit(response);
            }
            // A notification has no id and gets no reply, even when unknown.
            Inbound::Notification => {}
            // A response to a request this server sent; none is outstanding
            // outside a turn.
            Inbound::Response { .. } => {}
            Inbound::Invalid => emit(error_response(
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
            })?
            .to_owned();
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                BackendError::invalid_params("Invalid params: prompt must be an array")
            })?;
        let text = prompt_text(blocks).map_err(BackendError::invalid_params)?;

        let sink: Arc<dyn TurnSink> = Arc::new(ChannelSink {
            events: self.sender.clone(),
        });
        let Self {
            backend,
            events,
            sender,
            pending,
            next_request,
            closed,
            ..
        } = self;
        let stop = std::thread::scope(|scope| {
            let done = sender.clone();
            let turn_session = session_id.clone();
            scope.spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    backend.prompt(&turn_session, &text, sink)
                }))
                .unwrap_or_else(|_| Err(BackendError::internal("the turn panicked")));
                let _ = done.send(Event::Turn(TurnMsg::Done(result)));
            });
            // Pump until the turn is done: write what it reports, and route what the
            // client sends meanwhile.
            loop {
                match events.recv() {
                    Ok(Event::Turn(TurnMsg::Update(update))) => {
                        emit(session_update(&session_id, &update))
                    }
                    Ok(Event::Turn(TurnMsg::Ask { request, reply })) => {
                        if *closed {
                            // Nobody to ask: dropping the reply declines.
                            continue;
                        }
                        let id = *next_request;
                        *next_request += 1;
                        pending.insert(id, reply);
                        emit(permission_request(id, &session_id, &request));
                    }
                    Ok(Event::Turn(TurnMsg::Done(result))) => break result,
                    Ok(Event::Line(line)) => during_turn(&line, pending, emit),
                    Ok(Event::Invalid) => {
                        emit(error_response(Value::Null, PARSE_ERROR, "Parse error"))
                    }
                    Ok(Event::Eof | Event::ReadError(_)) => {
                        *closed = true;
                        // Dropping the replies declines whatever waits on them.
                        pending.clear();
                    }
                    Err(_) => break Err(BackendError::internal("the connection was lost")),
                }
            }
        });
        pending.clear();
        Ok(json!({ "stopReason": stop?.as_str() }))
    }
}

/// How an inbound message reads.
enum Inbound {
    Request {
        id: Value,
        method: String,
    },
    Notification,
    Response {
        id: Value,
        object: Map<String, Value>,
    },
    Invalid,
}

fn classify(object: &Map<String, Value>) -> Inbound {
    let id = object.get("id").cloned();
    let valid_id = matches!(
        id,
        None | Some(Value::Null) | Some(Value::String(_)) | Some(Value::Number(_))
    );
    match (object.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) if valid_id => Inbound::Request {
            id,
            method: method.to_owned(),
        },
        (Some(_), None) => Inbound::Notification,
        (None, Some(id))
            if valid_id && (object.contains_key("result") || object.contains_key("error")) =>
        {
            Inbound::Response {
                id,
                object: object.clone(),
            }
        }
        _ => Inbound::Invalid,
    }
}

/// Handle a line from the client while a turn runs. A response answers a
/// permission request; a request has to wait, since one turn runs at a time;
/// notifications are ignored until cancellation (slice 4) gives one a meaning.
fn during_turn(
    line: &str,
    pending: &mut HashMap<u64, Sender<Permission>>,
    emit: &mut dyn FnMut(Value),
) {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return;
    }
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) else {
        return emit(error_response(Value::Null, PARSE_ERROR, "Parse error"));
    };
    match classify(&object) {
        Inbound::Response { id, object } => {
            if let Some(reply) = id.as_u64().and_then(|id| pending.remove(&id)) {
                let _ = reply.send(permission_from(&object));
            }
        }
        Inbound::Request { id, .. } => emit(error_response(
            id,
            INVALID_REQUEST,
            "A prompt turn is running; wait for its response",
        )),
        Inbound::Notification => {}
        Inbound::Invalid => emit(error_response(
            Value::Null,
            INVALID_REQUEST,
            "Invalid request",
        )),
    }
}

/// The user's answer to a permission request. Only choosing `allow-once` allows.
fn permission_from(response: &Map<String, Value>) -> Permission {
    let outcome = response
        .get("result")
        .and_then(|result| result.get("outcome"));
    let selected = outcome.and_then(|outcome| outcome.get("outcome")) == Some(&json!("selected"));
    let option = outcome
        .and_then(|outcome| outcome.get("optionId"))
        .and_then(Value::as_str);
    if selected && option == Some(ALLOW_ONCE) {
        Permission::Allowed
    } else {
        Permission::Declined
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

fn text_content(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// The `session/update` notification for `update`.
fn session_update(session_id: &str, update: &Update) -> Value {
    let body = match update {
        Update::Message(text) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": text_content(text)
        }),
        Update::Thought(text) => json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": text_content(text)
        }),
        Update::ToolCall {
            id,
            title,
            name,
            kind,
            raw_input,
        } => json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": title,
            "name": name,
            "kind": kind.as_str(),
            "status": "pending",
            "rawInput": raw_input
        }),
        Update::ToolResult { id, ok, text } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if *ok { "completed" } else { "failed" },
            "content": [{ "type": "content", "content": text_content(text) }]
        }),
    };
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id, "update": body }
    })
}

/// The `session/request_permission` request: the call, in full, and the two
/// choices. The text is what the user is deciding on, so it is the whole detail.
fn permission_request(id: u64, session_id: &str, request: &PermissionRequest) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": request.tool_call_id,
                "title": request.title,
                "kind": request.kind.as_str(),
                "status": "pending",
                "rawInput": request.raw_input,
                "content": [{ "type": "content", "content": text_content(&request.text) }]
            },
            "options": [
                { "optionId": ALLOW_ONCE, "name": "Allow once", "kind": "allow_once" },
                { "optionId": REJECT_ONCE, "name": "Reject", "kind": "reject_once" }
            ]
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

/// Writes messages as lines and flushes each, since the client is waiting on it and
/// a turn's updates should reach the editor while it runs. The first failure (the
/// client went away) is kept and ends the connection.
struct Writer<W: Write> {
    output: W,
    failed: Option<io::Error>,
}

impl<W: Write> Writer<W> {
    fn send(&mut self, message: Value) {
        if self.failed.is_some() {
            return;
        }
        // `to_string` is compact and escapes newlines inside strings, so a
        // message is always exactly one line.
        let result = serde_json::to_string(&message)
            .map_err(io::Error::from)
            .and_then(|line| writeln!(self.output, "{line}"))
            .and_then(|()| self.output.flush());
        if let Err(error) = result {
            self.failed = Some(error);
        }
    }
}

/// Read newline-delimited messages from `input` on a thread of its own, so the
/// connection can keep listening while a turn runs.
fn read_loop<R: BufRead>(mut input: R, events: Sender<Event>) {
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let event = match input.read_until(b'\n', &mut buffer) {
            Ok(0) => {
                let _ = events.send(Event::Eof);
                return;
            }
            Ok(_) => match String::from_utf8(std::mem::take(&mut buffer)) {
                Ok(line) => Event::Line(line),
                // Messages must be UTF-8; anything else is not valid JSON.
                Err(_) => Event::Invalid,
            },
            Err(error) => {
                let _ = events.send(Event::ReadError(error.to_string()));
                return;
            }
        };
        if events.send(event).is_err() {
            return;
        }
    }
}

/// Serve the protocol on `input` and `output` until the client closes the
/// connection, which ends cleanly. `input` is read on a thread of its own.
pub fn serve<B, R, W>(input: R, output: W, backend: B) -> io::Result<()>
where
    B: Backend,
    R: BufRead + Send + 'static,
    W: Write,
{
    let mut server = Server::new(backend);
    let events = server.sender();
    std::thread::spawn(move || read_loop(input, events));
    let mut writer = Writer {
        output,
        failed: None,
    };
    let mut read_error = None;
    while !server.closed && writer.failed.is_none() {
        match server.events.recv() {
            Ok(Event::Line(line)) => {
                server.handle_line(&line, &mut |message| writer.send(message));
            }
            Ok(Event::Invalid) => {
                writer.send(error_response(Value::Null, PARSE_ERROR, "Parse error"))
            }
            Ok(Event::ReadError(error)) => {
                read_error = Some(error);
                break;
            }
            // The client closing its end is how a session ends.
            Ok(Event::Eof) | Err(_) => break,
            // A turn's leftovers: the turn is over.
            Ok(Event::Turn(_)) => {}
        }
    }
    if let Some(error) = writer.failed {
        return Err(error);
    }
    match read_error {
        Some(error) => Err(io::Error::other(error)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Mutex;

    /// What a scripted turn does, in order.
    enum Act {
        Update(Update),
        Ask(PermissionRequest),
    }

    /// A scripted backend: sessions are numbered, and a prompt does what the test
    /// queued, then ends the way it says.
    #[derive(Default)]
    struct Script {
        sessions: Vec<(String, usize)>,
        prompts: Vec<(String, String)>,
        acts: Vec<Act>,
        answers: Arc<Mutex<Vec<Permission>>>,
        stop: Option<Result<Stop, BackendError>>,
        panic: bool,
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
            sink: Arc<dyn TurnSink>,
        ) -> Result<Stop, BackendError> {
            self.prompts.push((session_id.to_owned(), text.to_owned()));
            if self.panic {
                panic!("scripted panic");
            }
            for act in self.acts.drain(..) {
                match act {
                    Act::Update(update) => sink.update(update),
                    Act::Ask(request) => {
                        let answer = sink.ask(request);
                        self.answers.lock().unwrap().push(answer);
                    }
                }
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

    const HI: &str = r#"[{"type":"text","text":"hi"}]"#;

    #[test]
    fn a_prompt_streams_its_updates_before_the_response() {
        let mut server = ready();
        server.backend.acts = vec![
            Act::Update(Update::Thought("hmm".into())),
            Act::Update(Update::Message("Hel".into())),
            Act::Update(Update::Message("lo".into())),
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

    fn write_call() -> Update {
        Update::ToolCall {
            id: "call_1".into(),
            title: "write_file(...)".into(),
            name: "write_file".into(),
            kind: ToolKind::Edit,
            raw_input: json!({"path": "a.txt", "content": "x"}),
        }
    }

    fn write_request() -> PermissionRequest {
        PermissionRequest {
            tool_call_id: "call_1".into(),
            title: "write_file(...)".into(),
            kind: ToolKind::Edit,
            raw_input: json!({"path": "a.txt"}),
            text: "--- a/a.txt\n+++ b/a.txt".into(),
        }
    }

    /// Run a prompt that reports a call, asks about it, and reports its result,
    /// answering the request with `reply`. Returns everything written, in order.
    fn ask_and_answer(
        server: &mut Server<Script>,
        reply: impl Fn(u64) -> String + 'static,
    ) -> Vec<Value> {
        server.backend.acts = vec![
            Act::Update(write_call()),
            Act::Ask(write_request()),
            Act::Update(Update::ToolResult {
                id: "call_1".into(),
                ok: true,
                text: "Wrote 1 byte".into(),
            }),
        ];
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "session/request_permission" {
                let id = message["id"].as_u64().unwrap();
                events.send(Event::Line(reply(id))).unwrap();
            }
            out.push(message);
        });
        out
    }

    /// How a test answers a permission request, given its id.
    type Reply = Box<dyn Fn(u64) -> String>;

    fn selected(id: u64, option: &str) -> String {
        json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"selected","optionId":option}}})
            .to_string()
    }

    #[test]
    fn a_tool_call_is_reported_asked_about_and_finished_in_order() {
        let mut server = ready();
        let out = ask_and_answer(&mut server, |id| selected(id, ALLOW_ONCE));
        let shape: Vec<String> = out
            .iter()
            .map(|m| {
                m["method"].as_str().map_or_else(
                    || "response".to_owned(),
                    |method| match method {
                        "session/update" => m["params"]["update"]["sessionUpdate"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                        other => other.to_owned(),
                    },
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                "tool_call",
                "session/request_permission",
                "tool_call_update",
                "response"
            ]
        );

        let call = &out[0]["params"]["update"];
        assert_eq!(call["toolCallId"], "call_1");
        assert_eq!(call["name"], "write_file");
        assert_eq!(call["kind"], "edit");
        assert_eq!(call["status"], "pending");
        assert_eq!(call["rawInput"]["path"], "a.txt");

        let request = &out[1];
        assert_eq!(request["id"], 0);
        assert_eq!(request["params"]["sessionId"], "sess_1");
        assert_eq!(request["params"]["toolCall"]["toolCallId"], "call_1");
        assert_eq!(
            request["params"]["toolCall"]["content"][0]["content"]["text"],
            "--- a/a.txt\n+++ b/a.txt"
        );
        let options = request["params"]["options"].as_array().unwrap();
        assert_eq!(options[0]["optionId"], ALLOW_ONCE);
        assert_eq!(options[0]["kind"], "allow_once");
        assert_eq!(options[1]["optionId"], REJECT_ONCE);
        assert_eq!(options[1]["kind"], "reject_once");

        let result = &out[2]["params"]["update"];
        assert_eq!(result["toolCallId"], "call_1");
        assert_eq!(result["status"], "completed");
        assert_eq!(result["content"][0]["content"]["text"], "Wrote 1 byte");

        assert_eq!(out[3]["result"]["stopReason"], "end_turn");
        assert_eq!(
            *server.backend.answers.lock().unwrap(),
            [Permission::Allowed]
        );
    }

    #[test]
    fn only_choosing_allow_once_allows() {
        let cases: [(&str, Reply); 6] = [
            ("reject", Box::new(|id| selected(id, REJECT_ONCE))),
            (
                "an option never offered",
                Box::new(|id| selected(id, "allow_always")),
            ),
            (
                "cancelled",
                Box::new(|id| {
                    json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"cancelled"}}})
                        .to_string()
                }),
            ),
            (
                "an error reply",
                Box::new(|id| {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32800,"message":"Cancelled"}})
                        .to_string()
                }),
            ),
            (
                "no outcome",
                Box::new(|id| json!({"jsonrpc":"2.0","id":id,"result":{}}).to_string()),
            ),
            (
                "selected with no option",
                Box::new(|id| {
                    json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"selected"}}})
                        .to_string()
                }),
            ),
        ];
        for (name, reply) in cases {
            let mut server = ready();
            let out = ask_and_answer(&mut server, reply);
            assert_eq!(
                *server.backend.answers.lock().unwrap(),
                [Permission::Declined],
                "{name}"
            );
            // The turn still finished and answered.
            assert_eq!(
                out.last().unwrap()["result"]["stopReason"],
                "end_turn",
                "{name}"
            );
        }
    }

    #[test]
    fn a_closed_connection_declines_and_the_connection_ends() {
        let mut server = ready();
        server.backend.acts = vec![Act::Ask(write_request()), Act::Ask(write_request())];
        let events = server.sender();
        let mut requests = 0;
        let out = {
            let mut out = Vec::new();
            server.handle_line(&prompt_line(HI), &mut |message| {
                if message["method"] == "session/request_permission" {
                    requests += 1;
                    events.send(Event::Eof).unwrap();
                }
                out.push(message);
            });
            out
        };
        // The first was asked; the client left; the second was never sent.
        assert_eq!(requests, 1);
        assert_eq!(
            *server.backend.answers.lock().unwrap(),
            [Permission::Declined, Permission::Declined]
        );
        assert!(server.closed);
        assert_eq!(out.last().unwrap()["id"], 5);
    }

    #[test]
    fn a_request_during_a_turn_waits_and_a_stray_response_is_ignored() {
        let mut server = ready();
        server.backend.acts = vec![Act::Ask(write_request())];
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "session/request_permission" {
                for line in [
                    // Another prompt while one runs.
                    r#"{"jsonrpc":"2.0","id":77,"method":"session/prompt","params":{}}"#.to_owned(),
                    // A response to nothing we asked.
                    selected(99, ALLOW_ONCE),
                    // A notification, and garbage.
                    r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"sess_1"}}"#
                        .to_owned(),
                    "{nope".to_owned(),
                    selected(0, ALLOW_ONCE),
                ] {
                    events.send(Event::Line(line)).unwrap();
                }
            }
            out.push(message);
        });
        let busy = out.iter().find(|m| m["id"] == 77).expect("a busy reply");
        assert_eq!(busy["error"]["code"], INVALID_REQUEST);
        assert!(out.iter().any(|m| m["error"]["code"] == PARSE_ERROR));
        // The stray response did not answer the request, the real one did.
        assert_eq!(
            *server.backend.answers.lock().unwrap(),
            [Permission::Allowed]
        );
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "end_turn");
        assert_eq!(server.backend.prompts.len(), 1, "no second turn started");
    }

    #[test]
    fn a_panicking_turn_is_an_error_and_not_a_hang() {
        let mut server = ready();
        server.backend.panic = true;
        let reply = one(&mut server, &prompt_line(HI));
        assert_eq!(reply["error"]["code"], INTERNAL_ERROR);
        assert_eq!(reply["error"]["message"], "the turn panicked");
    }

    #[test]
    fn a_failed_tool_call_is_reported_failed() {
        let update = session_update(
            "s",
            &Update::ToolResult {
                id: "c".into(),
                ok: false,
                text: "boom".into(),
            },
        );
        assert_eq!(update["params"]["update"]["status"], "failed");
    }

    #[test]
    fn stop_reasons_and_backend_errors_come_through() {
        let mut server = ready();
        for (stop, name) in [
            (Stop::MaxTurnRequests, "max_turn_requests"),
            (Stop::Cancelled, "cancelled"),
        ] {
            server.backend.stop = Some(Ok(stop));
            let reply = one(&mut server, &prompt_line(HI));
            assert_eq!(reply["result"]["stopReason"], name);
        }
        server.backend.stop = Some(Err(BackendError::internal("provider down")));
        let reply = one(&mut server, &prompt_line(HI));
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

    fn served(input: Vec<u8>) -> (io::Result<()>, Vec<Value>) {
        let mut output = Vec::new();
        let result = serve(Cursor::new(input), &mut output, Script::default());
        let text = String::from_utf8(output).unwrap();
        let out = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (result, out)
    }

    #[test]
    fn serve_writes_one_flushed_line_per_message_and_ends_at_end_of_input() {
        let input =
            format!("{INITIALIZE}\n\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}}\r\n");
        let (result, out) = served(input.into_bytes());
        result.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["id"], 0);
        assert_eq!(out[1]["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn serve_survives_a_last_line_without_a_newline_and_invalid_utf8() {
        let mut input = b"\xff\xfe\n".to_vec();
        input.extend_from_slice(INITIALIZE.as_bytes());
        let (result, out) = served(input);
        result.unwrap();
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
        let error = serve(Cursor::new(input.into_bytes()), Closed, Script::default()).unwrap_err();
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
