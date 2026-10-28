//! Sessions: the agent state threaded through a conversation and its SQLite
//! persistence, ported from Python's `orchestration/state.py` and the session
//! half of `memory/store.py`.
//!
//! The database file, schema, and `state_json` layout are Python's, so a session
//! started by either implementation can be resumed by the other. The same store
//! holds episodes and semantic facts; their half is in [`crate::memory`].

use crate::memory::embeddings::{Embedder, HashingEmbedder};
use crate::turn::{new_session_id, TurnSummary};
use crate::types::{AgentPhase, Message, Role, Step, Usage};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// Longest a derived session title may be, including the ellipsis.
pub const SESSION_TITLE_MAX: usize = 60;

/// Mutable state for one agent session, as Python's `AgentState`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(default = "new_session_id")]
    pub session_id: String,
    #[serde(default = "initial_phase")]
    pub phase: AgentPhase,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub usage: Usage,
    /// The turn that just finished; `None` before the first turn.
    #[serde(default)]
    pub last_turn: Option<TurnSummary>,
}

fn initial_phase() -> AgentPhase {
    AgentPhase::Init
}

impl AgentState {
    /// A fresh session, ready to execute, as Python's `Harness.new_session`.
    pub fn new() -> Self {
        Self {
            session_id: new_session_id(),
            phase: AgentPhase::Execute,
            messages: Vec::new(),
            steps: Vec::new(),
            usage: Usage::default(),
            last_turn: None,
        }
    }

    pub fn step_count(&self) -> usize {
        self.steps.len()
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

/// The header line an `@file` mention block opens with.
static MENTION_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^--- @.*---[ \t]*$").expect("valid pattern"));

/// What the user typed, with any `@file` attachment blocks stripped off.
pub fn typed_prefix(text: &str) -> &str {
    match MENTION_BLOCK.find(text) {
        None => text,
        Some(found) => {
            let head = &text[..found.start()];
            head.split("\n\n").next().unwrap_or_default()
        }
    }
}

/// A display name for a session: its first user message, collapsed and
/// truncated. `None` until there is one, so no placeholder name sticks.
pub fn session_title(state: &AgentState) -> Option<String> {
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    for message in state
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
    {
        let typed = collapse(typed_prefix(&message.content));
        let text = if typed.is_empty() {
            collapse(&message.content)
        } else {
            typed
        };
        if text.is_empty() {
            continue;
        }
        if text.chars().count() <= SESSION_TITLE_MAX {
            return Some(text);
        }
        let head: String = text.chars().take(SESSION_TITLE_MAX - 3).collect();
        return Some(format!("{head}..."));
    }
    None
}

/// The memory database could not be opened. The message says what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StoreError(pub String);

/// Python's memory database: sessions here, episodes and facts in [`crate::memory`].
///
/// Clones share one connection, so the CLI, the harness, and the memory tools
/// all use the same store. Access is serialized by its lock, as Python's store is
/// only ever used from one thread at a time.
#[derive(Clone)]
pub struct SessionStore {
    connection: Arc<Mutex<Connection>>,
    pub(crate) embedder: Arc<dyn Embedder>,
    path: PathBuf,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("path", &self.path)
            .field("embedder", &self.embedder.spec())
            .finish()
    }
}

impl SessionStore {
    /// Open (creating if needed) the database at `path`, or `:memory:`, embedding
    /// with the default `hash:256`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with(path, Arc::new(HashingEmbedder::default()))
    }

