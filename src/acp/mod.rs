//! The Agent Client Protocol server, beyond Python (`agent86 acp`).
//!
//! ACP lets an editor launch an agent as a subprocess and talk JSON-RPC 2.0 to it
//! over stdio: one message per line, UTF-8, no embedded newlines. The agent must
//! write nothing to stdout that is not a protocol message, so logging goes to
//! stderr and nothing else in this process may print to stdout in this mode.
//!
//! Built so far (slices 1 to 5 of the plan): the transport, the `initialize`
//! handshake, `session/new`, `session/prompt` with streamed text and reasoning, tool
//! calls reported as they happen, `session/request_permission` for the calls the
//! approval mode asks about, `session/cancel`, `session/load` with the history
//! replayed (and, since 1.0.21, `session/resume` without it), the MCP servers the
//! editor names, and the editor's own files (`fs/read_text_file` and
//! `fs/write_text_file`) for the file tools when it offers them. Slice 6 is
//! documentation, so for now every other method answers "method not
//! found" rather than pretending, and the capabilities advertised are only what is
//! built.
//!
//! Threads, as in the interactive interface: a thread of its own reads stdin and
//! hands every line over a channel; a prompt's turn runs on a worker thread; and the
//! connection's thread pumps both, writing what the turn reports and passing the
//! editor's replies to whoever waits for them. A permission request is the case that
//! needs it: the turn blocks on the editor's answer while the connection keeps
//! reading, and `session/cancel` is acted on at once. While a turn runs, a second
//! request is refused (one turn at a time).
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
use std::time::Duration;

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
/// ACP's "resource not found": a session that does not exist.
pub const RESOURCE_NOT_FOUND: i64 = -32002;

/// agent86's extension namespace (beyond Python): the spec asks for reverse-DNS
/// names from a domain the implementer controls, `<ns>/<name>` in `_meta` and
/// `_<ns>/<name>` for methods (`docs/protocol/v1/extensibility.mdx`).
pub const EXTENSION_NAMESPACE: &str = "com.maetek.agent86";
/// The capability a client names in `clientCapabilities._meta` to receive the
/// harness's state, and the agent advertises in `agentCapabilities._meta`.
pub const STATE_CAPABILITY: &str = "com.maetek.agent86/state";
/// The notification that carries one state event to a client that asked.
pub const STATE_METHOD: &str = "_com.maetek.agent86/session/event";

/// The `_meta` key of a permission request that a sub-agent asks, for a client
/// with [`STATE_CAPABILITY`]: which sub-agent, where it works, and what it has
/// spent (beyond Python).
pub const ASKER_META: &str = "com.maetek.agent86/subagent";
/// The version of the state events' shape; a change that breaks a reader bumps it.
pub const STATE_VERSION: u64 = 1;

/// The capability the agent advertises in `agentCapabilities._meta` for its
/// memory methods (beyond Python): list, pin, and forget single facts and
/// episodes. A client calls them when it wants to; nothing is sent unasked, so
/// no client capability is needed.
pub const MEMORY_CAPABILITY: &str = "com.maetek.agent86/memory";
/// The version of the memory methods' shapes.
pub const MEMORY_VERSION: u64 = 1;
/// `{sessionId, kind?, limit?, cursor?, pinned?}` to `{entries, nextCursor?}`.
pub const MEMORY_LIST_METHOD: &str = "_com.maetek.agent86/memory/list";
/// `{sessionId, kind?, id, pinned?}` to `{entry}`.
pub const MEMORY_PIN_METHOD: &str = "_com.maetek.agent86/memory/pin";
/// `{sessionId, kind?, id}` to `{}`.
pub const MEMORY_FORGET_METHOD: &str = "_com.maetek.agent86/memory/forget";
/// Entries per `memory/list` page when the client names no `limit`, and the most
/// it may name.
pub const MEMORY_PAGE: usize = 50;
pub const MEMORY_PAGE_MAX: usize = 200;

/// The capability for handing agent86 secrets (beyond Python). The agent
/// advertises it in `agentCapabilities._meta` for the three requests below, which
/// any client may make. A client names it in `clientCapabilities._meta` (`true` or
/// an object) to say it draws its own masked entry and keeps what is typed there
/// out of its thread, its history, and its logs; only such a client is sent
/// [`SECRET_REQUEST_METHOD`] and offered `request_secret`.
pub const SECRETS_CAPABILITY: &str = "com.maetek.agent86/secrets";
/// The version of the secret methods' shapes.
pub const SECRETS_VERSION: u64 = 1;
/// `{sessionId, name, value, remember?}` to `{name, remembered, warning?}`.
pub const SECRET_SET_METHOD: &str = "_com.maetek.agent86/secret/set";
/// `{sessionId}` to `{secrets: [{name, remembered}], canRemember}`.
pub const SECRET_LIST_METHOD: &str = "_com.maetek.agent86/secret/list";
/// `{sessionId, name}` to `{forgotten, warning?}`.
pub const SECRET_FORGET_METHOD: &str = "_com.maetek.agent86/secret/forget";
/// Agent to client: `{sessionId, name, reason, replacing, canRemember,
/// remembered}` to `{outcome: "entered", value, remember}` or
/// `{outcome: "declined"}`.
pub const SECRET_REQUEST_METHOD: &str = "_com.maetek.agent86/secret/request";

/// The capability for agent86UI, A2UI 1.0 surfaces the model draws (beyond
/// Python; ADR 0005). The agent advertises it in `agentCapabilities._meta` with
/// the catalogs it draws with. A client names it in `clientCapabilities._meta`
/// as an object whose `supportedCatalogIds` holds the basic catalog's id (the
/// only one agent86 draws with) to say it renders surfaces; only such a client
/// is offered `render_ui` and sent [`A2UI_RENDER_METHOD`] and [`A2UI_WAIT_METHOD`].
pub const A2UI_CAPABILITY: &str = "com.maetek.agent86/a2ui";
/// The version of the a2ui methods' shapes.
pub const A2UI_VERSION: u64 = 1;
/// Agent to client, a notification: `{sessionId, messages}`, A2UI 1.0 messages
/// for the client's renderer, already validated, in order.
pub const A2UI_RENDER_METHOD: &str = "_com.maetek.agent86/a2ui/render";
/// Agent to client, while a turn waits for the user's answer to surfaces it drew:
/// `{sessionId, surfaceIds}` to `{outcome: "action", action}` (an A2UI `action`
/// from one of those surfaces) or `{outcome: "dismissed"}`.
pub const A2UI_WAIT_METHOD: &str = "_com.maetek.agent86/a2ui/wait";
/// Client to agent, between turns: `{sessionId, action}` (an A2UI `action` on a
/// surface no call waited on) runs a turn on the action, as `session/prompt`
/// does, to `{stopReason}`. Agent functions (`callAgentFunction`) are refused.
pub const A2UI_ACTION_METHOD: &str = "_com.maetek.agent86/a2ui/action";

/// The capability for `ask_user` answered by the client's own form (beyond
/// Python; ADR 0006). The agent advertises it in `agentCapabilities._meta`; a
/// client names it in `clientCapabilities._meta` (`true` or an object) to draw
/// the questions itself, and is then sent [`QUESTIONS_ASK_METHOD`].
pub const QUESTIONS_CAPABILITY: &str = "com.maetek.agent86/questions";
/// The version of the questions method's shapes.
pub const QUESTIONS_VERSION: u64 = 1;
/// Agent to client, while a turn waits: `{sessionId, title?, questions}` (the
/// `ask_user` tool's own shape: each question's `id`, `question`, `kind`, and
/// `options` and `optional` where given) to `{answers: {<id>: value}}` or
/// `{dismissed: true}`.
pub const QUESTIONS_ASK_METHOD: &str = "_com.maetek.agent86/questions/ask";

/// One secret method's request, checked (beyond Python). `Debug` never shows a
/// value: [`crate::vault::SecretAnswer`] hides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretMethod {
    Set {
        name: String,
        answer: crate::vault::SecretAnswer,
    },
    List,
    Forget {
        name: String,
    },
}

/// What a secret method did. Never holds a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretReply {
    Set(crate::vault::Stored),
    /// Each name held and whether it is remembered for the project, and whether
    /// a keyring is there to remember one in.
    List {
        secrets: Vec<(String, bool)>,
        can_remember: bool,
    },
    Forgotten {
        forgotten: bool,
        warning: Option<String>,
    },
}

/// One memory method's request, checked (beyond Python).
#[derive(Debug, Clone, Copy)]
pub enum MemoryRequest {
    List(crate::memory::ListQuery),
    Pin {
        kind: crate::memory::EntryKind,
        id: i64,
        pinned: bool,
    },
    Forget {
        kind: crate::memory::EntryKind,
        id: i64,
    },
}

/// What a memory method found; text already redacted by the backend.
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryReply {
    Entries(Vec<crate::memory::Entry>),
    Entry(crate::memory::Entry),
    Forgotten,
}

/// The ids of the two options a permission request offers. Only the allowing one
/// approves: `allow-once` for a tool call, or, for a project specialty's file,
/// `allow-always` (beyond Python), since agent86 remembers that answer by the
/// file's hash. The agent keeps no other remembered decisions.
pub const ALLOW_ONCE: &str = "allow-once";
pub const ALLOW_ALWAYS: &str = "allow-always";
pub const REJECT_ONCE: &str = "reject-once";

/// The category of a tool call, which an editor uses to pick an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Search,
    Execute,
    Fetch,
    Other,
}

impl ToolKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Search => "search",
            Self::Execute => "execute",
            Self::Fetch => "fetch",
            Self::Other => "other",
        }
    }
}

/// A file a tool call reads or changes (ACP's `ToolCallLocation`), so an editor can
/// follow along (beyond Python).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Absolute.
    pub path: String,
    /// 1-based: the first changed line, for an edit.
    pub line: Option<usize>,
}

pub use crate::tools::FileChange;

/// Something the agent reports while a prompt runs, sent as a `session/update`.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// A chunk of the user's message (`user_message_chunk`), when a loaded session's
    /// history is replayed.
    User(String),
    /// A chunk of the agent's reply (`agent_message_chunk`). The harness's own
    /// notices (`[cancelled]`, `[guardrail] ...`) travel here too.
    Message(String),
    /// A chunk of the model's reasoning (`agent_thought_chunk`).
    Thought(String),
    /// The slash commands the session offers (`available_commands_update`).
    Commands(Vec<CommandInfo>),
    /// The session's settings changed (`config_option_update`).
    Config(Vec<ConfigOption>),
    /// The session was renamed (`session_info_update` with its `title`).
    Title(String),
    /// How full the context window is, and what the session has cost
    /// (`usage_update`, beyond Python): `used` and `size` in tokens, `cost` the
    /// session's cumulative US dollars, `None` for a model with no price.
    Usage {
        used: u64,
        size: u64,
        cost: Option<f64>,
    },
    /// One of the harness's state events (beyond Python), sent as [`STATE_METHOD`]
    /// rather than `session/update`, and only to a client that asked for them.
    State(Value),
    /// A2UI messages for the client's renderer (beyond Python), sent as
    /// [`A2UI_RENDER_METHOD`] rather than `session/update`, and only to a client
    /// with [`A2UI_CAPABILITY`], the only one offered `render_ui`.
    Ui(Vec<Value>),
    /// The history was compacted (`compaction_update`, beyond Python), for a client
    /// with the `session.compaction` capability: `completed` with the summary, or
    /// `failed` with why (the history was trimmed instead).
    Compaction {
        id: String,
        summary: Option<String>,
        error: Option<String>,
    },
    /// The model asked for a tool call (`tool_call`, status `pending`).
    ToolCall {
        id: String,
        title: String,
        name: String,
        kind: ToolKind,
        raw_input: Value,
        /// The files it touches, when known before it runs (beyond Python).
        locations: Vec<Location>,
    },
    /// A tool call's outcome (`tool_call_update`, status `completed` or `failed`),
    /// with the text the model is shown, and, for a file write, the change as a
    /// `diff` content item ahead of it, and where it changed (beyond Python).
    ToolResult {
        id: String,
        ok: bool,
        text: String,
        diff: Option<FileChange>,
        locations: Vec<Location>,
    },
    /// A running tool call's progress (`tool_call_update`, status `in_progress`),
    /// whose text replaces the call's content: a sub-agent's log on its
    /// delegation's card (beyond Python).
    ToolProgress { id: String, text: String },
}

/// A slash command the agent offers the editor to list, without the slash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    /// What to show while the user has typed no argument; `None` for a command that
    /// takes none.
    pub hint: Option<String>,
}

/// A session setting the editor shows as a selector (`configOptions`): the model
/// and the approval mode. Every one is a `select`; the boolean kind needs a client
/// capability this server does not ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOption {
    /// What `session/set_config_option` names it by.
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `model` or `mode`: the spec's reserved categories, which help the editor
    /// place the selector.
    pub category: &'static str,
    /// The value in force. Always one of `choices`.
    pub current: String,
    /// Every `(value, label)` pair, flat: what a value is checked against.
    pub choices: Vec<(String, String)>,
    /// The same choices under headings, `(heading, pairs)`, for a long list. When
    /// there are groups the editor is sent these instead of `choices`, and they must
    /// hold exactly the same values.
    pub groups: Vec<(String, Vec<(String, String)>)>,
}

/// One saved session, as `session/list` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    /// Absolute: where the session works.
    pub cwd: String,
    /// Whether `cwd` is the session's recorded directory. When it is not (the
    /// session was saved before directories were recorded), `cwd` is a stand-in the
    /// schema requires, and the entry says so with
    /// `_meta: {"agent86": {"cwdKnown": false}}`. Filtered lists never hold one.
    pub cwd_known: bool,
    pub title: Option<String>,
    /// Seconds since the Unix epoch.
    pub updated_at: f64,
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
    /// For a file write, the change as a `diff` content item, sent ahead of `text`
    /// (beyond Python).
    pub diff: Option<FileChange>,
    /// The files the call touches (beyond Python).
    pub locations: Vec<Location>,
    /// For a question whose answer the agent remembers (a project specialty's file),
    /// the name of the one allowing option, offered as `allow_always` in place of
    /// `allow_once`.
    pub remember: Option<String>,
    /// For a client with [`STATE_CAPABILITY`], the sub-agent asking, sent as the
    /// request's `_meta[`[`ASKER_META`]`]` (beyond Python). Already redacted.
    pub asker: Option<Value>,
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

    /// Call a method on the editor and wait for its result. `Err` carries the
    /// editor's error message, or why there was no answer (no reply within
    /// `timeout`, the turn cancelled, the connection gone). A request with no
    /// `timeout` waits as long as the editor takes.
    fn call(&self, method: &str, params: Value, timeout: Option<Duration>)
        -> Result<Value, String>;
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

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: RESOURCE_NOT_FOUND,
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

/// An MCP server the client asked the agent to connect to, as `session/new` and
/// `session/load` name it. Values are literal: the client wrote them, and
/// `${VAR}` in one is not a reference to the agent's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSpec {
    /// A local subprocess speaking MCP on its stdin and stdout.
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// A remote server over streamable HTTP, or over the older SSE transport.
    Remote {
        name: String,
        url: String,
        sse: bool,
        headers: Vec<(String, String)>,
    },
}

impl McpSpec {
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Remote { name, .. } => name,
        }
    }
}

