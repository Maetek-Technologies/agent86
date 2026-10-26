//! Sessions: titles, the SQLite store, and compatibility with Python's state_json.

mod support;

use agent86::session::{session_title, typed_prefix, AgentState, SessionStore};
use agent86::types::{AgentPhase, Message, Role};
use std::path::Path;
use support::fixture;

fn user(content: &str) -> Message {
    Message {
        role: Role::User,
        content: content.to_owned(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}

fn state_with(messages: Vec<Message>) -> AgentState {
    AgentState {
        messages,
        ..AgentState::new()
    }
}

#[test]
fn python_written_state_json_is_readable() {
    let state: AgentState = serde_json::from_str(&fixture("session/python_state.json")).unwrap();

    assert_eq!(state.session_id, "0123456789ab");
    assert_eq!(state.phase, AgentPhase::Done);
    assert_eq!(state.messages.len(), 4);
    assert_eq!(
        state.messages[1].tool_calls[0].arguments["path"],
        "README.md"
    );
    assert_eq!(state.messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(state.steps.len(), 2);
    assert_eq!(state.steps[0].results[0].content, "# Demo\n");
    assert_eq!(state.usage.input_tokens, 640);
    assert_eq!(state.usage.cache_read_tokens, 128);
    let turn = state.last_turn.as_ref().unwrap();
    assert_eq!(turn.tool_calls, 1);
    assert_eq!(turn.duration_s, 1.234);

    // And it survives a round trip through the Rust serializer.
    let again: AgentState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(again, state);
}

#[test]
fn new_sessions_start_executing_with_a_fresh_id() {
    let first = AgentState::new();
    let second = AgentState::new();
    assert_eq!(first.phase, AgentPhase::Execute);
    assert_eq!(first.session_id.len(), 12);
    assert_ne!(first.session_id, second.session_id);
    assert_eq!(
        serde_json::to_value(&first).unwrap()["last_turn"],
        serde_json::Value::Null
    );
}

#[test]
fn titles_follow_pythons_rules() {
    assert_eq!(session_title(&AgentState::new()), None);
    assert_eq!(
        session_title(&state_with(vec![user("  fix   the\n build  ")])).as_deref(),
        Some("fix the build")
    );
    let long = "word ".repeat(30);
    let title = session_title(&state_with(vec![user(&long)])).unwrap();
    assert_eq!(title.chars().count(), 60);
    assert!(title.ends_with("..."));

    // The typed text wins over an attached @file block.
    let mention = "summarize this\n\n--- @notes.md (3 lines) ---\nline one\nline two";
    assert_eq!(typed_prefix(mention), "summarize this");
    assert_eq!(
        session_title(&state_with(vec![user(mention)])).as_deref(),
        Some("summarize this")
    );

    // A message that is only an attachment is still named after its content.
    let bare = "--- @notes.md (1 lines) ---\nonly this";
    assert_eq!(
        session_title(&state_with(vec![user("   "), user(bare)])).as_deref(),
        Some("--- @notes.md (1 lines) --- only this")
    );
}

#[test]
fn the_store_saves_loads_and_names_sessions_once() {
    let store = SessionStore::open(Path::new(":memory:")).unwrap();
    let mut state = AgentState::new();

    store.persist(&state).unwrap();
    assert_eq!(store.title(&state.session_id).unwrap(), None);
    assert_eq!(store.load(&state.session_id).unwrap(), Some(state.clone()));

    state.messages.push(user("first question"));
    store.persist(&state).unwrap();
    assert_eq!(
        store.title(&state.session_id).unwrap().as_deref(),
        Some("first question")
    );

    // The name is write-once: a later persist does not rename it.
    state.messages.insert(0, user("an earlier message"));
    store.persist(&state).unwrap();
    assert_eq!(
        store.title(&state.session_id).unwrap().as_deref(),
        Some("first question")
    );
    assert_eq!(
        store
            .load(&state.session_id)
            .unwrap()
            .unwrap()
            .messages
            .len(),
        2
    );

    assert_eq!(store.load("nope").unwrap(), None);
    assert_eq!(store.title("nope").unwrap(), None);
}

#[test]
fn the_store_creates_its_directory_and_python_schema() {
    let root = std::env::temp_dir().join(format!("agent86-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("nested").join("memory.db");

    SessionStore::open(&path).unwrap();

    let connection = rusqlite::Connection::open(&path).unwrap();
    let mut tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    tables.sort();
    assert_eq!(tables, ["episodes", "memories", "sessions"]);
    let columns: Vec<String> = connection
        .prepare("SELECT name FROM pragma_table_info('sessions')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        columns,
        [
            "session_id",
            "created_at",
            "updated_at",
            "title",
            "state_json"
        ]
    );
    drop(connection);
    let _ = std::fs::remove_dir_all(&root);
}