    /// [`SessionStore::open`] with the embedder episodes and facts are indexed by.
    pub fn open_with(path: &Path, embedder: Arc<dyn Embedder>) -> Result<Self, StoreError> {
        let opened = (|| -> Result<Connection, Box<dyn std::error::Error>> {
            if path != Path::new(":memory:") {
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let connection = Connection::open(path)?;
            connection.execute_batch(SCHEMA)?;
            Ok(connection)
        })();
        opened
            .map(|connection| Self {
                connection: Arc::new(Mutex::new(connection)),
                embedder,
                path: path.to_owned(),
            })
            .map_err(|error| {
                StoreError(format!(
                    "Could not open the memory database at {} - {error}. Another agent86 may be \
                 using it (close it and retry), or point `[memory] path` at another file - \
                 `agent86 config path` shows which file to edit. `[memory] enabled = false` \
                 turns memory off entirely.",
                    path.display()
                ))
            })
    }

    /// The database file, as Python's `MemoryStore.path`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The embedder's spec, e.g. `hash:256`.
    pub fn embedder_spec(&self) -> String {
        self.embedder.spec()
    }

    /// The connection; a panic while it was held cannot leave SQLite inconsistent.
    pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Upsert a session's state. `title = None` leaves any stored name alone.
    pub fn save(&self, state: &AgentState, title: Option<&str>) -> Result<(), StoreError> {
        let state_json =
            serde_json::to_string(state).map_err(|error| StoreError(error.to_string()))?;
        let now = now();
        self.connection()
            .execute(
                "INSERT INTO sessions (session_id, created_at, updated_at, title, state_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET
                     updated_at = excluded.updated_at,
                     title = COALESCE(excluded.title, sessions.title),
                     state_json = excluded.state_json",
                params![state.session_id, now, now, title, state_json],
            )
            .map(drop)
            .map_err(|error| StoreError(error.to_string()))
    }

    /// The stored state for `session_id`, if there is one that parses.
    pub fn load(&self, session_id: &str) -> Result<Option<AgentState>, StoreError> {
        let raw: Option<String> = self
            .connection()
            .query_row(
                "SELECT state_json FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| StoreError(error.to_string()))?;
        raw.map(|json| {
            serde_json::from_str(&json).map_err(|error| {
                StoreError(format!("session {session_id} could not be read: {error}"))
            })
        })
        .transpose()
    }

    /// The stored title for one session, or `None` (including for an unknown session).
    pub fn title(&self, session_id: &str) -> Result<Option<String>, StoreError> {
        self.connection()
            .query_row(
                "SELECT title FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(|error| StoreError(error.to_string()))
    }

    /// Save `state`, naming the session once, on the first save that has a user
    /// message to name it after, as Python's `Harness._persist`.
    pub fn persist(&self, state: &AgentState) -> Result<(), StoreError> {
        let title = if self.title(&state.session_id)?.is_none() {
            session_title(state)
        } else {
            None
        };
        self.save(state, title.as_deref())
    }

    /// Recent sessions, newest first, as Python's `MemoryStore.recent_sessions`.
    pub fn recent_sessions(&self, limit: usize) -> Result<Vec<SessionInfo>, StoreError> {
        let error = |error: rusqlite::Error| StoreError(error.to_string());
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT session_id, updated_at, title FROM sessions \
                 ORDER BY updated_at DESC LIMIT ?1",
            )
            .map_err(error)?;
        let rows = statement
            .query_map(params![limit as i64], |row| {
                Ok(SessionInfo {
                    session_id: row.get(0)?,
                    updated_at: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                    title: row.get(2)?,
                })
            })
            .map_err(error)?;
        rows.collect::<Result<_, _>>().map_err(error)
    }

    /// Row counts of the three tables, for `/memory`.
    pub fn counts(&self) -> Result<StoreCounts, StoreError> {
        let count = |table: &str| {
            self.connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|error| StoreError(error.to_string()))
        };
        Ok(StoreCounts {
            sessions: count("sessions")?,
            episodes: count("episodes")?,
            memories: count("memories")?,
        })
    }
}

/// One row of the session list, as Python's `SessionInfo`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub session_id: String,
    /// `None` for a session saved before its first user message.
    pub title: Option<String>,
    /// Seconds since the Unix epoch.
    pub updated_at: f64,
}

impl SessionInfo {
    /// The title, or a stand-in for a session that never got one.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or("(untitled)")
    }
}

/// How many rows the memory database holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCounts {
    pub sessions: i64,
    pub episodes: i64,
    pub memories: i64,
}

/// Python's schema, verbatim.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL,
    title TEXT,
    state_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS episodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    ts REAL NOT NULL,
    task TEXT NOT NULL,
    outcome TEXT NOT NULL,
    embedding BLOB,
    metadata_json TEXT
);
CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts REAL NOT NULL,
    kind TEXT NOT NULL DEFAULT 'fact',
    text TEXT NOT NULL,
    embedding BLOB,
    metadata_json TEXT
);
";

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// `[memory] path` with a leading `~` expanded, as Python's `os.path.expanduser`.
pub fn expand_home(path: &str) -> PathBuf {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match (path.strip_prefix('~'), std::env::var_os(home)) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            // Joined part by part, so the separators are the platform's, as with
            // Python's `Path`; `~/.agent86/x` must not become `...\.agent86/x`.
            let mut expanded = PathBuf::from(home);
            for part in rest.split(['/', '\\']).filter(|part| !part.is_empty()) {
                expanded.push(part);
            }
            expanded
        }
        _ => PathBuf::from(path),
    }
}
