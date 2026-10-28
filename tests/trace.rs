//! The flight recorder: redaction, rotation, reading back, and OTLP export.

mod support;

use agent86::config::{ObservabilityConfig, RedactMode};
use agent86::observability::{
    otlp_document, parse_since, python_repr_value, read_trace, redact_event, rotated_path,
    Recorder, TraceFilter,
};
use agent86::pyjson::{python_json_dumps, python_json_dumps_indented};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use support::fixture;

fn golden() -> Value {
    serde_json::from_str(&fixture("trace/golden.json")).unwrap()
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("agent86-trace-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &Path, max_trace_bytes: u64, keep_traces: usize) -> ObservabilityConfig {
    ObservabilityConfig {
        trace: true,
        path: dir.display().to_string(),
        otel: false,
        otel_exporter: Default::default(),
        otel_endpoint: None,
        redact: RedactMode::Secrets,
        max_field_chars: 2000,
        max_trace_bytes,
        keep_traces,
    }
}

#[test]
fn redaction_matches_python() {
    for case in golden()["redact"].as_array().unwrap() {
        let redacted = redact_event(&object(&case["event"]), RedactMode::Secrets, 20);
        assert_eq!(
            Value::Object(redacted),
            case["redacted"],
            "{}",
            case["event"]
        );
    }
    let event = object(&json!({"task": "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaa"}));
    assert_eq!(redact_event(&event, RedactMode::None, 5), event);
}

#[test]
fn otlp_export_matches_python_byte_for_byte() {
    let golden = golden();
    let events: Vec<Map<String, Value>> = golden["otlp"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(object)
        .collect();
    let text = format!(
        "{}\n",
        python_json_dumps_indented(&otlp_document(&events), false, 2)
    );
    assert_eq!(text, golden["otlp"]["document_text"].as_str().unwrap());
    assert_eq!(
        python_json_dumps(&Value::Object(events[1].clone()), false),
        golden["jsonl_line"].as_str().unwrap()
    );
}

#[test]
fn durations_parse_like_python() {
    assert_eq!(parse_since("30s"), Ok(30.0));
    assert_eq!(parse_since(" 15m "), Ok(900.0));
    assert_eq!(parse_since("2H"), Ok(7200.0));
    assert_eq!(parse_since("1.5d"), Ok(129600.0));
    assert_eq!(parse_since("1w"), Ok(604800.0));
    assert_eq!(parse_since("45"), Ok(45.0));
    assert_eq!(
        parse_since("soon"),
        Err("'soon' is not a duration (try 30s, 15m, 2h, 7d)".to_owned())
    );
    assert!(parse_since("10y").is_err());
    assert!(parse_since(".5h").is_err());
}

#[test]
fn values_repr_like_python() {
    assert_eq!(
        python_repr_value(
            &json!({"tool": "x", "ok": true, "error": null, "n": [1, 2.5], "q": "it's"})
        ),
        "{'tool': 'x', 'ok': True, 'error': None, 'n': [1, 2.5], 'q': \"it's\"}"
    );
}

#[test]
fn the_recorder_writes_redacted_jsonl() {
    let dir = TempDir::new("write");
    let recorder = Recorder::from_config(&config(&dir.0, 0, 5));

    recorder.event(
        "s1",
        "turn_start",
        vec![("task", json!("key sk-abcdefghijklmnopqrstuvwxyz0123"))],
    );
    recorder.event(
        "s1",
        "turn_end",
        vec![("status", json!("done")), ("steps", json!(1))],
    );

    let text = std::fs::read_to_string(dir.0.join("trace.jsonl")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["session"], "s1");
    assert_eq!(first["kind"], "turn_start");
    assert_eq!(first["task"], "key ***REDACTED***");
    assert!(first["ts"].as_f64().unwrap() > 1.7e9);
    // Python's json.dumps layout: "key": value pairs separated by ", ".
    assert!(
        lines[1].contains("\"status\": \"done\", \"steps\": 1}"),
        "{}",
        lines[1]
    );
    assert!(!text.contains("abcdefghijklmnopqrstuvwxyz0123"));
}

#[test]
fn a_disabled_recorder_writes_nothing() {
    let dir = TempDir::new("disabled");
    let mut off = config(&dir.0, 0, 5);
    off.trace = false;
    Recorder::from_config(&off).event("s", "turn_start", vec![]);
    Recorder::disabled().event("s", "turn_start", vec![]);
    assert!(!dir.0.join("trace.jsonl").exists());
}

#[test]
fn the_trace_rotates_and_keeps_n_generations() {
    let dir = TempDir::new("rotate");
    let recorder = Recorder::from_config(&config(&dir.0, 200, 2));
    let live = dir.0.join("trace.jsonl");

    for index in 0..12 {
        recorder.event(
            "s1",
            "tick",
            vec![("n", json!(index)), ("pad", json!("x".repeat(60)))],
        );
    }

    assert!(live.exists());
    assert!(rotated_path(&live, 1).exists());
    assert!(rotated_path(&live, 2).exists());
    assert!(!rotated_path(&live, 3).exists());
    assert_eq!(rotated_path(&live, 2).file_name().unwrap(), "trace.2.jsonl");

    // Reading back walks the generations newest first and stays chronological.
    let all = read_trace(&live, &TraceFilter::default(), 100, 2);
    let numbers: Vec<i64> = all
        .iter()
        .map(|event| event["n"].as_i64().unwrap())
        .collect();
    assert!(
        numbers.windows(2).all(|pair| pair[0] < pair[1]),
        "{numbers:?}"
    );
    assert_eq!(*numbers.last().unwrap(), 11);
}

#[test]
fn reading_back_filters_before_limiting() {
    let dir = TempDir::new("filter");
    let recorder = Recorder::from_config(&config(&dir.0, 0, 5));
    for index in 0..10 {
        let session = if index % 2 == 0 { "even" } else { "odd" };
        let kind = if index % 3 == 0 {
            "tool_call"
        } else {
            "model_call"
        };
        recorder.event(session, kind, vec![("n", json!(index))]);
    }
    std::fs::write(
        dir.0.join("trace.jsonl"),
        std::fs::read_to_string(dir.0.join("trace.jsonl")).unwrap() + "not json\n\n",
    )
    .unwrap();
    let live = dir.0.join("trace.jsonl");
    let numbers = |filter: TraceFilter, limit: usize| -> Vec<i64> {
        read_trace(&live, &filter, limit, 5)
            .iter()
            .map(|event| event["n"].as_i64().unwrap())
            .collect()
    };

    assert_eq!(numbers(TraceFilter::default(), 3), [7, 8, 9]);
    let even = TraceFilter {
        session: Some("even".to_owned()),
        ..TraceFilter::default()
    };
    assert_eq!(numbers(even, 2), [6, 8]);
    let tools = TraceFilter {
        kinds: Some(vec!["tool_call".to_owned()]),
        ..TraceFilter::default()
    };
    assert_eq!(numbers(tools, 10), [0, 3, 6, 9]);
    let future = TraceFilter {
        after_ts: Some(f64::MAX),
        ..TraceFilter::default()
    };
    assert!(numbers(future, 10).is_empty());
}
