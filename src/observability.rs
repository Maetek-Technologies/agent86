//! The flight-data recorder, ported from Python's `observability/recorder.py`
//! and `observability/redact.py`, plus the reading, filtering, and OTLP export
//! behind `agent86 trace` from `cli.py`.
//!
//! An append-only JSONL trace of turn boundaries, model calls, tool calls,
//! guardrail hits, and errors, each tagged with the session id. Two properties
//! make it safe to leave on: every event is redacted first (secret shapes
//! scrubbed, large free-text fields clipped), and the file rotates at a size cap.
//! Writing is best-effort: observability must never be the reason a turn fails.

use crate::config::{ObservabilityConfig, RedactMode};
use crate::pyjson::{python_float_repr, python_json_dumps};
use crate::types::python_repr;
use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What a matched secret becomes in the trace.
pub const REDACTED: &str = "***REDACTED***";

/// Fields whose values are large free text. Truncation is inherited: every
/// string nested inside one of these is clipped too.
const TRUNCATE_FIELDS: &[&str] = &["arguments", "task", "content", "error", "outcome"];

/// Replace every secret-shaped run in `text`, using the guardrail tier's secret
/// patterns and the key-shape catch-all, as Python's `scrub_secrets`.
pub fn scrub_secrets(text: &str) -> String {
    let scrubbed = crate::guardrails::scrub_secrets(text, REDACTED);
    crate::secrets::redact_key_shapes(&scrubbed, REDACTED)
}

