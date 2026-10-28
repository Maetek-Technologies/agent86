//! Long-term memory against CPython: the hashing embedder, float32 storage, the
//! cosine scan, the `remember`/`recall` tools, recall notes, and retention.
//!
//! `tests/fixtures/memory/golden.json` comes from a local script that mirrors
//! Python's `HashingEmbedder` and the store's packing and scoring with CPython's
//! `hashlib` and `array`, and pydantic's schemas and `int` parsing for mirrors of
//! the tools' `Args` models. The reference code itself is never run. Floats are
//! stored as Python `repr` strings: `serde_json`'s float parsing is not always
//! correctly rounded, and these comparisons are bit-exact.

mod support;

use agent86::memory::embeddings::{build_embedder, pack, Embedder, HashingEmbedder};
use agent86::memory::{Hit, COMPACTION_KIND};
use agent86::session::SessionStore;
use agent86::tools::memory::format_2f;
use agent86::tools::{default_registry, Arguments, ToolContext};
use agent86::types::{ToolCall, ToolResult};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::fixture;

fn golden() -> Value {
    serde_json::from_str(&fixture("memory/golden.json")).unwrap()
}

fn store() -> SessionStore {
    SessionStore::open(Path::new(":memory:")).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

#[test]
fn the_hashing_embedder_matches_python_bit_for_bit() {
    let golden = golden();
    for case in golden["embeddings"].as_array().unwrap() {
        let dim = case["dim"].as_u64().unwrap() as usize;
        let text = case["text"].as_str().unwrap();
        let expected: Vec<f64> = case["vector"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().parse().unwrap())
            .collect();
        let vector = HashingEmbedder::new(dim).encode_one(text);
        assert_eq!(vector, expected, "{dim} {text:?}");
        // The BLOB is what `array("f", vector).tobytes()` writes.
        assert_eq!(hex(&pack(&vector)), case["packed_hex"], "{text:?}");
    }
}

#[test]
fn search_scores_match_python_and_print_as_python_rounds() {
    let golden = golden();
    for case in golden["scores"].as_array().unwrap() {
        let memory = store();
        memory
            .add_memory(case["text"].as_str().unwrap(), "fact", &Map::new())
            .unwrap();
        let hits = memory
            .search_memories(case["query"].as_str().unwrap(), 5)
            .unwrap();
        let expected: f64 = case["score"].as_str().unwrap().parse().unwrap();
        assert_eq!(hits[0].score, expected, "{case}");
        if let Some(naive) = case.get("naive") {
            // A left fold would have produced this instead; Python 3.12 does not.
            let naive: f64 = naive.as_str().unwrap().parse().unwrap();
            assert_ne!(naive, expected, "{case}");
        }
        assert_eq!(format_2f(hits[0].score), case["formatted"], "{case}");
    }
    for (value, formatted) in golden["format_2f"].as_object().unwrap() {
        assert_eq!(format_2f(value.parse().unwrap()), *formatted, "{value}");
    }
}

#[test]
fn rows_python_wrote_are_found_and_other_dimensions_are_skipped() {
    let golden = golden();
    let python_row = golden["embeddings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["dim"] == 256 && case["text"] == "list files here")
        .unwrap();
    let path = std::env::temp_dir().join(format!("agent86-memory-rows-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let memory = SessionStore::open(&path).unwrap();
    {
        // As Python's `add_episode` writes them: a 256-dim hash vector, and a
        // 384-dim row from sentence-transformers that a hash query must skip.
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO episodes (session_id, ts, task, outcome, embedding, metadata_json) \
                 VALUES ('py', 1.0, 'list files here', 'README.md, notes.txt', ?1, '{}')",
                [unhex(python_row["packed_hex"].as_str().unwrap())],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO episodes (session_id, ts, task, outcome, embedding, metadata_json) \
                 VALUES ('py', 2.0, 'list files here', 'other model', ?1, '{}')",
                [vec![0u8; 384 * 4]],
            )
            .unwrap();
    }

    let hits = memory.search_episodes("list files here", 5).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].outcome(), "README.md, notes.txt");
    assert!((hits[0].score - 1.0).abs() < 1e-6, "{}", hits[0].score);
    drop(memory);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn recall_notes_quote_tasks_and_trim_outcomes_as_python_does() {
    let memory = store();
    let long = format!("Listed\n\n  the   files: {}", "x".repeat(250));
    memory
        .add_episode("s1", "list the files in this directory", &long, &Map::new())
        .unwrap();
    memory
        .add_episode("s1", "what's the weather like", "sunny", &Map::new())
        .unwrap();
    let mut compaction = Map::new();
    compaction.insert("kind".into(), json!(COMPACTION_KIND));
    memory
        .add_episode(
            "s1",
            "list the files in this directory",
            "[...]",
            &compaction,
        )
        .unwrap();

    let note = memory
        .recall_note("list the files in this directory")
        .unwrap();
    let collapsed = format!("Listed the files: {}", "x".repeat(250));
    let expected_outcome: String = collapsed.chars().take(200).collect::<String>() + " ...";
    assert_eq!(
        note,
        format!(
            "Relevant past experience (from memory; may help or warn):\n\
             - task: 'list the files in this directory' -> {expected_outcome}"
        )
    );
    // Python's `repr` switches quotes for a task with an apostrophe.
    let note = memory.recall_note("what's the weather like").unwrap();
    assert!(
        note.ends_with("- task: \"what's the weather like\" -> sunny"),
        "{note}"
    );
    assert_eq!(memory.recall_note("build a rocket ship"), None);
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "call_1".into(),
        name: name.into(),
        arguments: arguments.as_object().unwrap().clone(),
    }
}

fn workspace() -> PathBuf {
    let root = std::env::temp_dir().join(format!("agent86-memory-tools-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn run(ctx: &ToolContext, name: &str, arguments: Value) -> ToolResult {
    default_registry(true).dispatch(&call(name, arguments), ctx)
}

#[test]
fn the_memory_tools_follow_the_builtins_with_pydantics_schemas() {
    let golden = golden();
    let names: Vec<String> = default_registry(true)
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(names[names.len() - 2..], ["remember", "recall"]);
    assert!(!default_registry(false)
        .names()
        .contains(&"remember".to_owned()));
    for spec in default_registry(true).specs() {
        if let Some(expected) = golden["schemas"].get(&spec.name) {
            assert_eq!(
                serde_json::to_string(&spec.parameters).unwrap(),
                serde_json::to_string(expected).unwrap(),
                "{}",
                spec.name
            );
            assert!(!spec.side_effecting, "{}", spec.name);
        }
    }
}

#[test]
fn remember_and_recall_round_trip_facts() {
    let mut ctx = ToolContext::new(&workspace()).unwrap();
    ctx.memory = Some(store());

    let remembered = run(
        &ctx,
        "remember",
        json!({"text": "The user prefers tabs over spaces"}),
    );
    assert!(remembered.ok, "{remembered:?}");
    assert_eq!(remembered.content, "Remembered.");
    run(
        &ctx,
        "remember",
        json!({"text": "The capital of France is Paris"}),
    );

    let recalled = run(
        &ctx,
        "recall",
        json!({"query": "does the user prefer tabs or spaces"}),
    );
    assert!(recalled.ok, "{recalled:?}");
    let lines: Vec<&str> = recalled.content.lines().collect();
    assert_eq!(lines.len(), 1, "{}", recalled.content);
    assert!(
        lines[0].starts_with("- The user prefers tabs over spaces  [score 0."),
        "{}",
        lines[0]
    );

    let nothing = run(&ctx, "recall", json!({"query": "kernel compilation flags"}));
    assert_eq!(nothing.content, "(no relevant memories)");

    // `k` is clamped to 1..=20, so zero still returns the best fact.
    let one = run(&ctx, "recall", json!({"query": "tabs spaces user", "k": 0}));
    assert_eq!(one.content.lines().count(), 1, "{}", one.content);

    let invalid = run(&ctx, "recall", json!({"query": "x", "k": "lots"}));
    assert_eq!(
        invalid.error.as_deref(),
        Some(
            "Invalid arguments for 'recall': k: Input should be a valid integer, unable to parse \
             string as an integer"
        )
    );
}

#[test]
fn the_memory_tools_say_when_memory_is_disabled() {
    let ctx = ToolContext::new(&workspace()).unwrap();
    for (name, arguments) in [
        ("remember", json!({"text": "x"})),
        ("recall", json!({"query": "x"})),
    ] {
        let result = run(&ctx, name, arguments);
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some("Memory is disabled."));
    }
}

#[test]
fn integer_arguments_parse_as_pydantic_does() {
    let golden = golden();
    for (raw, expected) in golden["recall_k"].as_object().unwrap() {
        // Integers past `u64` reach Rust as floats (serde_json has no big
        // integers), so pydantic's arbitrary-precision cases can't be reproduced.
        if ["18446744073709551621", "-1180591620717411303424"].contains(&raw.as_str()) {
            continue;
        }
        let mut args = Map::new();
        args.insert("k".into(), serde_json::from_str(raw).unwrap());
        let mut arguments = Arguments::new(&args);
        let k = arguments.integer("k", 5);
        match (arguments.finish(), expected.get("ok")) {
            (Ok(()), Some(ok)) => {
                // Beyond `i64` the value saturates; only the clamped value matters.
                let ok = ok.as_f64().unwrap();
                assert_eq!(k.clamp(1, 20), (ok.clamp(1.0, 20.0)) as i64, "{raw}");
            }
            (Err(agent86::tools::ToolError::InvalidArguments(errors)), None) => {
                assert_eq!(errors, [expected["error"].as_str().unwrap()], "{raw}");
            }
            (outcome, _) => panic!("{raw}: got {outcome:?} ({k}), expected {expected}"),
        }
    }
}

#[test]
fn prune_and_retention_count_what_they_remove() {
    let memory = store();
    for index in 0..5 {
        memory
            .add_episode("s", &format!("task {index}"), "done", &Map::new())
            .unwrap();
        memory
            .add_memory(&format!("fact {index}"), "fact", &Map::new())
            .unwrap();
    }

    // Caps of 0 disable retention; facts are never touched by it.
    assert!(memory.enforce_retention(0, 0, 0.0).unwrap().is_empty());
    assert_eq!(
        memory.enforce_retention(3, 500, 0.0).unwrap(),
        [("episodes", 2)]
    );
    assert_eq!(memory.counts().unwrap().memories, 5);

    assert_eq!(
        memory.prune(None, Some(1), false).unwrap(),
        [("episodes", 2), ("sessions", 0)]
    );
    assert_eq!(
        memory.prune(Some(0.0), None, true).unwrap(),
        [("episodes", 1), ("sessions", 0), ("memories", 5)]
    );
    // The newest episode survived until the age cutoff took everything.
    assert_eq!(memory.counts().unwrap().episodes, 0);

    let id = memory.add_memory("keep me", "fact", &Map::new()).unwrap();
    assert!(memory.delete_memory(id).unwrap());
    assert!(!memory.delete_memory(id).unwrap());
}

#[test]
fn a_custom_embedder_spec_is_used_for_storage() {
    let (embedder, note) = build_embedder("hash:8");
    assert!(note.is_none());
    let memory = SessionStore::open_with(Path::new(":memory:"), Arc::clone(&embedder)).unwrap();
    assert_eq!(memory.embedder_spec(), "hash:8");
    memory
        .add_memory("tabs over spaces", "fact", &Map::new())
        .unwrap();
    let hits: Vec<Hit> = memory.search_memories("tabs over spaces", 1).unwrap();
    assert!((hits[0].score - 1.0).abs() < 1e-6);
}