/// What the client said it can do, from `initialize`. The agent must not call a
/// file method the client did not advertise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    /// `fs/read_text_file` is available.
    pub read_text_file: bool,
    /// `fs/write_text_file` is available.
    pub write_text_file: bool,
    /// `session.compaction` was given (an object): the client takes
    /// `compaction_update`, which the spec forbids sending otherwise.
    pub compaction: bool,
    /// `_meta["com.maetek.agent86/state"]` was given (`true` or an object): the
    /// client wants agent86's state events ([`STATE_METHOD`], beyond Python).
    pub state: bool,
    /// `_meta["com.maetek.agent86/secrets"]` was given (`true` or an object): the
    /// client draws its own masked entry, so it is asked for secrets
    /// ([`SECRET_REQUEST_METHOD`], beyond Python).
    pub secrets: bool,
    /// `_meta["com.maetek.agent86/a2ui"]` was given as an object whose
    /// `supportedCatalogIds` holds the basic catalog's id: the client renders
    /// agent86UI surfaces ([`A2UI_CAPABILITY`], beyond Python).
    pub a2ui: bool,
    /// `_meta["com.maetek.agent86/questions"]` was given (`true` or an object):
    /// the client draws `ask_user`'s questions itself ([`QUESTIONS_ASK_METHOD`],
    /// beyond Python).
    pub questions: bool,
}

impl ClientCapabilities {
    fn from_params(params: &Value) -> Self {
        let capabilities = params.get("clientCapabilities");
        let fs = capabilities.and_then(|capabilities| capabilities.get("fs"));
        let flag =
            |name: &str| fs.and_then(|fs| fs.get(name)).and_then(Value::as_bool) == Some(true);
        // Omitted, or `null`, at either level means unsupported.
        let compaction = capabilities
            .and_then(|capabilities| capabilities.get("session"))
            .and_then(|session| session.get("compaction"))
            .is_some_and(Value::is_object);
        let extension = |name: &str| {
            capabilities
                .and_then(|capabilities| capabilities.get("_meta"))
                .and_then(|meta| meta.get(name))
                .is_some_and(|value| value.is_object() || value == &Value::Bool(true))
        };
        let basic = crate::sdk::Catalog::basic().id();
        let a2ui = capabilities
            .and_then(|capabilities| capabilities.get("_meta"))
            .and_then(|meta| meta.get(A2UI_CAPABILITY))
            .and_then(|a2ui| a2ui.get("supportedCatalogIds"))
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(basic)));
        Self {
            read_text_file: flag("readTextFile"),
            write_text_file: flag("writeTextFile"),
            compaction,
            state: extension(STATE_CAPABILITY),
            secrets: extension(SECRETS_CAPABILITY),
            a2ui,
            questions: extension(QUESTIONS_CAPABILITY),
        }
    }
}

/// Tells the connection that a session's config options changed on their own (a
/// model list arrived), so it can send them again. Callable from any thread.
pub type Notifier = Arc<dyn Fn(&str) + Send + Sync>;

/// Stops a running turn; see [`Backend::cancel_handle`].
pub type CancelHandle = Arc<dyn Fn() + Send + Sync>;

/// The agent behind the protocol. `Send`, because a turn runs on a worker thread.
pub trait Backend: Send {
    /// What the client can do, told once after a successful `initialize`, before any
    /// session opens. A backend that has no use for it need not override this.
    fn client_capabilities(&mut self, _capabilities: ClientCapabilities) {}

    /// Open a session jailed to `cwd` (an absolute path) and return its id.
    /// `mcp_servers` are the servers the client asked the agent to connect to.
    fn new_session(
        &mut self,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
    ) -> Result<String, BackendError>;

    /// The slash commands offered to the editor for `session_id`, which may differ
    /// between sessions (a session's skills depend on its folder). None by default.
    fn commands(&self, _session_id: &str) -> Vec<CommandInfo> {
        Vec::new()
    }

    /// Run `line` (starting with `/`) as a command on `session_id` and return what to
    /// tell the user, or `None` when it is not one of this backend's commands, in
    /// which case it goes to the model like any other prompt. Runs on the connection's
    /// thread, between turns.
    fn run_command(&mut self, _session_id: &str, _line: &str) -> Option<String> {
        None
    }

    /// The session's new title, when the command just run renamed it (`/rename`),
    /// taken so it is announced once. None by default.
    fn take_title_change(&mut self, _session_id: &str) -> Option<String> {
        None
    }

    /// The saved sessions, newest first, limited to those working in `cwd` when given.
    /// None by default.
    fn list_sessions(&mut self, _cwd: Option<&Path>) -> Vec<SessionSummary> {
        Vec::new()
    }

    /// Given once, before any session opens: how to say a session's options changed.
    fn set_notifier(&mut self, _notify: Notifier) {}

    /// The settings the editor may show for a session, current values included.
    fn config_options(&self, _session_id: &str) -> Vec<ConfigOption> {
        Vec::new()
    }

    /// Change one setting and return all of them as they now stand. A value that is
    /// not one of the option's choices, or an unknown id, is `invalid_params`.
    fn set_config_option(
        &mut self,
        _session_id: &str,
        config_id: &str,
        _value: &str,
    ) -> Result<Vec<ConfigOption>, BackendError> {
        Err(BackendError::invalid_params(format!(
            "Unknown config option: {config_id}"
        )))
    }

    /// Open an existing session and return its history as updates to replay, in
    /// order, or a `not_found` error when there is no such session. The client's
    /// `cwd` and servers are as for [`Backend::new_session`].
    fn load_session(
        &mut self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
    ) -> Result<Vec<Update>, BackendError>;

    /// Open an existing session without replaying its history (`session/resume`,
    /// beyond Python), or a `not_found` error when there is no such session. The
    /// client's `cwd` and servers are as for [`Backend::new_session`]. By default it
    /// loads the session and drops the replay.
    fn resume_session(
        &mut self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
    ) -> Result<(), BackendError> {
        self.load_session(session_id, cwd, mcp_servers).map(drop)
    }

    /// How to stop `session_id`'s running turn at its next safe point, callable from
    /// any thread while the turn runs; `None` for an unknown session. It must be
    /// obtained before the turn starts, since the turn borrows the backend.
    fn cancel_handle(&self, session_id: &str) -> Option<CancelHandle>;

    /// Answer one of the memory methods for `session_id` (beyond Python): its
    /// memory store, its vault's redaction over every text returned. An unknown
    /// session or entry is `not_found`. Runs on the connection's thread, between
    /// turns. A backend without memory leaves the methods unknown.
    fn memory(
        &mut self,
        _session_id: &str,
        request: MemoryRequest,
    ) -> Result<MemoryReply, BackendError> {
        let method = match request {
            MemoryRequest::List(_) => MEMORY_LIST_METHOD,
            MemoryRequest::Pin { .. } => MEMORY_PIN_METHOD,
            MemoryRequest::Forget { .. } => MEMORY_FORGET_METHOD,
        };
        Err(BackendError {
            code: METHOD_NOT_FOUND,
            message: format!("Method not found: {method}"),
        })
    }

    /// Answer one of the secret methods for `session_id` (beyond Python), on its
    /// vault and the project keyring it was given. An unknown session is
    /// `not_found`. Runs on the connection's thread, between turns. A backend with
    /// no vault leaves the methods unknown.
    fn secrets(
        &mut self,
        _session_id: &str,
        request: SecretMethod,
    ) -> Result<SecretReply, BackendError> {
        let method = match request {
            SecretMethod::Set { .. } => SECRET_SET_METHOD,
            SecretMethod::List => SECRET_LIST_METHOD,
            SecretMethod::Forget { .. } => SECRET_FORGET_METHOD,
        };
        Err(BackendError {
            code: METHOD_NOT_FOUND,
            message: format!("Method not found: {method}"),
        })
    }

    /// Run one turn on `session_id` with the user's `text`, reporting through
    /// `sink` as it goes. Runs on a worker thread, and may block on `sink.ask`.
    fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        sink: Arc<dyn TurnSink>,
    ) -> Result<Stop, BackendError>;

    /// Take an action from a surface no call waited on ([`A2UI_ACTION_METHOD`]):
    /// check it and give the text a turn runs on, which the connection then runs
    /// through [`Backend::prompt`]. Refused by default, for a backend that draws
    /// nothing.
    fn ui_action(
        &mut self,
        _session_id: &str,
        _action: &crate::sdk::agent86_ui::Action,
    ) -> Result<String, BackendError> {
        Err(BackendError::invalid_params(
            "Invalid params: this session draws no surfaces",
        ))
    }
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
    /// A session's config options changed outside any request (see [`Notifier`]).
    OptionsChanged(String),
}

/// What a running turn tells the connection.
#[derive(Debug)]
pub enum TurnMsg {
    Update(Update),
    /// A request for the editor. The reply is the editor's result, or its error's
    /// message; dropping it tells the caller there was no answer.
    Call {
        method: String,
        params: Value,
        reply: Sender<Result<Value, String>>,
    },
    Done(Result<Stop, BackendError>),
}

/// The [`TurnSink`] that hands everything to the connection's thread.
struct ChannelSink {
    events: Sender<Event>,
    session_id: String,
}

impl TurnSink for ChannelSink {
    fn update(&self, update: Update) {
        let _ = self.events.send(Event::Turn(TurnMsg::Update(update)));
    }

    fn ask(&self, request: PermissionRequest) -> Permission {
        let allow = if request.remember.is_some() {
            ALLOW_ALWAYS
        } else {
            ALLOW_ONCE
        };
        // No answer (the connection closed, the turn cancelled) declines.
        match self.call(
            "session/request_permission",
            permission_params(&self.session_id, &request),
            None,
        ) {
            Ok(result) => permission_from(&result, allow),
            Err(_) => Permission::Declined,
        }
    }

    fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, String> {
        let (reply, answer) = channel();
        self.events
            .send(Event::Turn(TurnMsg::Call {
                method: method.to_owned(),
                params,
                reply,
            }))
            .map_err(|_| "the connection is closed".to_owned())?;
        let gone = || "the connection closed or the turn was cancelled".to_owned();
        let received = match timeout {
            Some(timeout) => answer.recv_timeout(timeout).map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => format!(
                    "the editor did not answer {method} within {} seconds",
                    timeout.as_secs()
                ),
                std::sync::mpsc::RecvTimeoutError::Disconnected => gone(),
            })?,
            None => answer.recv().map_err(|_| gone())?,
        };
        received
    }
}

/// The protocol state of one connection.
pub struct Server<B: Backend> {
    backend: B,
    initialized: bool,
    events: Receiver<Event>,
    sender: Sender<Event>,
    /// The requests sent to the client and not yet answered, by request id.
    pending: HashMap<u64, Sender<Result<Value, String>>>,
    next_request: u64,
    /// The client is gone: the connection ends once the current line is handled.
    closed: bool,
    /// Notifications to send right after the current response, which they follow
    /// by design: a client learns a session's id from the response, so an update
    /// naming it must not arrive first.
    after: Vec<Value>,
}