fn clip(text: String, limit: usize) -> String {
    let length = text.chars().count();
    if limit == 0 || length <= limit {
        return text;
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…[truncated {} chars]", length - limit)
}

fn walk(value: &Value, limit: usize, truncate: bool, depth: usize) -> Value {
    if depth > 24 {
        return Value::String(clip(scrub_secrets(&python_repr_value(value)), limit));
    }
    match value {
        Value::String(text) => {
            let cleaned = scrub_secrets(text);
            Value::String(if truncate {
                clip(cleaned, limit)
            } else {
                cleaned
            })
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| {
                    let child = truncate || TRUNCATE_FIELDS.contains(&key.as_str());
                    (key.clone(), walk(item, limit, child, depth + 1))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| walk(item, limit, truncate, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A redacted copy of one event: unchanged in `none` mode, otherwise secret
/// shapes scrubbed everywhere and the free-text fields clipped.
pub fn redact_event(
    event: &Map<String, Value>,
    mode: RedactMode,
    max_field_chars: usize,
) -> Map<String, Value> {
    match (
        mode,
        walk(&Value::Object(event.clone()), max_field_chars, false, 0),
    ) {
        (RedactMode::None, _) => event.clone(),
        (RedactMode::Secrets, Value::Object(redacted)) => redacted,
        _ => event.clone(),
    }
}

/// `trace.jsonl` + 2 becomes `trace.2.jsonl`; generation 1 is the most recent.
pub fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let suffix = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{stem}.{index}{suffix}"))
}

/// Rename with brief retries: on Windows a rename fails while another process
/// (a tail, an editor, a virus scanner) holds the file open.
fn replace(source: &Path, destination: &Path) -> bool {
    for attempt in 0..5u32 {
        match std::fs::rename(source, destination) {
            Ok(()) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) if attempt < 4 => {
                std::thread::sleep(Duration::from_millis(50 * u64::from(attempt + 1)))
            }
            Err(_) => return false,
        }
    }
    false
}

struct Writer {
    file: File,
    size: u64,
}

/// Append-only JSONL writer for harness events.
pub struct Recorder {
    path: Option<PathBuf>,
    redact: RedactMode,
    max_field_chars: usize,
    max_bytes: u64,
    keep: usize,
    writer: Mutex<Option<Writer>>,
}

impl Recorder {
    /// A recorder that writes nothing.
    pub fn disabled() -> Self {
        Self {
            path: None,
            redact: RedactMode::Secrets,
            max_field_chars: 2000,
            max_bytes: 0,
            keep: 0,
            writer: Mutex::new(None),
        }
    }

    /// The recorder `config` asks for; a trace that cannot be opened is skipped.
    pub fn from_config(config: &ObservabilityConfig) -> Self {
        if !config.trace {
            return Self::disabled();
        }
        let path = crate::session::expand_home(&config.path).join("trace.jsonl");
        let mut recorder = Self {
            path: Some(path.clone()),
            redact: config.redact,
            max_field_chars: config.max_field_chars,
            max_bytes: config.max_trace_bytes,
            keep: config.keep_traces,
            writer: Mutex::new(None),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // A process that died over the cap must not keep appending to an oversized file.
        if recorder.max_bytes > 0 && file_size(&path) >= recorder.max_bytes {
            recorder.rotate_files();
        }
        *recorder
            .writer
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner()) = open_writer(&path);
        recorder
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Record one event: `ts`, `session`, and `kind`, then `fields` in order.
    pub fn event(&self, session: &str, kind: &str, fields: Vec<(&str, Value)>) {
        let Ok(mut guard) = self.writer.lock() else {
            return;
        };
        let Some(writer) = guard.as_mut() else {
            return;
        };
        let mut record = Map::new();
        record.insert("ts".to_owned(), json!(now()));
        record.insert("session".to_owned(), json!(session));
        record.insert("kind".to_owned(), json!(kind));
        for (name, value) in fields {
            record.insert(name.to_owned(), value);
        }
        let record = redact_event(&record, self.redact, self.max_field_chars);
        let line = format!("{}\n", python_json_dumps(&Value::Object(record), false));
        if writer
            .file
            .write_all(line.as_bytes())
            .and_then(|()| writer.file.flush())
            .is_err()
        {
            return;
        }
        writer.size += line.len() as u64;
        // Crossing the cap rotates for the next event; this one is already on disk.
        if self.max_bytes > 0 && writer.size >= self.max_bytes {
            *guard = None;
            self.rotate_files();
            if let Some(path) = &self.path {
                *guard = open_writer(path);
            }
        }
    }

    /// Shift the generations down and drop the oldest. The live file must be closed.
    fn rotate_files(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if self.keep == 0 {
            let _ = std::fs::remove_file(path);
            return;
        }
        let _ = std::fs::remove_file(rotated_path(path, self.keep));
        for index in (1..self.keep).rev() {
            let source = rotated_path(path, index);
            if source.exists() {
                replace(&source, &rotated_path(path, index + 1));
            }
        }
        if path.exists() {
            replace(path, &rotated_path(path, 1));
        }
    }
}

fn open_writer(path: &Path) -> Option<Writer> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    Some(Writer {
        file,
        size: file_size(path),
    })
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

// --------------------------------------------------------------------------
// Reading back
// --------------------------------------------------------------------------

/// Existing trace files, newest first: the live file, then `trace.1` ... `trace.N`.
pub fn trace_generations(path: &Path, keep: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if path.exists() {
        files.push(path.to_path_buf());
    }
    for index in 1..=keep {
        let candidate = rotated_path(path, index);
        if candidate.exists() {
            files.push(candidate);
        }
    }
    files
}

/// One trace file's records, skipping blank and malformed lines.
pub fn iter_events(path: &Path) -> impl Iterator<Item = Map<String, Value>> {
    let lines = File::open(path)
        .ok()
        .map(|file| BufReader::new(file).lines());
    lines.into_iter().flatten().filter_map(|line| {
        let line = line.ok()?;
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        match serde_json::from_str(line) {
            Ok(Value::Object(record)) => Some(record),
            _ => None,
        }
    })
}

/// Which events to read back.
#[derive(Debug, Clone, Default)]
pub struct TraceFilter {
    pub session: Option<String>,
    pub kinds: Option<Vec<String>>,
    pub after_ts: Option<f64>,
}

impl TraceFilter {
    fn matches(&self, record: &Map<String, Value>) -> bool {
        if let Some(session) = &self.session {
            if record.get("session").and_then(Value::as_str) != Some(session) {
                return false;
            }
        }
        if let Some(kinds) = &self.kinds {
            let kind = record
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !kinds.iter().any(|wanted| wanted == kind) {
                return false;
            }
        }
        if let Some(after) = self.after_ts {
            match record.get("ts").and_then(Value::as_f64) {
                Some(ts) if ts >= after => {}
                _ => return false,
            }
        }
        true
    }
}

/// The last `limit` matching events, in chronological order. Filtering happens
/// before the limit, generation by generation, newest first.
pub fn read_trace(
    path: &Path,
    filter: &TraceFilter,
    limit: usize,
    keep: usize,
) -> Vec<Map<String, Value>> {
    let mut events: Vec<Map<String, Value>> = Vec::new();
    for generation in trace_generations(path, keep) {
        let remaining = limit.saturating_sub(events.len());
        if remaining == 0 {
            break;
        }
        let mut chunk = std::collections::VecDeque::with_capacity(remaining);
        for record in iter_events(&generation).filter(|record| filter.matches(record)) {
            if chunk.len() == remaining {
                chunk.pop_front();
            }
            chunk.push_back(record);
        }
        chunk.extend(events);
        events = chunk.into_iter().collect();
    }
    events
}

/// `30s`, `15m`, `2h`, `7d`, or `1w` as seconds; a bare number means seconds.
pub fn parse_since(text: &str) -> Result<f64, String> {
    let trimmed = text.trim();
    let unit_start = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(unit_start);
    let scale = match unit.trim().to_lowercase().as_str() {
        "" | "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        "w" => 604800.0,
        _ => {
            return Err(format!(
                "{} is not a duration (try 30s, 15m, 2h, 7d)",
                python_repr(text)
            ))
        }
    };
    let number = number.trim();
    let valid = !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit() || c == '.')
        && number.matches('.').count() <= 1
        && !number.starts_with('.')
        && !number.ends_with('.');
    match number.parse::<f64>() {
        Ok(value) if valid => Ok(value * scale),
        _ => Err(format!(
            "{} is not a duration (try 30s, 15m, 2h, 7d)",
            python_repr(text)
        )),
    }
}

/// A JSON value as Python's `repr()` of the equivalent object writes it.
pub fn python_repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(int), _, _) => int.to_string(),
            (None, Some(int), _) => int.to_string(),
            (None, None, Some(float)) => python_float_repr(float),
            _ => number.to_string(),
        },
        Value::String(text) => python_repr(text),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(python_repr_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, item)| format!("{}: {}", python_repr(key), python_repr_value(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

// --------------------------------------------------------------------------
// OTLP JSON reconstruction
// --------------------------------------------------------------------------

/// Event kinds the OTLP reconstruction understands.
pub const SPAN_KINDS: &[&str] = &["turn_start", "turn_end", "model_call", "tool_call"];

/// A stable hex id derived from the event's identity, so re-exports are idempotent.
fn otlp_id(parts: &[String], width: usize) -> String {
    let digest = sha1_smol::Sha1::from(parts.join(":")).digest().to_string();
    digest[..width].to_owned()
}

fn otlp_value(value: &Value) -> Value {
    match value {
        Value::Bool(flag) => json!({ "boolValue": flag }),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            json!({ "intValue": number.to_string() })
        }
        Value::Number(number) => json!({ "doubleValue": number }),
        Value::Array(items) => {
            json!({ "arrayValue": { "values": items.iter().map(otlp_value).collect::<Vec<_>>() } })
        }
        Value::String(text) => json!({ "stringValue": text }),
        other => json!({ "stringValue": python_repr_value(other) }),
    }
}

fn otlp_attributes(attributes: &[(&str, Value)]) -> Value {
    Value::Array(
        attributes
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| json!({ "key": key, "value": otlp_value(value) }))
            .collect(),
    )
}

fn nanos(ts: &Value) -> String {
    ts.as_f64().map_or_else(
        || "0".to_owned(),
        |seconds| ((seconds * 1_000_000_000.0) as i128).to_string(),
    )
}

struct SpanSpec<'a> {
    name: &'a str,
    trace_id: &'a str,
    span_id: String,
    parent: Option<&'a str>,
    start: &'a Value,
    end: &'a Value,
    attributes: Vec<(&'a str, Value)>,
    status: u8,
}

fn span(spec: SpanSpec<'_>) -> Value {
    let mut span = Map::new();
    span.insert("traceId".to_owned(), json!(spec.trace_id));
    span.insert("spanId".to_owned(), json!(spec.span_id));
    span.insert("name".to_owned(), json!(spec.name));
    span.insert("kind".to_owned(), json!(1));
    span.insert("startTimeUnixNano".to_owned(), json!(nanos(spec.start)));
    span.insert("endTimeUnixNano".to_owned(), json!(nanos(spec.end)));
    span.insert("attributes".to_owned(), otlp_attributes(&spec.attributes));
    span.insert("status".to_owned(), json!({ "code": spec.status }));
    if let Some(parent) = spec.parent {
        span.insert("parentSpanId".to_owned(), json!(parent));
    }
    Value::Object(span)
}

fn field<'a>(event: &'a Map<String, Value>, name: &str) -> &'a Value {
    event.get(name).unwrap_or(&Value::Null)
}

