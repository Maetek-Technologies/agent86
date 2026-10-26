//! The MCP transports: newline-delimited JSON-RPC to a subprocess (`stdio`),
//! streamable HTTP (`http`), and the legacy HTTP+SSE transport (`sse`).
//!
//! Every transport hands what it reads to one [`Router`], which wakes the
//! request waiting for a response, answers server requests (`ping`), and
//! passes notifications on. A transport that ends (the subprocess exits, the
//! SSE stream closes) closes its router, which is how a drop is noticed.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Why a request got no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientError {
    /// The transport ended (the subprocess exited, the stream closed).
    Closed,
    Timeout,
    /// A JSON-RPC error response, or an HTTP failure reported against the request.
    Rpc(String),
    Io(String),
}

impl ClientError {
    /// As Python's `_describe(exc)`: `Type: message`.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Closed => "McpError: Connection closed".to_owned(),
            Self::Timeout => "TimeoutError".to_owned(),
            Self::Rpc(message) => format!("McpError: {message}"),
            Self::Io(message) => format!("OSError: {message}"),
        }
    }
}

type NotificationHandler = Box<dyn Fn(&str, &Value) + Send + Sync>;
type CloseHandler = Box<dyn Fn() + Send + Sync>;

/// Routes everything a transport reads.
#[derive(Default)]
pub(crate) struct Router {
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    closed: AtomicBool,
    /// Why the transport failed, when it knows (shown for a failed connect).
    last_error: Mutex<Option<String>>,
    on_notification: OnceLock<NotificationHandler>,
    on_close: OnceLock<CloseHandler>,
}

impl Router {
    pub(crate) fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Notifications that arrive before this is set are dropped, as Python
    /// ignores them until the session is live.
    pub(crate) fn set_notification_handler(&self, handler: NotificationHandler) {
        let _ = self.on_notification.set(handler);
    }

    /// Called once, when the transport ends, before waiting requests are woken.
    pub(crate) fn set_close_handler(&self, handler: CloseHandler) {
        let _ = self.on_close.set(handler);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn set_last_error(&self, error: String) {
        if let Ok(mut last) = self.last_error.lock() {
            last.get_or_insert(error);
        }
    }

    pub(crate) fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|last| last.clone())
    }

    fn expect(&self, id: u64) -> mpsc::Receiver<Value> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, sender);
        }
        receiver
    }

    fn forget(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    /// Route one incoming message (or batch) and return the replies the
    /// transport must send back: `ping` is answered, other server requests refused.
    pub(crate) fn deliver(&self, message: Value) -> Vec<Value> {
        let messages = match message {
            Value::Array(batch) => batch,
            single => vec![single],
        };
        let mut replies = Vec::new();
        for message in messages {
            let id = message.get("id").cloned().filter(|id| !id.is_null());
            match (message.get("method").and_then(Value::as_str), id) {
                (None, Some(id)) => {
                    let sender = id.as_u64().and_then(|id| {
                        self.pending
                            .lock()
                            .ok()
                            .and_then(|mut pending| pending.remove(&id))
                    });
                    if let Some(sender) = sender {
                        let _ = sender.send(message);
                    }
                }
                (Some(method), Some(id)) => replies.push(if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
                }),
                (Some(method), None) => {
                    if let Some(handler) = self.on_notification.get() {
                        let params = message.get("params").cloned().unwrap_or(Value::Null);
                        handler(method, &params);
                    }
                }
                (None, None) => {}
            }
        }
        replies
    }

    /// Fail a request without a response from the server (an HTTP error on its POST).
    fn fail(&self, id: Option<&Value>, message: String) {
        if let Some(id) = id {
            self.deliver(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}}),
            );
        }
    }

    /// The transport ended. Idempotent.
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(handler) = self.on_close.get() {
            handler();
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear(); // dropping the senders wakes every waiting request
        }
    }
}

/// Sends messages to one server.
pub(crate) trait Transport: Send + Sync {
    fn send(&self, message: &Value) -> Result<(), ClientError>;
    /// Stop the server or close the connection.
    fn shutdown(&self);
    /// The last lines a stdio server wrote to stderr, secrets redacted.
    fn stderr_tail(&self, _max_lines: usize, _max_bytes: u64) -> String {
        String::new()
    }
}

