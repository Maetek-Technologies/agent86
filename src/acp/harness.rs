//! The ACP [`Backend`] over agent86's own harness: each session is a harness built
//! by [`assemble_in`] for the session's directory, a saved [`AgentState`], and the
//! memory database's session store, so an ACP session is an ordinary agent86
//! session (`agent86 run --session <id>` can continue it).
//!
//! Nothing here writes to stdout; the protocol owns it. Notes for the user go to
//! stderr.
//!
//! Tool calls are reported to the editor as they happen. A call the approval mode
//! asks about becomes a `session/request_permission` request, through the gate's
//! per-call prompt: `ask` mode asks for every side-effecting call, `auto` asks for
//! none (except a write into agent86's own configuration, which is asked about in
//! every mode but `deny`), and `deny` refuses them. The model never sees a vault
//! secret in what the editor is shown: arguments and previews are redacted first.
//!
//! Not built yet: cancelling a turn mid-way, and connecting the client's MCP
//! servers.

use super::{
    Backend, BackendError, Permission, PermissionRequest, Stop, ToolKind, TurnSink, Update,
};
use crate::agent::{Harness, HarnessError, TurnEvent};
use crate::approval::ApprovalPreview;
use crate::config::Config;
use crate::escape::show_controls;
use crate::pyjson::python_json_dumps;
use crate::session::{AgentState, SessionStore};
use crate::setup::{assemble_in, SetupError};
use crate::types::{ToolCall, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

/// How many characters of a call's arguments go into its title.
const TITLE_ARGS: usize = 100;

/// The running turn's sink, which the session's approval hook reads. Set for the
/// length of a turn; with none set, the hook declines.
type SinkSlot = Arc<Mutex<Option<Arc<dyn TurnSink>>>>;

struct Session {
    harness: Harness,
    store: Option<SessionStore>,
    state: AgentState,
    sink: SinkSlot,
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
        // No terminal prompt and no secret store: the editor is asked instead (below),
        // and the keyring is for a terminal. Without a terminal prompt,
        // `request_secret` is not offered.
        let mut assembly =
            assemble_in(&self.config, cwd, None, None).map_err(|error| match error {
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

        // Approvals go to the editor, through whichever sink the running turn has.
        let sink: SinkSlot = Arc::default();
        let secrets = assembly.harness.context.secrets.clone();
        let hook_sink = Arc::clone(&sink);
        assembly.harness.gate.call_prompt = Some(Box::new(
            move |call: &ToolCall, preview: &ApprovalPreview| {
                let running = hook_sink
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                let Some(running) = running else {
                    return false;
                };
                let request = PermissionRequest {
                    tool_call_id: call.id.clone(),
                    title: title(call, &secrets),
                    kind: tool_kind(&call.name),
                    raw_input: redacted_input(call, &secrets),
                    text: approval_text(preview),
                };
                running.ask(request) == Permission::Allowed
            },
        ));

        let state = AgentState::new();
        let session_id = state.session_id.clone();
        let session = Session {
            harness: assembly.harness,
            store: assembly.store,
            state,
            sink,
        };
        persist(&session.store, &session.state);
        self.sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }

    fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        sink: Arc<dyn TurnSink>,
    ) -> Result<Stop, BackendError> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| {
            BackendError::invalid_params(format!("Unknown session: {session_id}"))
        })?;
        let Session {
            harness,
            store,
            state,
            sink: slot,
        } = session;
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&sink));
        let secrets = harness.context.secrets.clone();

        let result = self
            .runtime
            .block_on(
                harness.run_turn_events(state, text, text, |event| match event {
                    TurnEvent::Text(chunk) | TurnEvent::Notice(chunk) if !chunk.is_empty() => {
                        sink.update(Update::Message(chunk.to_owned()))
                    }
                    TurnEvent::Thinking(chunk) if !chunk.is_empty() => {
                        sink.update(Update::Thought(chunk.to_owned()))
                    }
                    TurnEvent::ToolCall { call, .. } => sink.update(Update::ToolCall {
                        id: call.id.clone(),
                        title: title(call, &secrets),
                        name: call.name.clone(),
                        kind: tool_kind(&call.name),
                        raw_input: redacted_input(call, &secrets),
                    }),
                    TurnEvent::ToolResult {
                        call,
                        result,
                        observation,
                        ..
                    } => sink.update(Update::ToolResult {
                        id: call.id.clone(),
                        ok: result.ok,
                        text: result_text(result, observation),
                    }),
                    _ => {}
                }),
            );
        // The turn is over: nothing may be asked of the editor until the next one.
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
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