/// Rebuild one session's span tree from its chronological events.
fn session_spans(session: &str, events: &[Map<String, Value>]) -> Vec<Value> {
    let trace_id = otlp_id(&[session.to_owned()], 32);
    let mut spans = Vec::new();
    let mut turn: Option<(String, Value, Value)> = None; // (id, start, task)
    let mut turns = 0;
    let mut previous = events
        .first()
        .map_or(json!(0.0), |event| field(event, "ts").clone());

    let close = |spans: &mut Vec<Value>,
                 turn: &mut Option<(String, Value, Value)>,
                 end: &Value,
                 extra: Vec<(&str, Value)>,
                 status: u8| {
        if let Some((id, start, task)) = turn.take() {
            let mut attributes = vec![("session.id", json!(session)), ("agent86.task", task)];
            attributes.extend(extra);
            spans.push(span(SpanSpec {
                name: "turn",
                trace_id: &trace_id,
                span_id: id,
                parent: None,
                start: &start,
                end,
                attributes,
                status,
            }));
        }
    };

    for (index, event) in events.iter().enumerate() {
        let kind = field(event, "kind").as_str().unwrap_or_default();
        let ts = event.get("ts").cloned().unwrap_or_else(|| previous.clone());
        match kind {
            "turn_start" => {
                // A turn with no turn_end (a crash) still gets a span.
                close(&mut spans, &mut turn, &ts, Vec::new(), 0);
                turns += 1;
                let id = otlp_id(
                    &[session.to_owned(), "turn".to_owned(), turns.to_string()],
                    16,
                );
                turn = Some((id, ts.clone(), field(event, "task").clone()));
            }
            "turn_end" => {
                let failed = matches!(field(event, "status").as_str(), Some("error" | "blocked"));
                let extra = vec![
                    ("agent86.status", field(event, "status").clone()),
                    ("agent86.steps", field(event, "steps").clone()),
                    ("agent86.reason", field(event, "reason").clone()),
                ];
                close(
                    &mut spans,
                    &mut turn,
                    &ts,
                    extra,
                    if failed { 2 } else { 1 },
                );
            }
            "model_call" => {
                let finish = match field(event, "stop_reason") {
                    Value::String(reason) if !reason.is_empty() => json!([reason]),
                    _ => Value::Null,
                };
                spans.push(span(SpanSpec {
                    name: "model_call",
                    trace_id: &trace_id,
                    span_id: otlp_id(
                        &[session.to_owned(), "model".to_owned(), index.to_string()],
                        16,
                    ),
                    parent: turn.as_ref().map(|(id, _, _)| id.as_str()),
                    start: &previous,
                    end: &ts,
                    attributes: vec![
                        ("gen_ai.request.model", field(event, "model").clone()),
                        (
                            "gen_ai.usage.input_tokens",
                            field(event, "input_tokens").clone(),
                        ),
                        (
                            "gen_ai.usage.output_tokens",
                            field(event, "output_tokens").clone(),
                        ),
                        ("gen_ai.response.finish_reasons", finish),
                        ("agent86.cost_usd", field(event, "cost_usd").clone()),
                        ("agent86.step", field(event, "step").clone()),
                    ],
                    status: 1,
                }));
            }
            "tool_call" => {
                let ok = field(event, "ok").as_bool().unwrap_or(false);
                spans.push(span(SpanSpec {
                    name: "tool_call",
                    trace_id: &trace_id,
                    span_id: otlp_id(
                        &[session.to_owned(), "tool".to_owned(), index.to_string()],
                        16,
                    ),
                    parent: turn.as_ref().map(|(id, _, _)| id.as_str()),
                    start: &previous,
                    end: &ts,
                    attributes: vec![
                        ("tool.name", field(event, "tool").clone()),
                        ("gen_ai.tool.name", field(event, "tool").clone()),
                        ("tool.ok", json!(ok)),
                        ("agent86.error", field(event, "error").clone()),
                    ],
                    status: if ok { 1 } else { 2 },
                }));
            }
            _ => {}
        }
        previous = ts;
    }
    let end = previous.clone();
    close(
        &mut spans,
        &mut turn,
        &end,
        vec![("agent86.status", json!("unfinished"))],
        0,
    );
    spans
}

/// Reconstructed spans in the OTLP/JSON envelope a collector accepts.
pub fn otlp_document(events: &[Map<String, Value>]) -> Value {
    let mut sessions: Vec<(String, Vec<Map<String, Value>>)> = Vec::new();
    for event in events {
        let session = match event.get("session") {
            Some(Value::String(session)) => session.clone(),
            Some(other) => python_repr_value(other),
            None => "unknown".to_owned(),
        };
        match sessions.iter_mut().find(|(name, _)| *name == session) {
            Some((_, list)) => list.push(event.clone()),
            None => sessions.push((session, vec![event.clone()])),
        }
    }
    let spans: Vec<Value> = sessions
        .iter()
        .flat_map(|(session, list)| session_spans(session, list))
        .collect();
    let version = env!("CARGO_PKG_VERSION");
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": otlp_attributes(&[
                    ("service.name", json!("agent86")),
                    ("service.version", json!(version)),
                ]),
            },
            "scopeSpans": [{
                "scope": { "name": "agent86", "version": version },
                "spans": spans,
            }],
        }],
    })
}