/// A JSON-RPC client over any transport.
pub(crate) struct Client {
    transport: Box<dyn Transport>,
    router: Arc<Router>,
    next_id: AtomicU64,
}

impl Client {
    pub(crate) fn new(transport: Box<dyn Transport>, router: Arc<Router>) -> Self {
        Self {
            transport,
            router,
            next_id: AtomicU64::new(1),
        }
    }

    /// Send a request and wait up to `timeout` for its result. With `progress`,
    /// the request carries a progress token (its own id), as the Python SDK mints
    /// one when a progress callback is passed.
    pub(crate) fn request(
        &self,
        method: &str,
        params: Option<Value>,
        progress: bool,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        if self.router.is_closed() {
            return Err(ClientError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        let mut params = params;
        if progress {
            let object = params.get_or_insert_with(|| json!({}));
            if let Some(object) = object.as_object_mut() {
                object.insert("_meta".to_owned(), json!({ "progressToken": id }));
            }
        }
        if let Some(params) = params {
            message["params"] = params;
        }
        let receiver = self.router.expect(id);
        if let Err(error) = self.transport.send(&message) {
            self.router.forget(id);
            return Err(error);
        }
        match receiver.recv_timeout(timeout) {
            Ok(response) => match response.get("error") {
                Some(error) => Err(ClientError::Rpc(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned(),
                )),
                None => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
            },
            Err(RecvTimeoutError::Timeout) => {
                self.router.forget(id);
                Err(ClientError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(ClientError::Closed),
        }
    }

    pub(crate) fn notify(&self, method: &str) -> Result<(), ClientError> {
        self.transport
            .send(&json!({"jsonrpc": "2.0", "method": method}))
    }

    pub(crate) fn stderr_tail(&self, max_lines: usize, max_bytes: u64) -> String {
        self.transport.stderr_tail(max_lines, max_bytes)
    }

    pub(crate) fn shutdown(&self) {
        self.transport.shutdown();
    }
}

// --------------------------------------------------------------------------
// stdio
// --------------------------------------------------------------------------

/// The server's stdin, shared with the reader thread (which answers `ping`).
type Writer = Arc<Mutex<Option<ChildStdin>>>;

/// One running stdio MCP server process.
pub(crate) struct StdioTransport {
    child: Mutex<Child>,
    stdin: Writer,
    router: Arc<Router>,
    stderr_path: PathBuf,
}

impl StdioTransport {
    pub(crate) fn spawn(
        command: &str,
        args: &[String],
        env: BTreeMap<OsString, OsString>,
        router: Arc<Router>,
    ) -> Result<Self, String> {
        let stderr_path = std::env::temp_dir().join(format!(
            "agent86-mcp-{}-{}.stderr",
            std::process::id(),
            crate::turn::new_session_id()
        ));
        let stderr = std::fs::File::create(&stderr_path).map_err(|error| error.to_string())?;
        let mut child = Command::new(resolve_command(command, &env))
            .args(args)
            .env_clear()
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| error.to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or("the server's stdout was not captured")?;
        let stdin: Writer = Arc::new(Mutex::new(child.stdin.take()));
        let transport = Self {
            child: Mutex::new(child),
            stdin: Arc::clone(&stdin),
            router: Arc::clone(&router),
            stderr_path,
        };
        std::thread::spawn(move || read_lines(stdout, &router, &stdin));
        Ok(transport)
    }
}

fn write_line(writer: &Writer, message: &Value) -> std::io::Result<()> {
    let mut guard = writer
        .lock()
        .map_err(|_| std::io::Error::other("stdin lock poisoned"))?;
    let stdin = guard
        .as_mut()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
    stdin.write_all(format!("{message}\n").as_bytes())?;
    stdin.flush()
}

fn read_lines(stdout: impl Read, router: &Router, writer: &Writer) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue; // a malformed line is not a death
        };
        for reply in router.deliver(message) {
            let _ = write_line(writer, &reply);
        }
    }
    router.close();
}

impl Transport for StdioTransport {
    fn send(&self, message: &Value) -> Result<(), ClientError> {
        write_line(&self.stdin, message).map_err(|error| {
            if self.router.is_closed() || error.kind() == std::io::ErrorKind::BrokenPipe {
                ClientError::Closed
            } else {
                ClientError::Io(error.to_string())
            }
        })
    }