impl<B: Backend> Server<B> {
    pub fn new(mut backend: B) -> Self {
        let (sender, events) = channel();
        let notify = sender.clone();
        backend.set_notifier(Arc::new(move |session_id| {
            let _ = notify.send(Event::OptionsChanged(session_id.to_owned()));
        }));
        Self {
            backend,
            initialized: false,
            events,
            sender,
            pending: HashMap::new(),
            next_request: 0,
            closed: false,
            after: Vec::new(),
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
                for message in std::mem::take(&mut self.after) {
                    emit(message);
                }
            }
            // A notification has no id and gets no reply, even when unknown.
            Inbound::Notification { .. } => {}
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
                self.backend
                    .client_capabilities(ClientCapabilities::from_params(params));
                Ok(result)
            }
            "session/new"
            | "session/load"
            | "session/resume"
            | "session/list"
            | "session/prompt"
            | "session/set_config_option"
            | MEMORY_LIST_METHOD
            | MEMORY_PIN_METHOD
            | MEMORY_FORGET_METHOD
            | SECRET_SET_METHOD
            | SECRET_LIST_METHOD
            | SECRET_FORGET_METHOD
            | A2UI_ACTION_METHOD
                if !self.initialized =>
            {
                Err(BackendError {
                    code: INVALID_REQUEST,
                    message: "Not initialized: call initialize first".into(),
                })
            }
            "authenticate" => authenticate(params).map_err(BackendError::invalid_params),
            "session/new" => self.session_new(params),
            "session/load" => self.session_load(params, emit),
            "session/resume" => self.session_resume(params),
            "session/prompt" => self.session_prompt(params, emit),
            "session/set_config_option" => self.set_config_option(params),
            "session/list" => self.session_list(params),
            MEMORY_LIST_METHOD | MEMORY_PIN_METHOD | MEMORY_FORGET_METHOD => {
                self.memory_method(method, params)
            }
            SECRET_SET_METHOD | SECRET_LIST_METHOD | SECRET_FORGET_METHOD => {
                self.secret_method(method, params)
            }
            A2UI_ACTION_METHOD => self.ui_action(params, emit),
            other => Err(BackendError {
                code: METHOD_NOT_FOUND,
                message: format!("Method not found: {other}"),
            }),
        }
    }

    fn session_new(&mut self, params: &Value) -> Result<Value, BackendError> {
        let (cwd, servers) = session_location(params)?;
        let session_id = self.backend.new_session(Path::new(&cwd), servers)?;
        self.advertise_commands(&session_id);
        let mut result = json!({ "sessionId": session_id });
        self.add_config_options(&mut result, &session_id);
        Ok(result)
    }

    /// `session/list`. One page: every session fits (the list is capped), so there
    /// is never a `nextCursor`, and a request that carries a cursor has reached the
    /// end.
    fn session_list(&mut self, params: &Value) -> Result<Value, BackendError> {
        let cwd = match params.get("cwd") {
            None | Some(Value::Null) => None,
            Some(Value::String(cwd)) if Path::new(cwd).is_absolute() => Some(cwd.as_str()),
            Some(_) => {
                return Err(BackendError::invalid_params(
                    "Invalid params: cwd must be an absolute path",
                ))
            }
        };
        if params.get("cursor").is_some_and(|cursor| !cursor.is_null()) {
            return Ok(json!({ "sessions": [] }));
        }
        let sessions: Vec<Value> = self
            .backend
            .list_sessions(cwd.map(Path::new))
            .iter()
            .map(|session| {
                let mut entry = json!({
                    "sessionId": session.session_id,
                    "cwd": session.cwd,
                    "updatedAt": iso8601(session.updated_at)
                });
                if let Some(title) = &session.title {
                    entry["title"] = json!(title);
                }
                if !session.cwd_known {
                    // The bare `agent86` key predates the namespace and stays while
                    // agent86-web reads it; the namespaced one replaces it later.
                    entry["_meta"] = json!({
                        "agent86": { "cwdKnown": false },
                        "com.maetek.agent86/cwdKnown": false
                    });
                }
                entry
            })
            .collect();
        Ok(json!({ "sessions": sessions }))
    }

    /// The memory methods (beyond Python). `kind` is `"fact"` (the default) or
    /// `"episode"`; a list page is `limit` entries (default [`MEMORY_PAGE`], at
    /// most [`MEMORY_PAGE_MAX`]) from an opaque `cursor`, newest first, and has a
    /// `nextCursor` while more remain.
    fn memory_method(&mut self, method: &str, params: &Value) -> Result<Value, BackendError> {
        use crate::memory::{EntryKind, ListQuery};
        let invalid =
            |message: &str| BackendError::invalid_params(format!("Invalid params: {message}"));
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("sessionId must be a string"))?;
        let kind = match params.get("kind") {
            None | Some(Value::Null) => EntryKind::Fact,
            Some(kind) if kind == "fact" => EntryKind::Fact,
            Some(kind) if kind == "episode" => EntryKind::Episode,
            Some(_) => return Err(invalid("kind must be \"fact\" or \"episode\"")),
        };
        let flag = |name: &str, default: bool| match params.get(name) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(invalid(&format!("{name} must be a boolean"))),
        };
        let id = || {
            params
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("id must be an integer"))
        };
        let unexpected = || BackendError::internal("Internal error: unexpected memory reply");
        match method {
            MEMORY_LIST_METHOD => {
                let limit = match params.get("limit") {
                    None | Some(Value::Null) => MEMORY_PAGE,
                    Some(limit) => limit
                        .as_u64()
                        .and_then(|limit| usize::try_from(limit).ok())
                        .filter(|limit| (1..=MEMORY_PAGE_MAX).contains(limit))
                        .ok_or_else(|| {
                            invalid(&format!(
                                "limit must be an integer from 1 to {MEMORY_PAGE_MAX}"
                            ))
                        })?,
                };
                let offset = match params.get("cursor") {
                    None | Some(Value::Null) => 0,
                    Some(Value::String(cursor)) => cursor
                        .parse::<usize>()
                        .map_err(|_| invalid("unknown cursor"))?,
                    Some(_) => return Err(invalid("cursor must be a string")),
                };
                let query = ListQuery {
                    kind,
                    // One more than the page, to know whether another follows.
                    limit: Some(limit + 1),
                    offset,
                    pinned_only: flag("pinned", false)?,
                };
                let MemoryReply::Entries(mut entries) = self
                    .backend
                    .memory(session_id, MemoryRequest::List(query))?
                else {
                    return Err(unexpected());
                };
                let more = entries.len() > limit;
                entries.truncate(limit);
                let mut result =
                    json!({ "entries": entries.iter().map(entry_json).collect::<Vec<_>>() });
                if more {
                    result["nextCursor"] = json!((offset + limit).to_string());
                }
                Ok(result)
            }
            MEMORY_PIN_METHOD => {
                let request = MemoryRequest::Pin {
                    kind,
                    id: id()?,
                    pinned: flag("pinned", true)?,
                };
                match self.backend.memory(session_id, request)? {
                    MemoryReply::Entry(entry) => Ok(json!({ "entry": entry_json(&entry) })),
                    _ => Err(unexpected()),
                }
            }
            _ => {
                let request = MemoryRequest::Forget { kind, id: id()? };
                match self.backend.memory(session_id, request)? {
                    MemoryReply::Forgotten => Ok(json!({})),
                    _ => Err(unexpected()),
                }
            }
        }
    }

    /// The secret methods (beyond Python). A name is the environment variable's
    /// (`^[A-Za-z_][A-Za-z0-9_]*$`); the user chose it, so the names reserved from
    /// the model's `request_secret` are allowed, as for the terminal's
    /// `/secret NAME`. No error message ever holds the value.
    fn secret_method(&mut self, method: &str, params: &Value) -> Result<Value, BackendError> {
        use crate::vault::{valid_name, SecretAnswer, MIN_SECRET_CHARS, NAME_PATTERN};
        let invalid =
            |message: &str| BackendError::invalid_params(format!("Invalid params: {message}"));
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("sessionId must be a string"))?;
        let name = || {
            params
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| valid_name(name))
                .map(str::to_owned)
                .ok_or_else(|| invalid(&format!("name must be a string matching {NAME_PATTERN}")))
        };
        let unexpected = || BackendError::internal("Internal error: unexpected secret reply");
        match method {
            SECRET_SET_METHOD => {
                let name = name()?;
                let value = match params.get("value") {
                    Some(Value::String(value)) => value,
                    _ => return Err(invalid("value must be a string")),
                };
                if value.chars().count() < MIN_SECRET_CHARS {
                    return Err(invalid(&format!(
                        "value must have at least {MIN_SECRET_CHARS} characters, or hiding \
                         it would rewrite ordinary text wherever those characters appear"
                    )));
                }
                let remember = match params.get("remember") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(remember)) => *remember,
                    Some(_) => return Err(invalid("remember must be a boolean")),
                };
                let request = SecretMethod::Set {
                    name: name.clone(),
                    answer: SecretAnswer {
                        value: value.clone(),
                        remember,
                    },
                };
                match self.backend.secrets(session_id, request)? {
                    SecretReply::Set(stored) => {
                        let mut result = json!({ "name": name, "remembered": stored.remembered });
                        if let Some(warning) = stored.warning {
                            result["warning"] = json!(warning);
                        }
                        Ok(result)
                    }
                    _ => Err(unexpected()),
                }
            }
            SECRET_LIST_METHOD => match self.backend.secrets(session_id, SecretMethod::List)? {
                SecretReply::List {
                    secrets,
                    can_remember,
                } => Ok(json!({
                    "secrets": secrets
                        .iter()
                        .map(|(name, remembered)| json!({ "name": name, "remembered": remembered }))
                        .collect::<Vec<_>>(),
                    "canRemember": can_remember
                })),
                _ => Err(unexpected()),
            },
            _ => {
                let request = SecretMethod::Forget { name: name()? };
                match self.backend.secrets(session_id, request)? {
                    SecretReply::Forgotten { forgotten, warning } => {
                        let mut result = json!({ "forgotten": forgotten });
                        if let Some(warning) = warning {
                            result["warning"] = json!(warning);
                        }
                        Ok(result)
                    }
                    _ => Err(unexpected()),
                }
            }
        }
    }

    /// Put the session's settings in `result` when it has any.
    fn add_config_options(&self, result: &mut Value, session_id: &str) {
        let options = self.backend.config_options(session_id);
        if !options.is_empty() {
            result["configOptions"] = config_options_json(&options);
        }
    }

    /// `session/set_config_option`: change a select option, answer with all of them.
    fn set_config_option(&mut self, params: &Value) -> Result<Value, BackendError> {
        let text = |key: &str| {
            params.get(key).and_then(Value::as_str).ok_or_else(|| {
                BackendError::invalid_params(format!("Invalid params: {key} must be a string"))
            })
        };
        let (session_id, config_id) = (text("sessionId")?, text("configId")?);
        let value = text("value")?;
        let options = self
            .backend
            .set_config_option(session_id, config_id, value)?;
        Ok(json!({ "configOptions": config_options_json(&options) }))
    }

    /// Queue the command list for after the response, when there is one to offer.
    fn advertise_commands(&mut self, session_id: &str) {
        let commands = self.backend.commands(session_id);
        if !commands.is_empty() {
            self.after
                .push(session_update(session_id, &Update::Commands(commands)));
        }
    }

    /// Reopen a session: its history is replayed as updates, in order, before the
    /// (empty) response, as the protocol asks.
    fn session_load(
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
        let (cwd, servers) = session_location(params)?;
        let history = self
            .backend
            .load_session(session_id, Path::new(&cwd), servers)?;
        for update in &history {
            emit(session_update(session_id, update));
        }
        self.advertise_commands(session_id);
        let mut result = json!({});
        self.add_config_options(&mut result, session_id);
        Ok(result)
    }

    /// `session/resume`: the session reopened with no replay, as the spec requires,
    /// then the same commands and options as after `session/load`.
    fn session_resume(&mut self, params: &Value) -> Result<Value, BackendError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BackendError::invalid_params("Invalid params: sessionId must be a string")
            })?;
        let (cwd, servers) = session_location(params)?;
        self.backend
            .resume_session(session_id, Path::new(&cwd), servers)?;
        self.advertise_commands(session_id);
        let mut result = json!({});
        self.add_config_options(&mut result, session_id);
        Ok(result)
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
        // A slash command the backend knows is answered here, with no model turn.
        if text.trim_start().starts_with('/') {
            if let Some(output) = self.backend.run_command(&session_id, text.trim()) {
                emit(session_update(&session_id, &Update::Message(output)));
                // A rename is announced, so the editor's list shows the new title.
                if let Some(title) = self.backend.take_title_change(&session_id) {
                    emit(session_update(&session_id, &Update::Title(title)));
                }
                // A command may have changed a setting the editor is showing.
                let options = self.backend.config_options(&session_id);
                if !options.is_empty() {
                    emit(session_update(&session_id, &Update::Config(options)));
                }
                return Ok(json!({ "stopReason": Stop::EndTurn.as_str() }));
            }
        }
        self.run_turn(session_id, text, emit)
    }

    /// `_com.maetek.agent86/a2ui/action` (beyond Python; ADR 0005): an action on a
    /// surface no call waited on, checked and recorded by the backend, then run as
    /// a turn on its text (`[ui action] <name> on <surfaceId>: ...`), which the
    /// turn's vault redaction and ingress guardrail cover as they cover a prompt.
    fn ui_action(
        &mut self,
        params: &Value,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Value, BackendError> {
        let invalid =
            |message: &str| BackendError::invalid_params(format!("Invalid params: {message}"));
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("sessionId must be a string"))?
            .to_owned();
        if params.get("callAgentFunction").is_some() {
            return Err(invalid(
                "agent functions are not supported; send a surface's action instead",
            ));
        }
        let action: crate::sdk::agent86_ui::Action = params
            .get("action")
            .cloned()
            .and_then(|action| serde_json::from_value(action).ok())
            .ok_or_else(|| {
                invalid("action must be an A2UI action (name, surfaceId, sourceComponentId, timestamp, context)")
            })?;
        let text = self.backend.ui_action(&session_id, &action)?;
        self.run_turn(session_id, text, emit)
    }

    /// Run a turn on `text` for `session_id`, reporting as it goes, and answer
    /// with its `stopReason`.
    fn run_turn(
        &mut self,
        session_id: String,
        text: String,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Value, BackendError> {
        let sink: Arc<dyn TurnSink> = Arc::new(ChannelSink {
            events: self.sender.clone(),
            session_id: session_id.clone(),
        });
        let mut cancel = CancelState {
            session_id: session_id.clone(),
            handle: self.backend.cancel_handle(&session_id),
            requested: false,
        };
        let Self {
            backend,
            events,
            sender,
            pending,
            next_request,
            closed,
            ..
        } = self;
        // Sessions whose options changed while the turn ran: the backend is busy with
        // the turn, so they are sent once it ends.
        let mut changed: Vec<String> = Vec::new();
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
                let event = events.recv();
                // A cancel that landed before the turn began waiting for one is
                // cleared by the turn's entry, so it is asked again as the turn
                // reports; the stop is asked of the turn until it ends.
                if matches!(
                    event,
                    Ok(Event::Turn(TurnMsg::Update(_) | TurnMsg::Call { .. }))
                ) {
                    cancel.repeat();
                }
                match event {
                    Ok(Event::Turn(TurnMsg::Update(update))) => {
                        emit(session_update(&session_id, &update))
                    }
                    Ok(Event::Turn(TurnMsg::Call {
                        method,
                        params,
                        reply,
                    })) => {
                        if *closed {
                            // Nobody to ask: dropping the reply is "no answer".
                            continue;
                        }
                        // No new question once the turn was cancelled (a sub-agent
                        // can still reach the gate before its next safe point):
                        // dropping the reply declines. `fs/*` calls still go
                        // through, since a running tool is allowed to finish.
                        if cancel.requested
                            && (method == "session/request_permission"
                                || method == SECRET_REQUEST_METHOD
                                || method == A2UI_WAIT_METHOD
                                || method == QUESTIONS_ASK_METHOD)
                        {
                            continue;
                        }
                        let id = *next_request;
                        *next_request += 1;
                        pending.insert(id, reply);
                        emit(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": method,
                            "params": params
                        }));
                    }
                    Ok(Event::Turn(TurnMsg::Done(result))) => break result,
                    Ok(Event::Line(line)) => during_turn(&line, pending, &mut cancel, emit),
                    Ok(Event::OptionsChanged(id)) => {
                        if !changed.contains(&id) {
                            changed.push(id);
                        }
                    }
                    Ok(Event::Invalid) => {
                        emit(error_response(Value::Null, PARSE_ERROR, "Parse error"))
                    }
                    Ok(Event::Eof | Event::ReadError(_)) => {
                        *closed = true;
                        // Dropping the replies declines whatever waits on them.
                        pending.clear();
                        // Nobody is left to read the answer: the turn (and any
                        // sub-agent in it) stops at its next safe point rather than
                        // spending its budget.
                        cancel.requested = true;
                        cancel.repeat();
                    }
                    Err(_) => break Err(BackendError::internal("the connection was lost")),
                }
            }
        });
        pending.clear();
        for id in changed {
            let options = self.backend.config_options(&id);
            if !options.is_empty() {
                emit(session_update(&id, &Update::Config(options)));
            }
        }
        // A cancelled turn is not an error and not an end of turn: whatever the
        // library underneath reported when its work was aborted, the answer is
        // `cancelled`.
        if cancel.requested {
            return Ok(json!({ "stopReason": Stop::Cancelled.as_str() }));
        }
        Ok(json!({ "stopReason": stop?.as_str() }))
    }
}

/// The `cwd` (absolute) and the `mcpServers` a session request names.
fn session_location(params: &Value) -> Result<(String, Vec<McpSpec>), BackendError> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError::invalid_params("Invalid params: cwd must be a string"))?;
    if !Path::new(cwd).is_absolute() {
        return Err(BackendError::invalid_params(
            "Invalid params: cwd must be an absolute path",
        ));
    }
    let servers = match params.get("mcpServers") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(servers)) => servers
            .iter()
            .map(parse_mcp_server)
            .collect::<Result<_, _>>()
            .map_err(BackendError::invalid_params)?,
        Some(_) => {
            return Err(BackendError::invalid_params(
                "Invalid params: mcpServers must be an array",
            ))
        }
    };
    Ok((cwd.to_owned(), servers))
}

/// One entry of `mcpServers`: `stdio` (no `type`, with a `command`), `http`, or `sse`.
fn parse_mcp_server(server: &Value) -> Result<McpSpec, String> {
    let object = server
        .as_object()
        .ok_or("Invalid params: an MCP server must be an object")?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let name = text("name").ok_or("Invalid params: an MCP server needs a name")?;
    match object.get("type").and_then(Value::as_str) {
        Some(kind @ ("http" | "sse")) => Ok(McpSpec::Remote {
            name: name.to_owned(),
            url: text("url")
                .ok_or_else(|| format!("Invalid params: MCP server '{name}' needs a url"))?
                .to_owned(),
            sse: kind == "sse",
            headers: name_value_pairs(object.get("headers"), name, "headers")?,
        }),
        Some(other) => Err(format!(
            "Invalid params: MCP server '{name}' has unknown type '{other}'"
        )),
        None => Ok(McpSpec::Stdio {
            name: name.to_owned(),
            command: text("command")
                .ok_or_else(|| format!("Invalid params: MCP server '{name}' needs a command"))?
                .to_owned(),
            args: match object.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(args)) => args
                    .iter()
                    .map(|arg| {
                        arg.as_str().map(str::to_owned).ok_or_else(|| {
                            format!("Invalid params: MCP server '{name}' args must be strings")
                        })
                    })
                    .collect::<Result<_, _>>()?,
                Some(_) => {
                    return Err(format!(
                        "Invalid params: MCP server '{name}' args must be an array"
                    ))
                }
            },
            env: name_value_pairs(object.get("env"), name, "env")?,
        }),
    }
}

