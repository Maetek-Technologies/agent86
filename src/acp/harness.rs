//! The ACP [`Backend`] over agent86's own harness: each session is a harness built
//! by [`assemble_in`] for the session's directory, a saved [`AgentState`], and the
//! memory database's session store, so an ACP session is an ordinary agent86
//! session (`agent86 run --session <id>` can continue it).
//!
//! Nothing here writes to stdout; the protocol owns it. Notes for the user go to
//! stderr.
//!
//! Not built yet, so a prompt cannot do these: show tool calls to the editor (they
//! run, but the editor sees only the text), ask the editor before a side-effecting
//! call (there is no approval prompt, so `ask` declines them, as in a piped
//! `run`), or be cancelled mid-turn. The client's MCP servers are not connected.

use super::{Backend, BackendError, Stop, Update};
use crate::agent::Harness;
use crate::agent::{HarnessError, TurnEvent};
use crate::config::Config;
use crate::session::{AgentState, SessionStore};
use crate::setup::{assemble_in, SetupError};
use std::collections::HashMap;
use std::path::Path;

struct Session {
    harness: Harness,
    store: Option<SessionStore>,
    state: AgentState,
}

/// Sessions on agent86's harness, driven on one single-threaded runtime.
pub struct HarnessBackend {
    config: Config,
    runtime: tokio::runtime::Runtime,
    sessions: HashMap<String, Session>,
}

impl HarnessBackend {
    pub fn new(config: Config) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            config,
            runtime,
            sessions: HashMap::new(),
        })
    }
}

impl Backend for HarnessBackend {
    fn new_session(&mut self, cwd: &Path, mcp_servers: usize) -> Result<String, BackendError> {
        // No approval prompt and no secret store: nobody can be asked yet, and the
        // keyring is for a terminal. Without a prompt, `request_secret` is not offered.
        let assembly = assemble_in(&self.config, cwd, None, None).map_err(|error| match error {
            SetupError::Model(message) => BackendError::internal(message),
            SetupError::Workspace(message) => BackendError::invalid_params(message),
        })?;
        if let Some(note) = &assembly.memory_note {
            eprintln!("memory: {note}");
        }
        if let Some(note) = &assembly.sandbox_note {
            eprintln!("sandbox: {note}");
        }
        if mcp_servers > 0 {
            eprintln!(
                "acp: the client named {mcp_servers} MCP server(s); connecting them is not \
                 supported yet, so only the configured servers are used"
            );
        }
        let state = AgentState::new();
        let session_id = state.session_id.clone();
        let session = Session {
            harness: assembly.harness,
            store: assembly.store,
            state,
        };
        persist(&session.store, &session.state);
        self.sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }

    fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        update: &mut dyn FnMut(Update),
    ) -> Result<Stop, BackendError> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| {
            BackendError::invalid_params(format!("Unknown session: {session_id}"))
        })?;
        let Session {
            harness,
            store,
            state,
        } = session;
        let result = self
            .runtime
            .block_on(
                harness.run_turn_events(state, text, text, |event| match event {
                    TurnEvent::Text(chunk) | TurnEvent::Notice(chunk) if !chunk.is_empty() => {
                        update(Update::Message(chunk.to_owned()))
                    }
                    TurnEvent::Thinking(chunk) if !chunk.is_empty() => {
                        update(Update::Thought(chunk.to_owned()))
                    }
                    // Tool calls and results reach the editor in a later slice.
                    _ => {}
                }),
            );
        // Saved whether the turn finished or failed, so the session stays resumable.
        persist(store, state);
        match result {
            Ok(_) if harness.cancel.is_cancelled() => Ok(Stop::Cancelled),
            Ok(_) => Ok(Stop::EndTurn),
            Err(HarnessError::CircuitTripped(reason)) if reason.starts_with("step budget") => {
                Ok(Stop::MaxTurnRequests)
            }
            Err(error) => Err(BackendError::internal(error.to_string())),
        }
    }
}

fn persist(store: &Option<SessionStore>, state: &AgentState) {
    if let Some(Err(error)) = store.as_ref().map(|store| store.persist(state)) {
        eprintln!("memory: could not save the session: {error}");
    }
}