    /// As Python's `_StderrCapture.tail`: the last lines, a partial first line dropped.
    fn stderr_tail(&self, max_lines: usize, max_bytes: u64) -> String {
        let Ok(mut file) = std::fs::File::open(&self.stderr_path) else {
            return String::new();
        };
        let size = file.metadata().map_or(0, |metadata| metadata.len());
        let start = size.saturating_sub(max_bytes);
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut lines: Vec<&str> = crate::difflib::splitlines(&text);
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        let tail = lines[lines.len().saturating_sub(max_lines)..].join("\n");
        crate::guardrails::redact(&tail).0
    }

    /// Close stdin, give the server a moment to exit, then kill it.
    fn shutdown(&self) {
        if let Ok(mut stdin) = self.stdin.lock() {
            stdin.take();
        }
        if let Ok(mut child) = self.child.lock() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

/// On Windows, find `command` the way `shutil.which` does (honouring PATHEXT),
/// so `npx` resolves to `npx.cmd` as it does for Python's MCP SDK.
fn resolve_command(command: &str, env: &BTreeMap<OsString, OsString>) -> PathBuf {
    if !cfg!(windows)
        || std::path::Path::new(command).extension().is_some()
        || command.contains(['/', '\\'])
    {
        return PathBuf::from(command);
    }
    let lookup = |name: &str| {
        env.get(&OsString::from(name))
            .cloned()
            .or_else(|| std::env::var_os(name))
    };
    let extensions = lookup("PATHEXT")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned());
    if let Some(path) = lookup("PATH") {
        for dir in std::env::split_paths(&path) {
            for extension in extensions.split(';').filter(|e| !e.is_empty()) {
                let candidate = dir.join(format!("{command}{}", extension.to_lowercase()));
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from(command)
}

// --------------------------------------------------------------------------
// Server-sent events
// --------------------------------------------------------------------------

/// An incremental `text/event-stream` parser yielding `(event, data)` pairs.
#[derive(Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let event = if self.event.is_empty() {
                        "message".to_owned()
                    } else {
                        std::mem::take(&mut self.event)
                    };
                    events.push((event, self.data.join("\n")));
                }
                self.event.clear();
                self.data.clear();
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = value.to_owned(),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        events
    }
}

/// Deliver every JSON message in an SSE body to `router`, posting replies via `outgoing`.
async fn pump_events(
    mut response: reqwest::Response,
    router: &Router,
    outgoing: &tokio::sync::mpsc::UnboundedSender<Outgoing>,
    mut on_event: impl FnMut(&str, &str) -> bool,
) -> Result<(), String> {
    let mut parser = SseParser::default();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                for (event, data) in parser.feed(&chunk) {
                    if !on_event(&event, &data) {
                        continue;
                    }
                    if event != "message" {
                        continue;
                    }
                    if let Ok(message) = serde_json::from_str::<Value>(&data) {
                        for reply in router.deliver(message) {
                            let _ = outgoing.send(Outgoing::Message(reply));
                        }
                    }
                }
            }
            Ok(None) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
    }
}

// --------------------------------------------------------------------------
// HTTP transports
// --------------------------------------------------------------------------

pub(crate) enum Outgoing {
    Message(Value),
    Shutdown,
}

/// Either HTTP transport: a thread running its own runtime, fed through a channel,
/// so sending never needs an async context.
pub(crate) struct HttpTransport {
    outgoing: tokio::sync::mpsc::UnboundedSender<Outgoing>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    router: Arc<Router>,
}

fn header_map(headers: &indexmap::IndexMap<String, String>) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name '{name}'"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| format!("invalid value for header '{name}'"))?;
        map.insert(name, value);
    }
    Ok(map)
}

fn http_client(headers: HeaderMap) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())
}

impl HttpTransport {
    /// Streamable HTTP: every message is POSTed; responses come back as JSON or
    /// an SSE body, and server-initiated messages on an optional GET stream.
    pub(crate) fn streamable(
        url: &str,
        headers: &indexmap::IndexMap<String, String>,
        router: Arc<Router>,
    ) -> Result<Self, String> {
        let client = http_client(header_map(headers)?)?;
        let url = reqwest::Url::parse(url).map_err(|error| format!("invalid url: {error}"))?;
        Self::start(router, move |receiver, sender, router| {
            streamable_loop(client, url, receiver, sender, router)
        })
    }