/// `[{"name": .., "value": ..}]`, as `env` and `headers` are written.
fn name_value_pairs(
    value: Option<&Value>,
    server: &str,
    field: &str,
) -> Result<Vec<(String, String)>, String> {
    let bad =
        || format!("Invalid params: MCP server '{server}' {field} must be name and value pairs");
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let name = item.get("name").and_then(Value::as_str).ok_or_else(bad)?;
                let value = item.get("value").and_then(Value::as_str).ok_or_else(bad)?;
                Ok((name.to_owned(), value.to_owned()))
            })
            .collect(),
        Some(_) => Err(bad()),
    }
}

/// How an inbound message reads.
enum Inbound {
    Request {
        id: Value,
        method: String,
    },
    Notification {
        method: String,
        params: Value,
    },
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
        (Some(method), None) => Inbound::Notification {
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        },
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

/// A prompt turn's cancellation: whether `session/cancel` has arrived for it, and
/// how to stop it.
struct CancelState {
    session_id: String,
    handle: Option<CancelHandle>,
    requested: bool,
}

impl CancelState {
    /// Ask the turn to stop, if a cancel has been requested.
    fn repeat(&self) {
        if self.requested {
            if let Some(handle) = &self.handle {
                handle();
            }
        }
    }
}

/// Handle a line from the client while a turn runs. A response answers a
/// permission request; `session/cancel` for this turn's session stops the turn,
/// and declines whatever it waits on, since the client's own answers to those
/// are `cancelled` anyway; any other request has to wait, since one turn runs at
/// a time; other notifications are ignored.
fn during_turn(
    line: &str,
    pending: &mut HashMap<u64, Sender<Result<Value, String>>>,
    cancel: &mut CancelState,
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
                let _ = reply.send(match object.get("error") {
                    Some(error) => Err(error_text(error)),
                    None => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
                });
            }
        }
        Inbound::Request { id, .. } => emit(error_response(
            id,
            INVALID_REQUEST,
            "A prompt turn is running; wait for its response",
        )),
        Inbound::Notification { method, params } => {
            if method == "session/cancel"
                && params.get("sessionId").and_then(Value::as_str) == Some(&cancel.session_id)
            {
                cancel.requested = true;
                cancel.repeat();
                // Dropping the replies declines the calls the turn waits on.
                pending.clear();
            }
        }
        Inbound::Invalid => emit(error_response(
            Value::Null,
            INVALID_REQUEST,
            "Invalid request",
        )),
    }
}

/// The message of a JSON-RPC error object, for a caller that only reports it.
fn error_text(error: &Value) -> String {
    let message = error.get("message").and_then(Value::as_str);
    match (message, error.get("code").and_then(Value::as_i64)) {
        (Some(message), Some(code)) => format!("{message} (code {code})"),
        (Some(message), None) => message.to_owned(),
        (None, Some(code)) => format!("error code {code}"),
        (None, None) => "the editor returned an error".to_owned(),
    }
}

/// The user's answer to a permission request. Only choosing the allowing option
/// the request offered (`allow`) allows.
fn permission_from(result: &Value, allow: &str) -> Permission {
    let outcome = result.get("outcome");
    let selected = outcome.and_then(|outcome| outcome.get("outcome")) == Some(&json!("selected"));
    let option = outcome
        .and_then(|outcome| outcome.get("optionId"))
        .and_then(Value::as_str);
    if selected && option == Some(allow) {
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

/// A fact or episode as the memory methods send it: `kind`, `id`, `text` (the
/// fact, or the episode's task), `storedAt`, `pinned`, and when present
/// `pinnedAt`, `category`, and an episode's `outcome` and `sessionId`.
pub fn entry_json(entry: &crate::memory::Entry) -> Value {
    let mut value = json!({
        "kind": entry.kind.as_str(),
        "id": entry.id,
        "text": entry.text,
        "storedAt": iso8601(entry.ts),
        "pinned": entry.pinned(),
    });
    if let Some(at) = entry.pinned_at {
        value["pinnedAt"] = json!(iso8601(at));
    }
    if let Some(category) = &entry.category {
        value["category"] = json!(category);
    }
    if let Some(outcome) = &entry.outcome {
        value["outcome"] = json!(outcome);
    }
    if let Some(session_id) = &entry.session_id {
        value["sessionId"] = json!(session_id);
    }
    value
}

/// `seconds` since the Unix epoch as `2025-10-29T14:22:15Z`.
pub fn iso8601(seconds: f64) -> String {
    let whole = seconds.max(0.0) as i64;
    let (days, rest) = (whole.div_euclid(86_400), whole.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// `options` as the protocol writes them.
fn config_options_json(options: &[ConfigOption]) -> Value {
    Value::Array(
        options
            .iter()
            .map(|option| {
                let pairs = |pairs: &[(String, String)]| {
                    pairs
                        .iter()
                        .map(|(value, name)| json!({ "value": value, "name": name }))
                        .collect::<Vec<_>>()
                };
                let choices = if option.groups.is_empty() {
                    pairs(&option.choices)
                } else {
                    option
                        .groups
                        .iter()
                        .map(|(heading, members)| {
                            json!({
                                "group": heading.to_lowercase().replace(' ', "-"),
                                "name": heading,
                                "options": pairs(members)
                            })
                        })
                        .collect()
                };
                let mut entry = json!({
                    "id": option.id,
                    "name": option.name,
                    "category": option.category,
                    "type": "select",
                    "currentValue": option.current,
                    "options": choices
                });
                if let Some(description) = &option.description {
                    entry["description"] = json!(description);
                }
                entry
            })
            .collect(),
    )
}

/// The `session/update` notification for `update`.
fn session_update(session_id: &str, update: &Update) -> Value {
    // Custom notifications, not `session/update`: the spec reserves the update
    // kinds for itself.
    match update {
        Update::State(event) => {
            return json!({
                "jsonrpc": "2.0",
                "method": STATE_METHOD,
                "params": { "sessionId": session_id, "event": event }
            })
        }
        Update::Ui(messages) => {
            return json!({
                "jsonrpc": "2.0",
                "method": A2UI_RENDER_METHOD,
                "params": { "sessionId": session_id, "messages": messages }
            })
        }
        _ => {}
    }
    let body = match update {
        Update::State(_) | Update::Ui(_) => unreachable!("sent as their own notifications above"),
        Update::User(text) => json!({
            "sessionUpdate": "user_message_chunk",
            "content": text_content(text)
        }),
        Update::Message(text) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": text_content(text)
        }),
        Update::Thought(text) => json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": text_content(text)
        }),
        Update::Config(options) => json!({
            "sessionUpdate": "config_option_update",
            "configOptions": config_options_json(options)
        }),
        Update::Title(title) => json!({
            "sessionUpdate": "session_info_update",
            "title": title
        }),
        Update::Usage { used, size, cost } => {
            let mut body = json!({
                "sessionUpdate": "usage_update",
                "used": used,
                "size": size
            });
            // An unpriced model's cost is unknown, not zero: left out, as the
            // terminal shows `cost n/a`.
            if let Some(amount) = cost {
                body["cost"] = json!({ "amount": amount, "currency": "USD" });
            }
            body
        }
        Update::Compaction { id, summary, error } => {
            let mut body = json!({
                "sessionUpdate": "compaction_update",
                "compactionId": id,
                "status": if summary.is_some() { "completed" } else { "failed" }
            });
            if let Some(summary) = summary {
                body["summary"] = json!([text_content(summary)]);
            }
            if let Some(error) = error {
                body["error"] = json!(error);
            }
            body
        }
        Update::Commands(commands) => json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": commands.iter().map(|command| {
                let mut entry = json!({
                    "name": command.name,
                    "description": command.description
                });
                if let Some(hint) = &command.hint {
                    entry["input"] = json!({ "hint": hint });
                }
                entry
            }).collect::<Vec<_>>()
        }),
        Update::ToolCall {
            id,
            title,
            name,
            kind,
            raw_input,
            locations,
        } => {
            let mut body = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": title,
                "name": name,
                "kind": kind.as_str(),
                "status": "pending",
                "rawInput": raw_input
            });
            with_locations(&mut body, locations);
            body
        }
        Update::ToolProgress { id, text } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": "in_progress",
            "content": [{ "type": "content", "content": text_content(text) }]
        }),
        Update::ToolResult {
            id,
            ok,
            text,
            diff,
            locations,
        } => {
            let mut body = json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "status": if *ok { "completed" } else { "failed" },
                "content": tool_content(diff.as_ref(), text)
            });
            with_locations(&mut body, locations);
            body
        }
    };
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id, "update": body }
    })
}

/// A tool call's `content`: the change as a `diff` item when there is one, then the
/// text, which stays for clients that do not show diffs (beyond Python).
fn tool_content(diff: Option<&FileChange>, text: &str) -> Value {
    let mut items = Vec::new();
    if let Some(change) = diff {
        items.push(json!({
            "type": "diff",
            "path": change.path,
            "oldText": change.old_text,
            "newText": change.new_text
        }));
    }
    items.push(json!({ "type": "content", "content": text_content(text) }));
    Value::Array(items)
}

/// Add `locations` to a tool call or update, when there are any.
fn with_locations(body: &mut Value, locations: &[Location]) {
    if locations.is_empty() {
        return;
    }
    body["locations"] = locations
        .iter()
        .map(|location| match location.line {
            Some(line) => json!({ "path": location.path, "line": line }),
            None => json!({ "path": location.path }),
        })
        .collect();
}

/// The parameters of `session/request_permission`: the call, in full, and the two
/// choices. The text is what the user is deciding on, so it is the whole detail.
fn permission_params(session_id: &str, request: &PermissionRequest) -> Value {
    let mut tool_call = json!({
        "toolCallId": request.tool_call_id,
        "title": request.title,
        "kind": request.kind.as_str(),
        "status": "pending",
        "rawInput": request.raw_input,
        "content": tool_content(request.diff.as_ref(), &request.text)
    });
    with_locations(&mut tool_call, &request.locations);
    let mut params = json!({
        "sessionId": session_id,
        "toolCall": tool_call,
        "options": match &request.remember {
            Some(name) => json!([
                { "optionId": ALLOW_ALWAYS, "name": name, "kind": "allow_always" },
                { "optionId": REJECT_ONCE, "name": "Reject", "kind": "reject_once" }
            ]),
            None => json!([
                { "optionId": ALLOW_ONCE, "name": "Allow once", "kind": "allow_once" },
                { "optionId": REJECT_ONCE, "name": "Reject", "kind": "reject_once" }
            ]),
        }
    });
    if let Some(asker) = &request.asker {
        params["_meta"] = json!({ ASKER_META: asker });
    }
    params
}

/// The id of the one authentication method: terminal setup, `agent86 auth`.
/// Keys may equally come from the environment, so no session is refused for
/// lack of authentication; the method exists so a client can offer the setup.
pub const AUTH_METHOD_ID: &str = "terminal-setup";