/// The category of a built-in tool; an MCP server's tools and the rest are `other`.
pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" | "list_dir" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "run_command" | "python_exec" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// `name(arguments)`, the arguments as the model sent them, capped, with vault
/// secrets and control characters dealt with: the name of a tool comes from the
/// model (an MCP server names its own), and the editor shows the title as text.
fn title(call: &ToolCall, secrets: &crate::vault::SecretVault) -> String {
    let arguments = python_json_dumps(&Value::Object(call.arguments.clone()), false);
    let arguments = secrets.redact(&arguments);
    let arguments = if arguments.chars().count() > TITLE_ARGS {
        format!(
            "{} ...",
            arguments.chars().take(TITLE_ARGS).collect::<String>()
        )
    } else {
        arguments
    };
    show_controls(&format!("{}({arguments})", call.name))
}

/// The call's arguments for the editor, with every vault value replaced by its
/// `${secret:NAME}` placeholder.
fn redacted_input(call: &ToolCall, secrets: &crate::vault::SecretVault) -> Value {
    let mut input = Value::Object(call.arguments.clone());
    secrets.redact_value(&mut input);
    input
}

/// What the user decides on: the warning if there is one, then the tool's own
/// detail (a diff, a command) or the argument summary, controls shown not sent.
/// Already redacted by the preview.
fn approval_text(preview: &ApprovalPreview) -> String {
    let detail = preview.detail.as_deref().unwrap_or(&preview.summary);
    let text = match &preview.warning {
        Some(warning) => format!("Warning: {warning}\n\n{detail}"),
        None => detail.to_owned(),
    };
    show_controls(&text)
}

/// A result for the editor: what the model is shown, or the error when that is
/// empty.
fn result_text(result: &ToolResult, observation: &str) -> String {
    if !observation.trim().is_empty() {
        return observation.to_owned();
    }
    match result.error.as_deref().filter(|error| !error.is_empty()) {
        Some(error) => error.to_owned(),
        None => "(no output)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    #[test]
    fn built_in_tools_have_kinds_and_the_rest_are_other() {
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("list_dir"), ToolKind::Read);
        assert_eq!(tool_kind("write_file"), ToolKind::Edit);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("run_command"), ToolKind::Execute);
        assert_eq!(tool_kind("python_exec"), ToolKind::Execute);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind("firecrawl_scrape"), ToolKind::Other);
    }

    #[test]
    fn a_title_names_the_tool_and_caps_and_cleans_its_arguments() {
        let secrets = crate::vault::SecretVault::default();
        let short = title(&call("read_file", json!({"path": "a.rs"})), &secrets);
        assert_eq!(short, r#"read_file({"path": "a.rs"})"#);

        let long = title(
            &call("write_file", json!({"content": "x".repeat(500)})),
            &secrets,
        );
        assert!(long.ends_with(" ...)"), "{long}");
        assert!(long.chars().count() < 140, "{}", long.chars().count());

        // A tool name from an MCP server can carry control characters.
        let hostile = title(&call("evil\u{1b}[2J", json!({})), &secrets);
        assert!(!hostile.contains('\u{1b}'), "{hostile:?}");
    }

    #[test]
    fn what_the_user_approves_is_the_detail_with_the_warning_first() {
        let preview = ApprovalPreview {
            summary: "{\"path\": \"a\"}".into(),
            detail: Some("--- a/a\n+++ b/a\r\n@@".into()),
            lexer: Some("diff"),
            warning: None,
        };
        let text = approval_text(&preview);
        assert!(text.starts_with("--- a/a"));
        assert!(
            !text.contains('\r'),
            "controls are shown, not sent: {text:?}"
        );

        let warned = ApprovalPreview {
            warning: Some("changes what agent86 runs".into()),
            detail: None,
            ..preview
        };
        let text = approval_text(&warned);
        assert!(text.starts_with("Warning: changes what agent86 runs\n\n"));
        assert!(
            text.ends_with("{\"path\": \"a\"}"),
            "falls back to the summary"
        );
    }

    #[test]
    fn a_result_shows_the_observation_or_else_the_error() {
        let failed = ToolResult {
            call_id: "c1".into(),
            name: "x".into(),
            ok: false,
            content: String::new(),
            error: Some("denied".into()),
            metadata: Default::default(),
        };
        assert_eq!(result_text(&failed, "saw this"), "saw this");
        assert_eq!(result_text(&failed, "  "), "denied");
        let empty = ToolResult {
            error: None,
            ..failed
        };
        assert_eq!(result_text(&empty, ""), "(no output)");
    }
}