    /// The legacy HTTP+SSE transport: a GET stream announces the endpoint to
    /// POST to, and carries every response.
    pub(crate) fn sse(
        url: &str,
        headers: &indexmap::IndexMap<String, String>,
        router: Arc<Router>,
    ) -> Result<Self, String> {
        let client = http_client(header_map(headers)?)?;
        let url = reqwest::Url::parse(url).map_err(|error| format!("invalid url: {error}"))?;
        Self::start(router, move |receiver, sender, router| {
            sse_loop(client, url, receiver, sender, router)
        })
    }

    fn start<F, Fut>(router: Arc<Router>, run: F) -> Result<Self, String>
    where
        F: FnOnce(
                tokio::sync::mpsc::UnboundedReceiver<Outgoing>,
                tokio::sync::mpsc::UnboundedSender<Outgoing>,
                Arc<Router>,
            ) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let loop_sender = sender.clone();
        let loop_router = Arc::clone(&router);
        let thread = std::thread::spawn(move || {
            runtime.block_on(run(receiver, loop_sender, Arc::clone(&loop_router)));
            loop_router.close();
        });
        Ok(Self {
            outgoing: sender,
            thread: Mutex::new(Some(thread)),
            router,
        })
    }
}

impl Transport for HttpTransport {
    fn send(&self, message: &Value) -> Result<(), ClientError> {
        if self.router.is_closed() {
            return Err(ClientError::Closed);
        }
        self.outgoing
            .send(Outgoing::Message(message.clone()))
            .map_err(|_| ClientError::Closed)
    }