/// `authenticate`: the terminal flow runs outside this connection (the client
/// launches `agent86 auth` itself), so there is nothing to do but accept the id.
fn authenticate(params: &Value) -> Result<Value, String> {
    match params.get("methodId").and_then(Value::as_str) {
        Some(AUTH_METHOD_ID) => Ok(json!({})),
        Some(other) => Err(format!(
            "Invalid params: unknown authentication method '{other}'"
        )),
        None => Err("Invalid params: methodId must be a string".into()),
    }
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
            "loadSession": true,
            "sessionCapabilities": { "list": {}, "resume": {} },
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": false
            },
            "mcpCapabilities": {
                "http": true,
                "sse": true
            },
            // Advertised to every client, as the spec asks of extensions; only a
            // client that names it in its own capabilities is sent the events.
            "_meta": {
                STATE_CAPABILITY: { "version": STATE_VERSION },
                MEMORY_CAPABILITY: { "version": MEMORY_VERSION },
                SECRETS_CAPABILITY: { "version": SECRETS_VERSION },
                A2UI_CAPABILITY: {
                    "version": A2UI_VERSION,
                    "catalogIds": [crate::sdk::Catalog::basic().id()]
                },
                QUESTIONS_CAPABILITY: { "version": QUESTIONS_VERSION }
            }
        },
        "agentInfo": {
            "name": "agent86",
            "title": "agent86",
            "version": env!("CARGO_PKG_VERSION")
        },
        "authMethods": [{
            "id": AUTH_METHOD_ID,
            "name": "Set up a provider and API key",
            "description": "Choose a model provider and store its API key in the OS keyring.",
            "type": "terminal",
            "args": ["auth"]
        }]
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
            Ok(Event::OptionsChanged(id)) => {
                let options = server.backend.config_options(&id);
                if !options.is_empty() {
                    writer.send(session_update(&id, &Update::Config(options)));
                }
            }
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// What a scripted turn does, in order.
    enum Act {
        Update(Update),
        Ask(PermissionRequest),
        /// A method call on the editor, with how long to wait for it.
        Call(&'static str, Value, Option<Duration>),
    }

    /// A scripted backend: sessions are numbered, and a prompt does what the test
    /// queued, then ends the way it says.
    #[derive(Default)]
    struct Script {
        /// What `take_title_change` hands over once, after a command.
        title_change: Option<String>,
        sessions: Vec<(String, Vec<McpSpec>)>,
        prompts: Vec<(String, String)>,
        acts: Vec<Act>,
        answers: Arc<Mutex<Vec<Permission>>>,
        stop: Option<Result<Stop, BackendError>>,
        panic: bool,
        /// What `load_session` replays, and whether it finds the session at all.
        history: Vec<Update>,
        missing: bool,
        loads: Vec<(String, String, Vec<McpSpec>)>,
        /// Set by the cancel handle.
        cancelled: Arc<AtomicBool>,
        /// After its acts, the turn waits (up to five seconds) for a cancel.
        wait_for_cancel: bool,
        /// Clear the cancel flag when the turn starts, as the harness does.
        clear_on_entry: bool,
        /// What `initialize` told the backend about the client.
        capabilities: Option<ClientCapabilities>,
        /// What each `Act::Call` got back.
        call_results: Arc<Mutex<Vec<Result<Value, String>>>>,
        /// The commands offered, and the one this backend answers (`/ping`).
        offered: Vec<CommandInfo>,
        commands_run: Vec<String>,
        /// The settings offered; `set_config_option` changes `current` of the one
        /// named, if the value is one of its choices.
        options: Vec<ConfigOption>,
        /// How the backend was told to say its options changed.
        notify: Arc<Mutex<Option<Notifier>>>,
        /// Whether the turn should announce a change to its options.
        announce_during_turn: bool,
        /// The sessions listed, and the `cwd` each request asked about.
        saved: Vec<SessionSummary>,
        listed_for: Vec<Option<String>>,
        /// The facts and episodes the memory methods act on; `None` leaves the
        /// methods unknown, as the trait's default does.
        memory: Option<Vec<crate::memory::Entry>>,
        /// Each memory request, with its session.
        memory_asked: Vec<(String, MemoryRequest)>,
        /// The secrets held, with whether each is remembered; `None` leaves the
        /// secret methods unknown, as the trait's default does.
        secrets: Option<Vec<(String, bool)>>,
        /// Each secret request, with its session.
        secrets_asked: Vec<(String, SecretMethod)>,
    }

    impl Backend for Script {
        fn secrets(
            &mut self,
            session_id: &str,
            request: SecretMethod,
        ) -> Result<SecretReply, BackendError> {
            let Some(held) = &mut self.secrets else {
                return Err(BackendError {
                    code: METHOD_NOT_FOUND,
                    message: "Method not found".into(),
                });
            };
            self.secrets_asked
                .push((session_id.to_owned(), request.clone()));
            if session_id != "s" {
                return Err(BackendError::not_found("Session not found"));
            }
            match request {
                SecretMethod::Set { name, answer } => {
                    held.retain(|(known, _)| *known != name);
                    held.push((name, answer.remember));
                    Ok(SecretReply::Set(crate::vault::Stored {
                        remembered: answer.remember,
                        warning: (!answer.remember).then(|| "kept for this session".into()),
                    }))
                }
                SecretMethod::List => Ok(SecretReply::List {
                    secrets: held.clone(),
                    can_remember: true,
                }),
                SecretMethod::Forget { name } => {
                    let before = held.len();
                    held.retain(|(known, _)| *known != name);
                    Ok(SecretReply::Forgotten {
                        forgotten: held.len() < before,
                        warning: None,
                    })
                }
            }
        }

        fn memory(
            &mut self,
            session_id: &str,
            request: MemoryRequest,
        ) -> Result<MemoryReply, BackendError> {
            let Some(entries) = &mut self.memory else {
                return Err(BackendError {
                    code: METHOD_NOT_FOUND,
                    message: "Method not found".into(),
                });
            };
            self.memory_asked.push((session_id.to_owned(), request));
            let missing = || BackendError::not_found("No such entry");
            match request {
                MemoryRequest::List(query) => Ok(MemoryReply::Entries(
                    entries
                        .iter()
                        .filter(|entry| entry.kind == query.kind)
                        .filter(|entry| !query.pinned_only || entry.pinned())
                        .skip(query.offset)
                        .take(query.limit.unwrap_or(usize::MAX))
                        .cloned()
                        .collect(),
                )),
                MemoryRequest::Pin { kind, id, pinned } => entries
                    .iter_mut()
                    .find(|entry| entry.kind == kind && entry.id == id)
                    .map(|entry| {
                        entry.pinned_at = pinned.then_some(1_700_000_100.0);
                        MemoryReply::Entry(entry.clone())
                    })
                    .ok_or_else(missing),
                MemoryRequest::Forget { kind, id } => {
                    let before = entries.len();
                    entries.retain(|entry| entry.kind != kind || entry.id != id);
                    if entries.len() < before {
                        Ok(MemoryReply::Forgotten)
                    } else {
                        Err(missing())
                    }
                }
            }
        }

        fn commands(&self, _session_id: &str) -> Vec<CommandInfo> {
            self.offered.clone()
        }

        fn set_notifier(&mut self, notify: Notifier) {
            *self.notify.lock().unwrap() = Some(notify);
        }

        fn list_sessions(&mut self, cwd: Option<&Path>) -> Vec<SessionSummary> {
            self.listed_for
                .push(cwd.map(|cwd| cwd.to_string_lossy().into_owned()));
            self.saved.clone()
        }

        fn config_options(&self, _session_id: &str) -> Vec<ConfigOption> {
            self.options.clone()
        }

        fn set_config_option(
            &mut self,
            _session_id: &str,
            config_id: &str,
            value: &str,
        ) -> Result<Vec<ConfigOption>, BackendError> {
            let option = self
                .options
                .iter_mut()
                .find(|option| option.id == config_id)
                .ok_or_else(|| BackendError::invalid_params("unknown option"))?;
            if !option.choices.iter().any(|(choice, _)| choice == value) {
                return Err(BackendError::invalid_params("not a choice"));
            }
            option.current = value.to_owned();
            Ok(self.options.clone())
        }

        fn take_title_change(&mut self, _session_id: &str) -> Option<String> {
            self.title_change.take()
        }

        fn run_command(&mut self, _session_id: &str, line: &str) -> Option<String> {
            self.commands_run.push(line.to_owned());
            (line == "/ping" || line.starts_with("/ping ")).then(|| format!("pong from {line}"))
        }

        fn client_capabilities(&mut self, capabilities: ClientCapabilities) {
            self.capabilities = Some(capabilities);
        }

        fn new_session(
            &mut self,
            cwd: &Path,
            mcp_servers: Vec<McpSpec>,
        ) -> Result<String, BackendError> {
            self.sessions
                .push((cwd.to_string_lossy().into_owned(), mcp_servers));
            Ok(format!("sess_{}", self.sessions.len()))
        }

        fn load_session(
            &mut self,
            session_id: &str,
            cwd: &Path,
            mcp_servers: Vec<McpSpec>,
        ) -> Result<Vec<Update>, BackendError> {
            self.loads.push((
                session_id.to_owned(),
                cwd.to_string_lossy().into_owned(),
                mcp_servers,
            ));
            if self.missing {
                return Err(BackendError::not_found(format!(
                    "Session not found: {session_id}"
                )));
            }
            Ok(self.history.clone())
        }

        fn cancel_handle(&self, _session_id: &str) -> Option<CancelHandle> {
            let flag = Arc::clone(&self.cancelled);
            Some(Arc::new(move || flag.store(true, Ordering::SeqCst)))
        }

        fn prompt(
            &mut self,
            session_id: &str,
            text: &str,
            sink: Arc<dyn TurnSink>,
        ) -> Result<Stop, BackendError> {
            self.prompts.push((session_id.to_owned(), text.to_owned()));
            if self.announce_during_turn {
                let notify = self.notify.lock().unwrap().clone().unwrap();
                notify(session_id);
                notify(session_id);
            }
            if self.clear_on_entry {
                self.cancelled.store(false, Ordering::SeqCst);
            }
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
                    Act::Call(method, params, timeout) => {
                        let result = sink.call(method, params, timeout);
                        self.call_results.lock().unwrap().push(result);
                    }
                }
            }
            if self.wait_for_cancel {
                let start = Instant::now();
                while !self.cancelled.load(Ordering::SeqCst) {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "the turn was never asked to stop"
                    );
                    std::thread::sleep(Duration::from_millis(2));
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
        let method = &result["authMethods"][0];
        assert_eq!(result["authMethods"].as_array().unwrap().len(), 1);
        assert_eq!(method["id"], AUTH_METHOD_ID);
        assert_eq!(method["type"], "terminal");
        assert_eq!(method["args"], json!(["auth"]));
        assert_eq!(result["agentInfo"]["name"], "agent86");
        assert_eq!(result["agentInfo"]["version"], env!("CARGO_PKG_VERSION"));
        let capabilities = &result["agentCapabilities"];
        assert_eq!(capabilities["loadSession"], true);
        assert_eq!(capabilities["promptCapabilities"]["image"], false);
        assert_eq!(capabilities["promptCapabilities"]["audio"], false);
        assert_eq!(capabilities["promptCapabilities"]["embeddedContext"], false);
        assert_eq!(capabilities["mcpCapabilities"]["http"], true);
        assert_eq!(capabilities["mcpCapabilities"]["sse"], true);
        assert!(server.initialized());
    }

    fn fact(id: i64, text: &str) -> crate::memory::Entry {
        crate::memory::Entry {
            kind: crate::memory::EntryKind::Fact,
            id,
            ts: 1_700_000_000.0,
            text: text.into(),
            outcome: None,
            session_id: None,
            category: Some("fact".into()),
            pinned_at: None,
        }
    }

    fn memory_line(method: &str, params: Value) -> String {
        json!({"jsonrpc":"2.0","id":9,"method":method,"params":params}).to_string()
    }

    /// A server whose backend holds facts 3, 2, and 1 (newest first) and one
    /// episode.
    fn with_memory() -> Server<Script> {
        let mut server = ready();
        let episode = crate::memory::Entry {
            kind: crate::memory::EntryKind::Episode,
            id: 1,
            ts: 1_700_000_000.0,
            text: "list the files".into(),
            outcome: Some("README.md".into()),
            session_id: Some("sess-1".into()),
            category: None,
            pinned_at: None,
        };
        server.backend.memory = Some(vec![
            fact(3, "ship on fridays"),
            fact(2, "the db is postgres"),
            fact(1, "tabs, not spaces"),
            episode,
        ]);
        server
    }

    /// A value no reply or error may ever hold.
    const SECRET_VALUE: &str = "sk-live-hunter2-0123456789";

    fn with_secrets() -> Server<Script> {
        let mut server = ready();
        server.backend.secrets = Some(vec![("GITHUB_TOKEN".into(), true)]);
        server
    }

    #[test]
    fn the_secret_methods_are_advertised_and_need_initialize() {
        let mut server = Server::new(Script::default());
        for method in [SECRET_SET_METHOD, SECRET_LIST_METHOD, SECRET_FORGET_METHOD] {
            let early = one(&mut server, &memory_line(method, json!({"sessionId": "s"})));
            assert_eq!(early["error"]["code"], INVALID_REQUEST, "{method}");
        }
        let init = one(&mut server, INITIALIZE);
        assert_eq!(
            init["result"]["agentCapabilities"]["_meta"][SECRETS_CAPABILITY],
            json!({"version": 1})
        );
        // A backend with no vault leaves the methods unknown.
        let unknown = one(
            &mut server,
            &memory_line(SECRET_LIST_METHOD, json!({"sessionId": "s"})),
        );
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn the_client_says_it_draws_a_masked_entry_in_its_own_meta() {
        let capabilities = |meta: Value| {
            ClientCapabilities::from_params(&json!({ "clientCapabilities": { "_meta": meta } }))
        };
        assert!(capabilities(json!({ SECRETS_CAPABILITY: true })).secrets);
        assert!(capabilities(json!({ SECRETS_CAPABILITY: { "version": 1 } })).secrets);
        for refused in [json!(false), json!("yes"), json!(1), Value::Null] {
            assert!(
                !capabilities(json!({ SECRETS_CAPABILITY: refused })).secrets,
                "{refused}"
            );
        }
        assert!(!capabilities(json!({ STATE_CAPABILITY: true })).secrets);
        assert!(!ClientCapabilities::from_params(&json!({})).secrets);
    }

    #[test]
    fn a_client_renders_surfaces_only_when_it_names_the_basic_catalog() {
        let capabilities = |meta: Value| {
            ClientCapabilities::from_params(&json!({ "clientCapabilities": { "_meta": meta } }))
        };
        let basic = crate::sdk::Catalog::basic().id();
        assert!(
            capabilities(json!({ A2UI_CAPABILITY: { "supportedCatalogIds": ["other", basic] } }))
                .a2ui
        );
        // A renderer has to name what it draws: `true`, which the other extensions
        // take, is not enough, nor are other catalogs.
        for refused in [
            json!(true),
            json!({}),
            json!({ "supportedCatalogIds": [] }),
            json!({ "supportedCatalogIds": ["other"] }),
            json!({ "supportedCatalogIds": basic }),
        ] {
            assert!(
                !capabilities(json!({ A2UI_CAPABILITY: refused })).a2ui,
                "{refused}"
            );
        }
        assert!(!ClientCapabilities::from_params(&json!({})).a2ui);
    }

    #[test]
    fn secret_set_hands_the_backend_a_checked_answer_and_never_repeats_the_value() {
        let mut server = with_secrets();
        let reply = one(
            &mut server,
            &memory_line(
                SECRET_SET_METHOD,
                json!({"sessionId": "s", "name": "STRIPE_KEY", "value": SECRET_VALUE,
                       "remember": true}),
            ),
        );
        assert_eq!(
            reply["result"],
            json!({"name": "STRIPE_KEY", "remembered": true})
        );
        let (session, request) = &server.backend.secrets_asked[0];
        assert_eq!(session, "s");
        assert_eq!(
            request,
            &SecretMethod::Set {
                name: "STRIPE_KEY".into(),
                answer: crate::vault::SecretAnswer {
                    value: SECRET_VALUE.into(),
                    remember: true,
                },
            }
        );
        // The request's `Debug` (a panic message, a log) hides the value.
        assert!(!format!("{request:?}").contains(SECRET_VALUE));
        // `remember` defaults to false, and the backend's warning comes back.
        let reply = one(
            &mut server,
            &memory_line(
                SECRET_SET_METHOD,
                json!({"sessionId": "s", "name": "PATH", "value": SECRET_VALUE}),
            ),
        );
        assert_eq!(
            reply["result"],
            json!({"name": "PATH", "remembered": false, "warning": "kept for this session"})
        );
        assert!(!reply.to_string().contains(SECRET_VALUE));
        let missing = one(
            &mut server,
            &memory_line(
                SECRET_SET_METHOD,
                json!({"sessionId": "nope", "name": "A_KEY", "value": SECRET_VALUE}),
            ),
        );
        assert_eq!(missing["error"]["code"], RESOURCE_NOT_FOUND);
        assert!(!missing.to_string().contains(SECRET_VALUE));
    }

    #[test]
    fn bad_secret_params_are_refused_before_the_backend_and_never_quote_the_value() {
        let mut server = with_secrets();
        let base = json!({"sessionId": "s", "name": "API_KEY", "value": SECRET_VALUE});
        let with = |key: &str, value: Value| {
            let mut params = base.clone();
            params[key] = value;
            params
        };
        let mut without_value = base.clone();
        without_value.as_object_mut().unwrap().remove("value");
        for params in [
            with("sessionId", json!(7)),
            with("name", json!("1BAD")),
            with("name", json!("has space")),
            with("name", json!(SECRET_VALUE)),
            with("value", json!(12345678)),
            with("value", json!("short")),
            with("remember", json!("yes")),
            without_value,
        ] {
            let reply = one(&mut server, &memory_line(SECRET_SET_METHOD, params.clone()));
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{params}");
            assert!(!reply.to_string().contains("sk-live"), "{reply}");
        }
        let reply = one(
            &mut server,
            &memory_line(
                SECRET_FORGET_METHOD,
                json!({"sessionId": "s", "name": "x y"}),
            ),
        );
        assert_eq!(reply["error"]["code"], INVALID_PARAMS);
        assert!(server.backend.secrets_asked.is_empty());
        // A line that is not JSON says so without repeating any of it.
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"{SECRET_SET_METHOD}","params":{{"value":"{SECRET_VALUE}""#
        );
        let reply = one(&mut server, &line);
        assert_eq!(reply["error"]["code"], PARSE_ERROR);
        assert!(!reply.to_string().contains(SECRET_VALUE));
    }

    #[test]
    fn secret_list_and_forget_answer_in_their_shapes() {
        let mut server = with_secrets();
        one(
            &mut server,
            &memory_line(
                SECRET_SET_METHOD,
                json!({"sessionId": "s", "name": "NPM_TOKEN", "value": SECRET_VALUE}),
            ),
        );
        let list = one(
            &mut server,
            &memory_line(SECRET_LIST_METHOD, json!({"sessionId": "s"})),
        );
        assert_eq!(
            list["result"],
            json!({"secrets": [
                {"name": "GITHUB_TOKEN", "remembered": true},
                {"name": "NPM_TOKEN", "remembered": false}
            ], "canRemember": true})
        );
        let forget = |server: &mut Server<Script>| {
            one(
                server,
                &memory_line(
                    SECRET_FORGET_METHOD,
                    json!({"sessionId": "s", "name": "GITHUB_TOKEN"}),
                ),
            )
        };
        assert_eq!(forget(&mut server)["result"], json!({"forgotten": true}));
        assert_eq!(forget(&mut server)["result"], json!({"forgotten": false}));
    }

    #[test]
    fn no_secret_request_is_sent_after_a_cancel() {
        let mut server = ready();
        server.backend.acts = vec![Act::Call(
            SECRET_REQUEST_METHOD,
            json!({"sessionId": "sess_1", "name": "API_KEY"}),
            None,
        )];
        server
            .sender()
            .send(Event::Line(cancel_line("sess_1")))
            .unwrap();
        let out = replies(&mut server, &prompt_line(HI));
        assert!(
            !out.iter()
                .any(|message| message["method"] == SECRET_REQUEST_METHOD),
            "{out:?}"
        );
        assert!(server.backend.call_results.lock().unwrap()[0].is_err());
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "cancelled");
    }

    #[test]
    fn a_secret_request_reaches_the_client_and_its_answer_comes_back() {
        let mut server = ready();
        server.backend.acts = vec![Act::Call(
            SECRET_REQUEST_METHOD,
            json!({"sessionId": "sess_1", "name": "API_KEY"}),
            None,
        )];
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == SECRET_REQUEST_METHOD {
                let id = message["id"].clone();
                events
                    .send(Event::Line(
                        json!({"jsonrpc":"2.0","id":id,"result":{
                            "outcome":"entered","value":SECRET_VALUE,"remember":false}})
                        .to_string(),
                    ))
                    .unwrap();
            }
            out.push(message);
        });
        assert_eq!(
            server.backend.call_results.lock().unwrap()[0],
            Ok(json!({"outcome":"entered","value":SECRET_VALUE,"remember":false}))
        );
        // The value went to the turn, and nothing the agent sent holds it.
        assert!(out
            .iter()
            .all(|message| !message.to_string().contains(SECRET_VALUE)));
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "end_turn");
    }

    #[test]
    fn the_memory_methods_are_advertised_and_need_initialize() {
        let mut server = Server::new(Script::default());
        let early = one(
            &mut server,
            &memory_line(MEMORY_LIST_METHOD, json!({"sessionId": "s"})),
        );
        assert_eq!(early["error"]["code"], INVALID_REQUEST);
        let init = one(&mut server, INITIALIZE);
        assert_eq!(
            init["result"]["agentCapabilities"]["_meta"][MEMORY_CAPABILITY],
            json!({"version": 1})
        );
        // A backend without memory leaves the methods unknown.
        let unknown = one(
            &mut server,
            &memory_line(MEMORY_LIST_METHOD, json!({"sessionId": "s"})),
        );
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn memory_list_pages_newest_first_in_its_shape() {
        let mut server = with_memory();
        let first = one(
            &mut server,
            &memory_line(MEMORY_LIST_METHOD, json!({"sessionId": "s", "limit": 2})),
        );
        assert_eq!(
            first["result"],
            json!({"entries": [
                {"kind": "fact", "id": 3, "text": "ship on fridays",
                 "storedAt": "2023-11-14T22:13:20Z", "pinned": false, "category": "fact"},
                {"kind": "fact", "id": 2, "text": "the db is postgres",
                 "storedAt": "2023-11-14T22:13:20Z", "pinned": false, "category": "fact"}
            ], "nextCursor": "2"})
        );
        let rest = one(
            &mut server,
            &memory_line(
                MEMORY_LIST_METHOD,
                json!({"sessionId": "s", "limit": 2, "cursor": "2"}),
            ),
        );
        let entries = rest["result"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], 1);
        assert!(rest["result"].get("nextCursor").is_none());

        let episodes = one(
            &mut server,
            &memory_line(
                MEMORY_LIST_METHOD,
                json!({"sessionId": "s", "kind": "episode"}),
            ),
        );
        assert_eq!(
            episodes["result"]["entries"],
            json!([{"kind": "episode", "id": 1, "text": "list the files",
                    "storedAt": "2023-11-14T22:13:20Z", "pinned": false,
                    "outcome": "README.md", "sessionId": "sess-1"}])
        );
        // The backend was asked for one more than the page, from the cursor.
        let (session, request) = &server.backend.memory_asked[1];
        assert_eq!(session, "s");
        let MemoryRequest::List(query) = request else {
            panic!("{request:?}")
        };
        assert_eq!((query.limit, query.offset), (Some(3), 2));
        let MemoryRequest::List(query) = &server.backend.memory_asked[2].1 else {
            panic!()
        };
        assert_eq!(query.limit, Some(MEMORY_PAGE + 1));
    }

    #[test]
    fn memory_pin_and_forget_answer_or_say_not_found() {
        let mut server = with_memory();
        let pinned = one(
            &mut server,
            &memory_line(MEMORY_PIN_METHOD, json!({"sessionId": "s", "id": 2})),
        );
        assert_eq!(pinned["result"]["entry"]["pinned"], true);
        assert_eq!(
            pinned["result"]["entry"]["pinnedAt"],
            "2023-11-14T22:15:00Z"
        );
        let only = one(
            &mut server,
            &memory_line(
                MEMORY_LIST_METHOD,
                json!({"sessionId": "s", "pinned": true}),
            ),
        );
        assert_eq!(only["result"]["entries"].as_array().unwrap().len(), 1);
        let unpinned = one(
            &mut server,
            &memory_line(
                MEMORY_PIN_METHOD,
                json!({"sessionId": "s", "id": 2, "pinned": false}),
            ),
        );
        assert_eq!(unpinned["result"]["entry"]["pinned"], false);
        assert!(unpinned["result"]["entry"].get("pinnedAt").is_none());
        let episode = one(
            &mut server,
            &memory_line(
                MEMORY_PIN_METHOD,
                json!({"sessionId": "s", "id": 1, "kind": "episode", "pinned": true}),
            ),
        );
        assert_eq!(episode["result"]["entry"]["kind"], "episode");
        let nowhere = one(
            &mut server,
            &memory_line(MEMORY_PIN_METHOD, json!({"sessionId": "s", "id": 99})),
        );
        assert_eq!(nowhere["error"]["code"], RESOURCE_NOT_FOUND);

        let forgot = one(
            &mut server,
            &memory_line(MEMORY_FORGET_METHOD, json!({"sessionId": "s", "id": 3})),
        );
        assert_eq!(forgot["result"], json!({}));
        let again = one(
            &mut server,
            &memory_line(MEMORY_FORGET_METHOD, json!({"sessionId": "s", "id": 3})),
        );
        assert_eq!(again["error"]["code"], RESOURCE_NOT_FOUND);
    }

    #[test]
    fn memory_params_are_checked_before_the_backend_is_asked() {
        let mut server = with_memory();
        for (method, params) in [
            (MEMORY_LIST_METHOD, json!({})),
            (MEMORY_LIST_METHOD, json!({"sessionId": 5})),
            (
                MEMORY_LIST_METHOD,
                json!({"sessionId": "s", "kind": "facts"}),
            ),
            (MEMORY_LIST_METHOD, json!({"sessionId": "s", "limit": 0})),
            (MEMORY_LIST_METHOD, json!({"sessionId": "s", "limit": 201})),
            (MEMORY_LIST_METHOD, json!({"sessionId": "s", "limit": "5"})),
            (MEMORY_LIST_METHOD, json!({"sessionId": "s", "cursor": 2})),
            (
                MEMORY_LIST_METHOD,
                json!({"sessionId": "s", "cursor": "next"}),
            ),
            (
                MEMORY_LIST_METHOD,
                json!({"sessionId": "s", "pinned": "yes"}),
            ),
            (MEMORY_PIN_METHOD, json!({"sessionId": "s"})),
            (MEMORY_PIN_METHOD, json!({"sessionId": "s", "id": "2"})),
            (
                MEMORY_PIN_METHOD,
                json!({"sessionId": "s", "id": 2, "pinned": 1}),
            ),
            (MEMORY_FORGET_METHOD, json!({"sessionId": "s", "id": 2.5})),
        ] {
            let reply = one(&mut server, &memory_line(method, params.clone()));
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{method} {params}");
        }
        assert!(server.backend.memory_asked.is_empty());
    }

    #[test]
    fn authenticate_accepts_the_terminal_method_only() {
        let mut server = Server::new(Script::default());
        one(&mut server, INITIALIZE);
        let ok = one(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"authenticate","params":{"methodId":"terminal-setup"}}"#,
        );
        assert_eq!(ok["result"], json!({}));
        for params in [r#"{"methodId":"other"}"#, r#"{}"#] {
            let line =
                format!(r#"{{"jsonrpc":"2.0","id":2,"method":"authenticate","params":{params}}}"#);
            let bad = one(&mut server, &line);
            assert_eq!(bad["error"]["code"], INVALID_PARAMS, "{params}");
        }
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
        for method in ["session/close", "session/set_mode", "nonsense"] {
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
            r#"{"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":"s","cwd":"/w","mcpServers":[]}}"#,
        ] {
            let reply = one(&mut server, line);
            assert_eq!(reply["error"]["code"], INVALID_REQUEST, "{line}");
        }
        assert!(server.backend.sessions.is_empty());
        assert!(server.backend.loads.is_empty());
    }

    #[test]
    fn session_new_passes_the_directory_and_returns_the_id() {
        let mut server = ready();
        let cwd = std::env::temp_dir();
        let line = json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd": cwd, "mcpServers": mcp_servers_json()}})
        .to_string();
        let reply = one(&mut server, &line);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["sessionId"], "sess_1");
        assert_eq!(
            server.backend.sessions,
            vec![(cwd.to_string_lossy().into_owned(), mcp_specs())]
        );
    }

    /// One server of each kind, as an editor writes them.
    fn mcp_servers_json() -> Value {
        json!([
            {"name": "files", "command": "/bin/mcp-files", "args": ["--stdio", "/w"],
             "env": [{"name": "TOKEN", "value": "${HOME}/x"}]},
            {"type": "http", "name": "docs", "url": "https://h.example/mcp",
             "headers": [{"name": "Authorization", "value": "Bearer abc"}]},
            {"type": "sse", "name": "legacy", "url": "https://h.example/sse", "headers": []},
            {"name": "bare", "command": "mcp-bare"}
        ])
    }

    fn mcp_specs() -> Vec<McpSpec> {
        vec![
            McpSpec::Stdio {
                name: "files".into(),
                command: "/bin/mcp-files".into(),
                args: vec!["--stdio".into(), "/w".into()],
                env: vec![("TOKEN".into(), "${HOME}/x".into())],
            },
            McpSpec::Remote {
                name: "docs".into(),
                url: "https://h.example/mcp".into(),
                sse: false,
                headers: vec![("Authorization".into(), "Bearer abc".into())],
            },
            McpSpec::Remote {
                name: "legacy".into(),
                url: "https://h.example/sse".into(),
                sse: true,
                headers: vec![],
            },
            McpSpec::Stdio {
                name: "bare".into(),
                command: "mcp-bare".into(),
                args: vec![],
                env: vec![],
            },
        ]
    }

    #[test]
    fn bad_mcp_servers_are_refused_before_a_session_opens() {
        let mut server = ready();
        let cwd = std::env::temp_dir();
        for bad in [
            json!(["not an object"]),
            json!([{"command": "x"}]),
            json!([{"name": "", "command": "x"}]),
            json!([{"name": "a"}]),
            json!([{"name": "a", "command": ""}]),
            json!([{"name": "a", "command": "x", "args": "no"}]),
            json!([{"name": "a", "command": "x", "args": [1]}]),
            json!([{"name": "a", "command": "x", "env": [{"name": "K"}]}]),
            json!([{"name": "a", "command": "x", "env": "K=V"}]),
            json!([{"type": "http", "name": "a"}]),
            json!([{"type": "http", "name": "a", "url": "u", "headers": [{"value": "v"}]}]),
            json!([{"type": "websocket", "name": "a", "url": "u"}]),
        ] {
            let line = json!({"jsonrpc":"2.0","id":1,"method":"session/new",
                "params":{"cwd": cwd, "mcpServers": bad}})
            .to_string();
            let reply = one(&mut server, &line);
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{bad}");
        }
        assert!(server.backend.sessions.is_empty());
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
            locations: vec![Location {
                path: "/w/a.txt".into(),
                line: None,
            }],
        }
    }

    /// The change `write_call` makes to `/w/a.txt`.
    fn write_change() -> FileChange {
        FileChange {
            path: "/w/a.txt".into(),
            old_text: Some("y\n".into()),
            new_text: "x".into(),
            line: Some(1),
        }
    }

    fn write_request() -> PermissionRequest {
        PermissionRequest {
            tool_call_id: "call_1".into(),
            title: "write_file(...)".into(),
            kind: ToolKind::Edit,
            raw_input: json!({"path": "a.txt"}),
            text: "--- a/a.txt\n+++ b/a.txt".into(),
            remember: None,
            asker: None,
            diff: Some(write_change()),
            locations: vec![Location {
                path: "/w/a.txt".into(),
                line: Some(1),
            }],
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
                diff: Some(write_change()),
                locations: vec![Location {
                    path: "/w/a.txt".into(),
                    line: Some(1),
                }],
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
    fn a_remembered_question_offers_one_allow_always_option_and_only_it_allows() {
        let mut request = write_request();
        request.remember = Some("Allow this version of repo".into());
        let params = permission_params("sess_1", &request);
        assert_eq!(
            params["options"],
            json!([
                { "optionId": ALLOW_ALWAYS, "name": "Allow this version of repo", "kind": "allow_always" },
                { "optionId": REJECT_ONCE, "name": "Reject", "kind": "reject_once" }
            ])
        );
        let answer = |option: &str| json!({"outcome": {"outcome": "selected", "optionId": option}});
        assert_eq!(
            permission_from(&answer(ALLOW_ALWAYS), ALLOW_ALWAYS),
            Permission::Allowed
        );
        // An option the request did not offer declines.
        assert_eq!(
            permission_from(&answer(ALLOW_ONCE), ALLOW_ALWAYS),
            Permission::Declined
        );
        assert_eq!(
            permission_from(&answer(ALLOW_ALWAYS), ALLOW_ONCE),
            Permission::Declined
        );
        let progress = session_update(
            "sess_1",
            &Update::ToolProgress {
                id: "d".into(),
                text: "reviewer started".into(),
            },
        );
        assert_eq!(
            progress["params"]["update"],
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "d",
                "status": "in_progress",
                "content": [{"type": "content", "content": {"type": "text", "text": "reviewer started"}}]
            })
        );
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
        assert_eq!(call["locations"], json!([{"path": "/w/a.txt"}]));

        // The diff comes first, the text stays beside it for clients without diffs.
        let diff = json!({"type": "diff", "path": "/w/a.txt", "oldText": "y\n", "newText": "x"});
        let request = &out[1];
        assert_eq!(request["id"], 0);
        assert_eq!(request["params"]["sessionId"], "sess_1");
        assert_eq!(request["params"]["toolCall"]["toolCallId"], "call_1");
        assert_eq!(request["params"]["toolCall"]["content"][0], diff);
        assert_eq!(
            request["params"]["toolCall"]["content"][1]["content"]["text"],
            "--- a/a.txt\n+++ b/a.txt"
        );
        assert_eq!(
            request["params"]["toolCall"]["locations"],
            json!([{"path": "/w/a.txt", "line": 1}])
        );
        let options = request["params"]["options"].as_array().unwrap();
        assert_eq!(options[0]["optionId"], ALLOW_ONCE);
        assert_eq!(options[0]["kind"], "allow_once");
        assert_eq!(options[1]["optionId"], REJECT_ONCE);
        assert_eq!(options[1]["kind"], "reject_once");

        let result = &out[2]["params"]["update"];
        assert_eq!(result["toolCallId"], "call_1");
        assert_eq!(result["status"], "completed");
        assert_eq!(result["content"][0], diff);
        assert_eq!(result["content"][1]["content"]["text"], "Wrote 1 byte");
        assert_eq!(
            result["locations"],
            json!([{"path": "/w/a.txt", "line": 1}])
        );

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
                    r#"{"jsonrpc":"2.0","method":"session/something_else","params":{"sessionId":"sess_1"}}"#
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

    fn cancel_line(session: &str) -> String {
        json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId": session}})
            .to_string()
    }

    /// Run a prompt whose turn reports one chunk and then waits to be stopped; the
    /// "editor" sends `session/cancel` when it sees that chunk.
    fn cancelled_turn(stop: Option<Result<Stop, BackendError>>) -> (Vec<Value>, Server<Script>) {
        let mut server = ready();
        server.backend.acts = vec![Act::Update(Update::Message("partial".into()))];
        server.backend.wait_for_cancel = true;
        server.backend.stop = stop;
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "session/update" {
                events.send(Event::Line(cancel_line("sess_1"))).unwrap();
            }
            out.push(message);
        });
        (out, server)
    }

    #[test]
    fn cancelling_stops_the_turn_and_answers_cancelled_whatever_it_returned() {
        for stop in [
            None,
            Some(Ok(Stop::MaxTurnRequests)),
            // A library that was aborted often fails; that is not an error here.
            Some(Err(BackendError::internal("stream aborted"))),
        ] {
            let (out, server) = cancelled_turn(stop);
            assert!(server.backend.cancelled.load(Ordering::SeqCst));
            // The update sent before the cancel still arrived, then the response.
            assert_eq!(out[0]["params"]["update"]["content"]["text"], "partial");
            let last = out.last().unwrap();
            assert_eq!(last["id"], 5);
            assert_eq!(last["result"]["stopReason"], "cancelled", "{last}");
        }
    }

    #[test]
    fn a_cancel_that_beat_the_turn_to_its_start_still_stops_it() {
        // The harness clears its cancel flag as a turn begins, so a cancel already
        // handled by then would be lost; the pump asks again as the turn reports.
        let mut server = ready();
        server.backend.acts = vec![Act::Update(Update::Message("partial".into()))];
        server.backend.wait_for_cancel = true;
        server.backend.clear_on_entry = true;
        server
            .sender()
            .send(Event::Line(cancel_line("sess_1")))
            .unwrap();
        let out = replies(&mut server, &prompt_line(HI));
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "cancelled");
    }

    #[test]
    fn cancelling_declines_a_pending_permission_request() {
        let mut server = ready();
        server.backend.acts = vec![Act::Ask(write_request())];
        server.backend.wait_for_cancel = true;
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "session/request_permission" {
                events.send(Event::Line(cancel_line("sess_1"))).unwrap();
                // The client's own `cancelled` answer arrives too, and is not needed.
                events
                    .send(Event::Line(
                        json!({"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"cancelled"}}})
                            .to_string(),
                    ))
                    .unwrap();
            }
            out.push(message);
        });
        assert_eq!(
            *server.backend.answers.lock().unwrap(),
            [Permission::Declined]
        );
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "cancelled");
    }

    #[test]
    fn no_permission_request_is_sent_after_a_cancel() {
        // A sub-agent can reach the gate after the cancel, before its next safe
        // point; the question is declined without reaching the editor.
        let mut server = ready();
        server.backend.acts = vec![
            Act::Ask(write_request()),
            Act::Call("fs/read_text_file", json!({"path": "/w/a"}), None),
        ];
        server
            .sender()
            .send(Event::Line(cancel_line("sess_1")))
            .unwrap();
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "fs/read_text_file" {
                let id = message["id"].clone();
                events
                    .send(Event::Line(
                        json!({"jsonrpc":"2.0","id":id,"result":{"content":"a"}}).to_string(),
                    ))
                    .unwrap();
            }
            out.push(message);
        });
        assert!(
            !out.iter()
                .any(|message| message["method"] == "session/request_permission"),
            "{out:?}"
        );
        assert_eq!(
            *server.backend.answers.lock().unwrap(),
            [Permission::Declined]
        );
        // A file call still goes through: a running tool is allowed to finish.
        assert!(out
            .iter()
            .any(|message| message["method"] == "fs/read_text_file"));
        assert!(server.backend.call_results.lock().unwrap()[0].is_ok());
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "cancelled");
    }

    #[test]
    fn a_closed_connection_cancels_the_turn() {
        let mut server = ready();
        server.backend.acts = vec![Act::Update(Update::Message("partial".into()))];
        server.backend.wait_for_cancel = true;
        let events = server.sender();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "session/update" {
                events.send(Event::Eof).unwrap();
            }
        });
        assert!(server.backend.cancelled.load(Ordering::SeqCst));
        assert!(server.closed);
    }

    #[test]
    fn a_cancel_for_another_session_or_when_idle_changes_nothing() {
        let mut server = ready();
        // Idle: nothing runs, nothing is said, and nothing is left behind.
        assert!(replies(&mut server, &cancel_line("sess_1")).is_empty());
        assert!(!server.backend.cancelled.load(Ordering::SeqCst));

        server.backend.acts = vec![Act::Ask(write_request())];
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "session/request_permission" {
                events
                    .send(Event::Line(cancel_line("someone-else")))
                    .unwrap();
                events.send(Event::Line(selected(0, ALLOW_ONCE))).unwrap();
            }
            out.push(message);
        });
        assert_eq!(
            *server.backend.answers.lock().unwrap(),
            [Permission::Allowed]
        );
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "end_turn");
        assert!(!server.backend.cancelled.load(Ordering::SeqCst));
    }

    fn load_line(session: &str, cwd: &str) -> String {
        json!({"jsonrpc":"2.0","id":8,"method":"session/load",
            "params":{"sessionId": session, "cwd": cwd, "mcpServers": mcp_servers_json()}})
        .to_string()
    }

    #[test]
    fn loading_a_session_replays_its_history_before_the_empty_response() {
        let mut server = ready();
        let cwd = std::env::temp_dir();
        server.backend.history = vec![
            Update::User("What is here?".into()),
            Update::ToolCall {
                id: "c1".into(),
                title: "list_dir(...)".into(),
                name: "list_dir".into(),
                kind: ToolKind::Read,
                raw_input: json!({"path": "."}),
                locations: Vec::new(),
            },
            Update::ToolResult {
                id: "c1".into(),
                ok: true,
                text: "a.txt".into(),
                diff: None,
                locations: Vec::new(),
            },
            Update::Message("One file.".into()),
        ];
        let out = replies(&mut server, &load_line("sess_9", cwd.to_str().unwrap()));
        let kinds: Vec<&str> = out
            .iter()
            .take(4)
            .map(|m| m["params"]["update"]["sessionUpdate"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "user_message_chunk",
                "tool_call",
                "tool_call_update",
                "agent_message_chunk"
            ]
        );
        for message in &out[..4] {
            assert_eq!(message["params"]["sessionId"], "sess_9");
        }
        assert_eq!(
            out[0]["params"]["update"]["content"]["text"],
            "What is here?"
        );
        assert_eq!(out.len(), 5);
        assert_eq!(out[4]["id"], 8);
        assert_eq!(out[4]["result"], json!({}));
        assert_eq!(
            server.backend.loads,
            vec![(
                "sess_9".to_owned(),
                cwd.to_string_lossy().into_owned(),
                mcp_specs()
            )]
        );
    }

    #[test]
    fn loading_an_unknown_session_is_resource_not_found_and_bad_params_are_refused() {
        let mut server = ready();
        server.backend.missing = true;
        let cwd = std::env::temp_dir();
        let reply = one(&mut server, &load_line("nope", cwd.to_str().unwrap()));
        assert_eq!(reply["error"]["code"], RESOURCE_NOT_FOUND);
        assert!(reply["error"]["message"].as_str().unwrap().contains("nope"));

        for params in [
            r#"{"cwd":"/w","mcpServers":[]}"#,
            r#"{"sessionId":"s","mcpServers":[]}"#,
            r#"{"sessionId":"s","cwd":"relative","mcpServers":[]}"#,
            r#"{"sessionId":"s","cwd":"/w","mcpServers":7}"#,
        ] {
            let line =
                format!(r#"{{"jsonrpc":"2.0","id":1,"method":"session/load","params":{params}}}"#);
            assert_eq!(
                one(&mut server, &line)["error"]["code"],
                INVALID_PARAMS,
                "{params}"
            );
        }
        assert_eq!(
            server.backend.loads.len(),
            1,
            "refused loads never reached it"
        );
    }

    fn initialize_with(capabilities: Value) -> String {
        json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":1,"clientCapabilities":capabilities}})
        .to_string()
    }

    #[test]
    fn the_clients_file_capabilities_are_told_to_the_backend_exactly() {
        for (capabilities, read, write) in [
            (
                json!({"fs": {"readTextFile": true, "writeTextFile": true}}),
                true,
                true,
            ),
            (json!({"fs": {"readTextFile": true}}), true, false),
            (
                json!({"fs": {"writeTextFile": true, "readTextFile": false}}),
                false,
                true,
            ),
            // Only a real `true` counts: capabilities omitted or odd are unsupported.
            (
                json!({"fs": {"readTextFile": "yes", "writeTextFile": 1}}),
                false,
                false,
            ),
            (json!({"fs": null}), false, false),
            (json!({}), false, false),
        ] {
            let mut server = Server::new(Script::default());
            one(&mut server, &initialize_with(capabilities.clone()));
            assert_eq!(
                server.backend.capabilities,
                Some(ClientCapabilities {
                    read_text_file: read,
                    write_text_file: write,
                    compaction: false,
                    state: false,
                    secrets: false,
                    a2ui: false,
                    questions: false,
                }),
                "{capabilities}"
            );
        }
        // Without `clientCapabilities` at all.
        let mut server = Server::new(Script::default());
        one(
            &mut server,
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        assert_eq!(
            server.backend.capabilities,
            Some(ClientCapabilities::default())
        );
        // A refused handshake tells it nothing.
        let mut server = Server::new(Script::default());
        one(
            &mut server,
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#,
        );
        assert_eq!(server.backend.capabilities, None);
    }

    /// Run a prompt whose turn makes `calls`, answering each with what `reply` gives
    /// for its request (`None`: no answer). Returns what was written.
    fn call_turn(
        server: &mut Server<Script>,
        calls: Vec<Act>,
        reply: impl Fn(&Value) -> Option<String>,
    ) -> Vec<Value> {
        server.backend.acts = calls;
        let events = server.sender();
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message.get("method").is_some() && message.get("id").is_some() {
                if let Some(line) = reply(&message) {
                    events.send(Event::Line(line)).unwrap();
                }
            }
            out.push(message);
        });
        out
    }

    fn result_line(request: &Value, result: Value) -> Option<String> {
        Some(json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string())
    }

    #[test]
    fn a_turn_can_call_the_editor_and_gets_its_result_or_its_error() {
        let mut server = ready();
        let params = json!({"sessionId": "sess_1", "path": "/w/a.rs"});
        let out = call_turn(
            &mut server,
            vec![
                Act::Call("fs/read_text_file", params.clone(), None),
                Act::Ask(write_request()),
                Act::Call("fs/write_text_file", json!({"path": "/w/b"}), None),
            ],
            |request| match request["method"].as_str().unwrap() {
                "fs/read_text_file" => result_line(request, json!({"content": "fn main() {}\n"})),
                "session/request_permission" => result_line(
                    request,
                    json!({"outcome": {"outcome": "selected", "optionId": ALLOW_ONCE}}),
                ),
                _ => Some(
                    json!({"jsonrpc":"2.0","id":request["id"],
                        "error":{"code":-32603,"message":"disk full"}})
                    .to_string(),
                ),
            },
        );
        // Each request went out as written, under its own id, in order.
        let requests: Vec<&Value> = out.iter().filter(|m| m.get("method").is_some()).collect();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["method"], "fs/read_text_file");
        assert_eq!(requests[0]["params"], params);
        assert_eq!(requests[1]["method"], "session/request_permission");
        assert_eq!(requests[2]["method"], "fs/write_text_file");
        let ids: Vec<&Value> = requests.iter().map(|r| &r["id"]).collect();
        assert!(
            ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
            "{ids:?}"
        );

        let results = server.backend.call_results.lock().unwrap().clone();
        assert_eq!(results[0], Ok(json!({"content": "fn main() {}\n"})));
        let error = results[1].clone().unwrap_err();
        assert!(
            error.contains("disk full") && error.contains("-32603"),
            "{error}"
        );
        assert_eq!(
            *server.backend.answers.lock().unwrap(),
            [Permission::Allowed]
        );
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "end_turn");
    }

    #[test]
    fn a_call_the_editor_never_answers_times_out_and_the_turn_goes_on() {
        let mut server = ready();
        let out = call_turn(
            &mut server,
            vec![Act::Call(
                "fs/read_text_file",
                json!({"path": "/w/a"}),
                Some(Duration::from_millis(50)),
            )],
            |_| None,
        );
        let results = server.backend.call_results.lock().unwrap().clone();
        let error = results[0].clone().unwrap_err();
        assert!(
            error.contains("did not answer fs/read_text_file"),
            "{error}"
        );
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "end_turn");
    }

    #[test]
    fn cancelling_ends_a_call_the_turn_is_waiting_on() {
        let mut server = ready();
        server.backend.wait_for_cancel = true;
        let events = server.sender();
        server.backend.acts = vec![Act::Call(
            "fs/read_text_file",
            json!({"path": "/w/a"}),
            None,
        )];
        let mut out = Vec::new();
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "fs/read_text_file" {
                events.send(Event::Line(cancel_line("sess_1"))).unwrap();
            }
            out.push(message);
        });
        let results = server.backend.call_results.lock().unwrap().clone();
        assert!(results[0].is_err());
        assert_eq!(out.last().unwrap()["result"]["stopReason"], "cancelled");
    }

    #[test]
    fn a_closed_connection_ends_a_call_the_turn_is_waiting_on() {
        let mut server = ready();
        let events = server.sender();
        server.backend.acts = vec![
            Act::Call("fs/read_text_file", json!({"path": "/w/a"}), None),
            Act::Call("fs/read_text_file", json!({"path": "/w/b"}), None),
        ];
        server.handle_line(&prompt_line(HI), &mut |message| {
            if message["method"] == "fs/read_text_file" {
                events.send(Event::Eof).unwrap();
            }
        });
        let results = server.backend.call_results.lock().unwrap().clone();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_err), "{results:?}");
        assert!(server.closed);
    }

    fn mode_and_model() -> Vec<ConfigOption> {
        let choices = |values: &[&str]| {
            values
                .iter()
                .map(|v| ((*v).to_owned(), v.to_uppercase()))
                .collect()
        };
        vec![
            ConfigOption {
                id: "mode".into(),
                name: "Approval mode".into(),
                description: Some("How changes are approved".into()),
                category: "mode",
                current: "ask".into(),
                choices: choices(&["ask", "auto", "deny"]),
                groups: Vec::new(),
            },
            ConfigOption {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: "model",
                current: "p:a".into(),
                choices: choices(&["p:a", "p:b"]),
                groups: Vec::new(),
            },
        ]
    }

    fn new_session_line() -> String {
        json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd": std::env::temp_dir(), "mcpServers": []}})
        .to_string()
    }

    fn set_option_line(config_id: &str, value: Value) -> String {
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_config_option",
            "params":{"sessionId":"sess_1","configId":config_id,"value":value}})
        .to_string()
    }

    #[test]
    fn config_options_ride_on_the_session_response_in_the_protocols_shape() {
        let mut server = ready();
        server.backend.options = mode_and_model();
        let reply = one(&mut server, &new_session_line());
        assert_eq!(
            reply["result"]["configOptions"],
            json!([
                {"id": "mode", "name": "Approval mode", "description": "How changes are approved",
                 "category": "mode", "type": "select", "currentValue": "ask",
                 "options": [{"value":"ask","name":"ASK"},{"value":"auto","name":"AUTO"},
                             {"value":"deny","name":"DENY"}]},
                {"id": "model", "name": "Model", "category": "model", "type": "select",
                 "currentValue": "p:a",
                 "options": [{"value":"p:a","name":"P:A"},{"value":"p:b","name":"P:B"}]}
            ])
        );
        // A load carries them too, and a backend with none leaves the field out.
        let cwd = std::env::temp_dir();
        let loaded = replies(&mut server, &load_line("sess_9", cwd.to_str().unwrap()));
        assert!(loaded[0]["result"]["configOptions"].is_array());
        let mut bare = ready();
        let reply = one(&mut bare, &new_session_line());
        assert!(reply["result"].get("configOptions").is_none());
    }

    #[test]
    fn setting_an_option_answers_with_every_option_and_refuses_what_is_not_offered() {
        let mut server = ready();
        server.backend.options = mode_and_model();
        let reply = one(&mut server, &set_option_line("model", json!("p:b")));
        assert_eq!(reply["id"], 4);
        let options = reply["result"]["configOptions"].as_array().unwrap();
        assert_eq!(options.len(), 2, "the complete list, not just the change");
        assert_eq!(options[1]["currentValue"], "p:b");
        assert_eq!(options[0]["currentValue"], "ask");

        for (id, value) in [
            ("model", json!("p:zzz")),
            ("nonsense", json!("x")),
            ("mode", json!(true)),
            ("mode", json!(7)),
        ] {
            let reply = one(&mut server, &set_option_line(id, value.clone()));
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{id} {value}");
        }
        let reply = one(
            &mut server,
            r#"{"jsonrpc":"2.0","id":5,"method":"session/set_config_option","params":{"sessionId":"sess_1","value":"x"}}"#,
        );
        assert_eq!(reply["error"]["code"], INVALID_PARAMS);
        // Still as set before the refusals.
        assert_eq!(server.backend.options[1].current, "p:b");
    }

    #[test]
    fn a_long_list_is_sent_under_headings_with_the_same_values() {
        let mut options = mode_and_model();
        options[1].groups = vec![
            ("Configured".into(), vec![("p:a".into(), "p:a".into())]),
            (
                "Open Router".into(),
                vec![("p:b".into(), "B, long name".into())],
            ),
        ];
        let json = config_options_json(&options);
        assert_eq!(
            json[1]["options"],
            json!([
                {"group": "configured", "name": "Configured",
                 "options": [{"value": "p:a", "name": "p:a"}]},
                {"group": "open-router", "name": "Open Router",
                 "options": [{"value": "p:b", "name": "B, long name"}]}
            ])
        );
        // The short option is still flat.
        assert_eq!(
            json[0]["options"][0],
            json!({"value": "ask", "name": "ASK"})
        );
    }

    /// A reader the test feeds by hand, so the connection can sit idle.
    struct Fed(std::sync::mpsc::Receiver<Vec<u8>>, Vec<u8>);

    impl io::Read for Fed {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.1.is_empty() {
                match self.0.recv() {
                    Ok(bytes) => self.1 = bytes,
                    Err(_) => return Ok(0),
                }
            }
            let n = out.len().min(self.1.len());
            out[..n].copy_from_slice(&self.1[..n]);
            self.1.drain(..n);
            Ok(n)
        }
    }

    /// Collects what the connection writes, readable while it runs.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }

        fn wait_for(&self, needle: &str) {
            let start = Instant::now();
            while !self.text().contains(needle) {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "never saw {needle}: {}",
                    self.text()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn an_idle_connection_sends_new_options_when_the_backend_says_they_changed() {
        let script = Script {
            options: mode_and_model(),
            ..Script::default()
        };
        let notify = Arc::clone(&script.notify);
        let (feed, reader) = std::sync::mpsc::channel();
        let output = Shared::default();
        let writer = output.clone();
        let connection = std::thread::spawn(move || {
            serve(io::BufReader::new(Fed(reader, Vec::new())), writer, script).unwrap();
        });
        feed.send(format!("{INITIALIZE}\n").into_bytes()).unwrap();
        output.wait_for("\"protocolVersion\"");
        // Nothing asked: the backend alone decides when to say it.
        let notifier = notify.lock().unwrap().clone().expect("given a notifier");
        notifier("sess_7");
        output.wait_for("config_option_update");
        drop(feed);
        connection.join().unwrap();
        let text = output.text();
        let update: Value = serde_json::from_str(
            text.lines()
                .find(|line| line.contains("config_option_update"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(update["params"]["sessionId"], "sess_7");
        assert_eq!(update["params"]["update"]["configOptions"][0]["id"], "mode");
    }

    #[test]
    fn a_change_during_a_turn_is_sent_once_after_it_before_the_response() {
        let mut server = ready();
        server.backend.options = mode_and_model();
        server.backend.announce_during_turn = true;
        let out = replies(&mut server, &prompt_line(HI));
        let kinds: Vec<&str> = out
            .iter()
            .map(|m| {
                m["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("response")
            })
            .collect();
        // Told twice, sent once.
        assert_eq!(kinds, ["config_option_update", "response"]);
        assert_eq!(out[0]["params"]["sessionId"], "sess_1");
    }

    #[test]
    fn timestamps_are_iso_8601_in_utc() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400.0), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000.9), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(4_102_444_799.0), "2099-12-31T23:59:59Z");
        assert_eq!(iso8601(-5.0), "1970-01-01T00:00:00Z");
    }

    fn saved(id: &str, title: Option<&str>) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            cwd_known: true,
            title: title.map(str::to_owned),
            updated_at: 1_700_000_000.0,
        }
    }

    fn list_line(params: Value) -> String {
        json!({"jsonrpc":"2.0","id":6,"method":"session/list","params":params}).to_string()
    }

    #[test]
    fn the_session_list_is_advertised_and_answers_in_the_protocols_shape() {
        let mut server = Server::new(Script::default());
        let init = one(&mut server, INITIALIZE);
        assert_eq!(
            init["result"]["agentCapabilities"]["sessionCapabilities"],
            json!({"list": {}, "resume": {}})
        );
        server.backend.saved = vec![saved("a1", Some("Fix the build")), saved("b2", None)];
        let reply = one(&mut server, &list_line(json!({})));
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        assert_eq!(
            reply["result"],
            json!({"sessions": [
                {"sessionId": "a1", "cwd": cwd, "updatedAt": "2023-11-14T22:13:20Z",
                 "title": "Fix the build"},
                {"sessionId": "b2", "cwd": cwd, "updatedAt": "2023-11-14T22:13:20Z"}
            ]})
        );
        assert!(reply["result"].get("nextCursor").is_none());
        assert_eq!(server.backend.listed_for, [None]);
    }

    #[test]
    fn a_session_whose_directory_is_a_stand_in_says_so() {
        let mut server = ready();
        let mut unknown = saved("c3", Some("From before"));
        unknown.cwd_known = false;
        server.backend.saved = vec![saved("a1", Some("t")), unknown];
        let reply = one(&mut server, &list_line(json!({})));
        let sessions = reply["result"]["sessions"].as_array().unwrap();
        assert!(sessions[0].get("_meta").is_none(), "{reply}");
        assert_eq!(
            sessions[1]["_meta"],
            json!({"agent86": {"cwdKnown": false}, "com.maetek.agent86/cwdKnown": false})
        );
        // The schema requires a directory either way.
        assert!(sessions[1]["cwd"].is_string());
    }

    #[test]
    fn the_session_list_passes_the_directory_and_ends_at_a_cursor() {
        let mut server = ready();
        server.backend.saved = vec![saved("a1", Some("t"))];
        let dir = std::env::temp_dir();
        let reply = one(&mut server, &list_line(json!({"cwd": dir})));
        assert_eq!(reply["result"]["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(
            server.backend.listed_for,
            [Some(dir.to_string_lossy().into_owned())]
        );
        // One page only: asking for the next is asking for nothing.
        let reply = one(&mut server, &list_line(json!({"cursor": "x"})));
        assert_eq!(reply["result"], json!({"sessions": []}));
        assert_eq!(server.backend.listed_for.len(), 1, "not even asked");
        // A null cursor is no cursor, and a relative directory is refused.
        let reply = one(&mut server, &list_line(json!({"cursor": null})));
        assert_eq!(reply["result"]["sessions"].as_array().unwrap().len(), 1);
        for bad in [json!({"cwd": "relative"}), json!({"cwd": 5})] {
            let reply = one(&mut server, &list_line(bad));
            assert_eq!(reply["error"]["code"], INVALID_PARAMS);
        }
        let mut early = Server::new(Script::default());
        let reply = one(&mut early, &list_line(json!({})));
        assert_eq!(reply["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn setting_an_option_needs_the_handshake() {
        let mut server = Server::new(Script::default());
        let reply = one(&mut server, &set_option_line("mode", json!("auto")));
        assert_eq!(reply["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn a_command_that_may_change_a_setting_tells_the_editor_after_its_answer() {
        let mut server = ready();
        server.backend.options = mode_and_model();
        let out = replies(
            &mut server,
            &prompt_line(r#"[{"type":"text","text":"/ping"}]"#),
        );
        let kinds: Vec<&str> = out
            .iter()
            .map(|m| {
                m["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("response")
            })
            .collect();
        assert_eq!(
            kinds,
            ["agent_message_chunk", "config_option_update", "response"]
        );
        assert_eq!(out[1]["params"]["update"]["configOptions"][0]["id"], "mode");
    }

    #[test]
    fn a_rename_is_announced_as_session_info_after_the_answer() {
        let mut server = ready();
        server.backend.title_change = Some("Release prep".into());
        let out = replies(
            &mut server,
            &prompt_line(r#"[{"type":"text","text":"/ping"}]"#),
        );
        let kinds: Vec<&str> = out
            .iter()
            .map(|m| {
                m["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("response")
            })
            .collect();
        assert_eq!(
            kinds,
            ["agent_message_chunk", "session_info_update", "response"]
        );
        assert_eq!(out[1]["params"]["update"]["title"], "Release prep");
        assert_eq!(out[2]["result"]["stopReason"], "end_turn");
        // Taken once: the next command announces nothing.
        let out = replies(
            &mut server,
            &prompt_line(r#"[{"type":"text","text":"/ping"}]"#),
        );
        assert_eq!(out.len(), 2, "{out:?}");
    }

    fn offer(server: &mut Server<Script>) {
        server.backend.offered = vec![
            CommandInfo {
                name: "ping".into(),
                description: "Pong".into(),
                hint: Some("anything".into()),
            },
            CommandInfo {
                name: "cost".into(),
                description: "Cost".into(),
                hint: None,
            },
        ];
    }

    #[test]
    fn commands_are_advertised_after_the_session_response_never_before() {
        let mut server = ready();
        offer(&mut server);
        let cwd = std::env::temp_dir();
        let line = json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd": cwd, "mcpServers": []}})
        .to_string();
        let out = replies(&mut server, &line);
        assert_eq!(out.len(), 2, "{out:?}");
        // The response first: a client learns the session id from it.
        assert_eq!(out[0]["result"]["sessionId"], "sess_1");
        let update = &out[1]["params"]["update"];
        assert!(out[1].get("id").is_none());
        assert_eq!(out[1]["params"]["sessionId"], "sess_1");
        assert_eq!(update["sessionUpdate"], "available_commands_update");
        assert_eq!(
            update["availableCommands"],
            json!([
                {"name": "ping", "description": "Pong", "input": {"hint": "anything"}},
                {"name": "cost", "description": "Cost"}
            ])
        );
        // And only once: the next request carries none.
        let again = replies(&mut server, &line);
        assert_eq!(again.len(), 2);
        let reply = one(&mut server, &prompt_line(HI));
        assert!(reply.get("result").is_some());
    }

    #[test]
    fn a_loaded_session_advertises_after_its_history_and_response() {
        let mut server = ready();
        offer(&mut server);
        server.backend.history = vec![Update::User("hi".into())];
        let cwd = std::env::temp_dir();
        let out = replies(&mut server, &load_line("sess_9", cwd.to_str().unwrap()));
        let kinds: Vec<&str> = out
            .iter()
            .map(|m| {
                m["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("response")
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "user_message_chunk",
                "response",
                "available_commands_update"
            ]
        );
    }

    #[test]
    fn a_backend_with_no_commands_advertises_nothing() {
        let mut server = ready();
        let cwd = std::env::temp_dir();
        let line = json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd": cwd, "mcpServers": []}})
        .to_string();
        assert_eq!(replies(&mut server, &line).len(), 1);
    }

    #[test]
    fn a_command_is_answered_without_a_model_turn_and_others_go_to_the_model() {
        let mut server = ready();
        let out = replies(
            &mut server,
            &prompt_line(r#"[{"type":"text","text":"  /ping now  "}]"#),
        );
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(
            out[0]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(
            out[0]["params"]["update"]["content"]["text"],
            "pong from /ping now"
        );
        assert_eq!(out[1]["result"]["stopReason"], "end_turn");
        assert!(server.backend.prompts.is_empty(), "no turn ran");

        // Not this backend's command, and not a command at all: both reach the model.
        for text in ["/unknown thing", "see /ping later", "plain"] {
            let line = prompt_line(&format!(r#"[{{"type":"text","text":"{text}"}}]"#));
            let out = replies(&mut server, &line);
            assert_eq!(out.last().unwrap()["result"]["stopReason"], "end_turn");
        }
        assert_eq!(server.backend.prompts.len(), 3);
        // Only text that starts with a slash is offered as a command.
        assert_eq!(server.backend.commands_run, ["/ping now", "/unknown thing"]);
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
    fn usage_and_compaction_updates_have_the_specs_shape() {
        let priced = session_update(
            "s",
            &Update::Usage {
                used: 1200,
                size: 128_000,
                cost: Some(0.25),
            },
        );
        assert_eq!(
            priced["params"]["update"],
            json!({
                "sessionUpdate": "usage_update",
                "used": 1200,
                "size": 128_000,
                "cost": {"amount": 0.25, "currency": "USD"}
            })
        );
        let unpriced = session_update(
            "s",
            &Update::Usage {
                used: 1,
                size: 2,
                cost: None,
            },
        );
        assert!(unpriced["params"]["update"].get("cost").is_none());

        let failed = session_update(
            "s",
            &Update::Compaction {
                id: "cmp-1".into(),
                summary: None,
                error: Some("the summary could not be written".into()),
            },
        );
        assert_eq!(
            failed["params"]["update"],
            json!({
                "sessionUpdate": "compaction_update",
                "compactionId": "cmp-1",
                "status": "failed",
                "error": "the summary could not be written"
            })
        );
    }

    #[test]
    fn the_compaction_capability_needs_an_object() {
        let parse = |capabilities: Value| {
            ClientCapabilities::from_params(&json!({ "clientCapabilities": capabilities }))
                .compaction
        };
        assert!(parse(json!({"session": {"compaction": {}}})));
        assert!(!parse(json!({"session": {"compaction": null}})));
        assert!(!parse(json!({"session": null})));
        assert!(!parse(json!({"session": {"compaction": true}})));
        assert!(!parse(json!({})));
    }

    #[test]
    fn a_failed_tool_call_is_reported_failed() {
        let update = session_update(
            "s",
            &Update::ToolResult {
                id: "c".into(),
                ok: false,
                text: "boom".into(),
                diff: None,
                locations: Vec::new(),
            },
        );
        assert_eq!(update["params"]["update"]["status"], "failed");
        // No diff and no locations: one text item, and no `locations` key at all.
        assert_eq!(
            update["params"]["update"]["content"],
            json!([{"type": "content", "content": {"type": "text", "text": "boom"}}])
        );
        assert!(update["params"]["update"].get("locations").is_none());
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