    fn shutdown(&self) {
        let _ = self.outgoing.send(Outgoing::Shutdown);
        let thread = self.thread.lock().ok().and_then(|mut thread| thread.take());
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct HttpSession {
    id: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    listening: AtomicBool,
}

impl HttpSession {
    fn headers(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(id) = self.id.lock().ok().and_then(|id| id.clone()) {
            request = request.header("mcp-session-id", id);
        }
        if let Some(version) = self
            .protocol
            .lock()
            .ok()
            .and_then(|version| version.clone())
        {
            request = request.header("mcp-protocol-version", version);
        }
        request
    }
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    content_type(response).starts_with("text/event-stream")
}

fn content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

async fn streamable_loop(
    client: reqwest::Client,
    url: reqwest::Url,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<Outgoing>,
    sender: tokio::sync::mpsc::UnboundedSender<Outgoing>,
    router: Arc<Router>,
) {
    let session = Arc::new(HttpSession::default());
    while let Some(item) = receiver.recv().await {
        let Outgoing::Message(message) = item else {
            break;
        };
        let (client, url, sender, router, session) = (
            client.clone(),
            url.clone(),
            sender.clone(),
            Arc::clone(&router),
            Arc::clone(&session),
        );
        tokio::spawn(async move {
            post_streamable(&client, &url, message, &sender, &router, &session).await;
        });
    }
    // Terminate the server-side session, as the Python SDK does on close.
    let id = session.id.lock().ok().and_then(|id| id.clone());
    if let Some(id) = id {
        let _ = client
            .delete(url)
            .header("mcp-session-id", id)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    }
}

async fn post_streamable(
    client: &reqwest::Client,
    url: &reqwest::Url,
    message: Value,
    sender: &tokio::sync::mpsc::UnboundedSender<Outgoing>,
    router: &Arc<Router>,
    session: &Arc<HttpSession>,
) {
    let id = message
        .get("id")
        .filter(|_| message.get("method").is_some());
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let request = session.headers(
        client
            .post(url.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(message.to_string()),
    );
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return router.fail(id, error.to_string()),
    };
    if method == "initialize" {
        if let Some(value) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            if let Ok(mut slot) = session.id.lock() {
                *slot = Some(value.to_owned());
            }
        }
    }
    let status = response.status();
    if !status.is_success() {
        let detail = if status == reqwest::StatusCode::NOT_FOUND
            && session.id.lock().is_ok_and(|id| id.is_some())
        {
            "Session terminated".to_owned()
        } else {
            format!("HTTP {status} from {url}")
        };
        return router.fail(id, detail);
    }
    if method == "notifications/initialized" && !session.listening.swap(true, Ordering::SeqCst) {
        let (client, url, sender, session) = (
            client.clone(),
            url.clone(),
            sender.clone(),
            Arc::clone(session),
        );
        let router = Arc::clone(router);
        tokio::spawn(async move {
            listen_streamable(&client, &url, &sender, &router, &session).await;
        });
    }
    if status == reqwest::StatusCode::ACCEPTED || id.is_none() {
        return;
    }
    if is_event_stream(&response) {
        if let Err(error) = pump_events(response, router, sender, |_, _| true).await {
            router.fail(id, error);
        }
        return;
    }
    if !content_type(&response).starts_with("application/json") {
        let kind = content_type(&response);
        return router.fail(id, format!("Unexpected content type: {kind}"));
    }
    match response.bytes().await {
        Ok(body) => match serde_json::from_slice::<Value>(&body) {
            Ok(reply) => {
                if method == "initialize" {
                    remember_protocol(session, &reply);
                }
                for answer in router.deliver(reply) {
                    let _ = sender.send(Outgoing::Message(answer));
                }
            }
            Err(error) => router.fail(id, format!("invalid JSON response: {error}")),
        },
        Err(error) => router.fail(id, error.to_string()),
    }
}

fn remember_protocol(session: &HttpSession, reply: &Value) {
    if let Some(version) = reply
        .pointer("/result/protocolVersion")
        .and_then(Value::as_str)
    {
        if let Ok(mut slot) = session.protocol.lock() {
            *slot = Some(version.to_owned());
        }
    }
}

/// The optional GET stream for server-initiated messages. A server that does
/// not offer one (405, or any failure) simply sends nothing this way.
async fn listen_streamable(
    client: &reqwest::Client,
    url: &reqwest::Url,
    sender: &tokio::sync::mpsc::UnboundedSender<Outgoing>,
    router: &Router,
    session: &HttpSession,
) {
    let request = session.headers(client.get(url.clone()).header(ACCEPT, "text/event-stream"));
    if let Ok(response) = request.send().await {
        if response.status().is_success() && is_event_stream(&response) {
            let _ = pump_events(response, router, sender, |_, _| true).await;
        }
    }
}

async fn sse_loop(
    client: reqwest::Client,
    url: reqwest::Url,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<Outgoing>,
    sender: tokio::sync::mpsc::UnboundedSender<Outgoing>,
    router: Arc<Router>,
) {
    let (endpoint_tx, mut endpoint_rx) = tokio::sync::watch::channel(None::<reqwest::Url>);
    let reader_router = Arc::clone(&router);
    let reader_client = client.clone();
    let reader_url = url.clone();
    let reader_sender = sender.clone();
    let reader = tokio::spawn(async move {
        let router = reader_router;
        let response = match reader_client
            .get(reader_url.clone())
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                router.set_last_error(format!("HTTP {} from {reader_url}", response.status()));
                return router.close();
            }
            Err(error) => {
                router.set_last_error(error.to_string());
                return router.close();
            }
        };
        let base = reader_url.clone();
        let outcome = pump_events(response, &router, &reader_sender, |event, data| {
            if event != "endpoint" {
                return true;
            }
            match base.join(data.trim()) {
                Ok(endpoint)
                    if endpoint.scheme() == base.scheme()
                        && endpoint.host_str() == base.host_str()
                        && endpoint.port_or_known_default() == base.port_or_known_default() =>
                {
                    let _ = endpoint_tx.send(Some(endpoint));
                }
                _ => {
                    router.set_last_error(format!(
                        "Endpoint origin does not match connection origin: {}",
                        data.trim()
                    ));
                    router.close();
                }
            }
            false
        })
        .await;
        if let Err(error) = outcome {
            router.set_last_error(error);
        }
        router.close();
    });

    while let Some(item) = receiver.recv().await {
        let Outgoing::Message(message) = item else {
            break;
        };
        let endpoint = match endpoint_rx.wait_for(Option::is_some).await {
            Ok(endpoint) => endpoint.clone().unwrap_or_else(|| url.clone()),
            Err(_) => break,
        };
        let (client, router) = (client.clone(), Arc::clone(&router));
        tokio::spawn(async move {
            let id = message
                .get("id")
                .filter(|_| message.get("method").is_some());
            let sent = client
                .post(endpoint)
                .header(CONTENT_TYPE, "application/json")
                .body(message.to_string())
                .send()
                .await;
            match sent {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => router.fail(id, format!("HTTP {}", response.status())),
                Err(error) => router.fail(id, error.to_string()),
            }
        });
    }
    reader.abort();
}
