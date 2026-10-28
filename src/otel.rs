//! OpenTelemetry spans, ported from Python's `observability/tracing.py` and the spans
//! `orchestration/loop.py` opens.
//!
//! With `[observability] otel = true`, each user turn is a `turn` span, parent of the
//! turn's `model_call` and `tool_call` spans, with Python's attribute names (the GenAI
//! semantic conventions where they have a word, `agent86.*` otherwise). A span that
//! ends with a failure gets the ERROR status and an `exception` event, as Python's
//! `start_as_current_span` records an exception that escapes it.
//!
//! `otel_exporter` picks where spans go: `otlp` (the default), `console` (stderr), or
//! `none` (recorded, sent nowhere). Python's OTLP exporter speaks gRPC and falls back
//! to HTTP; this one sends OTLP/HTTP with the JSON encoding, which the OpenTelemetry
//! Collector, Jaeger, and Tempo accept on port 4318 by default (a deliberate choice:
//! no gRPC stack). The endpoint is `otel_endpoint`, else the standard
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`, else
//! `http://localhost:4318`; headers come from `OTEL_EXPORTER_OTLP_HEADERS`.
//!
//! Spans are batched on a worker thread, as Python's `BatchSpanProcessor` does, and
//! flushed when a turn ends and when the tracer is dropped. Every failure is kept on
//! [`Tracer::note`]: a missing collector degrades to "no traces", never to "no agent".

use crate::config::{ObservabilityConfig, OtelExporter};
use crate::observability::otlp_attributes;
use crate::pyjson::python_json_dumps_indented;
use serde_json::{json, Map, Value};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Spans are exported at the latest when this many are waiting, as the SDK's batch size.
const MAX_BATCH: usize = 512;
/// How long a drop waits for the last batch, as the OTLP exporter's own timeout.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// An `exception` event, as the SDK's `record_exception` writes it.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanEvent {
    pub name: &'static str,
    pub time_ns: u128,
    pub attributes: Vec<(&'static str, Value)>,
}

/// A finished span, ready to export.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanData {
    pub name: &'static str,
    pub trace_id: String,
    pub span_id: String,
    pub parent: Option<String>,
    pub start_ns: u128,
    pub end_ns: u128,
    pub attributes: Vec<(&'static str, Value)>,
    pub events: Vec<SpanEvent>,
    /// OTLP's status code: 0 unset, 2 error.
    pub status: u8,
    pub description: Option<String>,
}

/// A span in progress, or nothing when tracing is off, so callers never branch.
#[derive(Debug, Default)]
pub struct Span(Option<SpanData>);

impl Span {
    /// Set attributes, replacing any of the same name. `null` values are skipped, as
    /// Python's `set_attributes` drops `None`.
    pub fn set(&mut self, attributes: Vec<(&'static str, Value)>) {
        let Some(span) = &mut self.0 else { return };
        for (key, value) in attributes {
            if value.is_null() {
                continue;
            }
            match span.attributes.iter_mut().find(|(name, _)| *name == key) {
                Some(slot) => slot.1 = value,
                None => span.attributes.push((key, value)),
            }
        }
    }

    /// Mark the span failed by an exception of Python type `kind`, as the SDK does
    /// when one escapes `start_as_current_span`.
    pub fn fail(&mut self, kind: &str, message: &str) {
        let Some(span) = &mut self.0 else { return };
        span.status = 2;
        span.description = Some(format!("{kind}: {message}"));
        span.events.push(SpanEvent {
            name: "exception",
            time_ns: now_ns(),
            attributes: vec![
                ("exception.type", json!(kind)),
                ("exception.message", json!(message)),
            ],
        });
    }
}

enum Message {
    Span(Box<SpanData>),
    Flush,
}

/// Where spans go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    Otlp {
        url: String,
        headers: Vec<(String, String)>,
        timeout: Duration,
    },
    Console,
    Nowhere,
}

struct Worker {
    sender: mpsc::Sender<Message>,
    done: mpsc::Receiver<()>,
}

/// The tracer handle the harness holds; [`Tracer::default`] records nothing.
#[derive(Default)]
pub struct Tracer {
    worker: Mutex<Option<Worker>>,
    /// The running turn's (trace id, span id), parent of its model and tool spans.
    turn: Mutex<Option<(String, String)>>,
    note: Arc<Mutex<Option<String>>>,
}

impl Tracer {
    /// The tracer `[observability]` asks for, as Python's `build_tracer`.
    pub fn from_config(config: &ObservabilityConfig) -> Self {
        if !config.otel {
            return Self::default();
        }
        let destination = match config.otel_exporter {
            OtelExporter::Otlp => Destination::Otlp {
                url: otlp_url(config.otel_endpoint.as_deref(), &|name| {
                    std::env::var(name).ok().filter(|value| !value.is_empty())
                }),
                headers: otlp_headers(&|name| std::env::var(name).ok()),
                timeout: otlp_timeout(&|name| std::env::var(name).ok()),
            },
            OtelExporter::Console => Destination::Console,
            OtelExporter::None => Destination::Nowhere,
        };
        Self::to(destination)
    }

    /// A tracer that sends its spans to `destination`.
    pub fn to(destination: Destination) -> Self {
        let tracer = Self::default();
        let note = Arc::clone(&tracer.note);
        let (sender, inbox) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("agent86-otel".into())
            .spawn(move || {
                export_loop(&destination, &inbox, &note);
                let _ = finished.send(());
            });
        match spawned {
            Ok(_) => *tracer.lock_worker() = Some(Worker { sender, done }),
            Err(error) => tracer.set_note(format!("otel disabled: {error}")),
        }
        tracer
    }

    fn lock_worker(&self) -> std::sync::MutexGuard<'_, Option<Worker>> {
        self.worker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether spans are recorded.
    pub fn active(&self) -> bool {
        self.lock_worker().is_some()
    }

    /// Why spans are not reaching their destination, if anything went wrong.
    pub fn note(&self) -> Option<String> {
        self.note
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_note(&self, note: String) {
        *self.note.lock().unwrap_or_else(PoisonError::into_inner) = Some(note);
    }

    /// Open a span. A `turn` span starts a new trace and becomes the parent of the
    /// spans opened until it ends; any other span is a child of the running turn.
    pub fn start(&self, name: &'static str, attributes: Vec<(&'static str, Value)>) -> Span {
        if !self.active() {
            return Span(None);
        }
        let mut turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        let span_id = random_hex(16);
        let (trace_id, parent) = match (&*turn, name) {
            (Some((trace_id, parent)), name) if name != "turn" => {
                (trace_id.clone(), Some(parent.clone()))
            }
            _ => (random_hex(32), None),
        };
        if name == "turn" {
            *turn = Some((trace_id.clone(), span_id.clone()));
        }
        let mut span = Span(Some(SpanData {
            name,
            trace_id,
            span_id,
            parent,
            start_ns: now_ns(),
            end_ns: 0,
            attributes: Vec::new(),
            events: Vec::new(),
            status: 0,
            description: None,
        }));
        span.set(attributes);
        span
    }

    /// Close a span and hand it to the exporter; a `turn` span also flushes the batch.
    pub fn end(&self, span: Span) {
        let Some(mut span) = span.0 else { return };
        span.end_ns = now_ns();
        let turn = span.name == "turn";
        if turn {
            let mut current = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
            if current.as_ref().is_some_and(|(_, id)| *id == span.span_id) {
                *current = None;
            }
        }
        if let Some(worker) = &*self.lock_worker() {
            let _ = worker.sender.send(Message::Span(Box::new(span)));
            if turn {
                let _ = worker.sender.send(Message::Flush);
            }
        }
    }
}

impl Drop for Tracer {
    /// Export what is left, as Python's provider shutdown does at exit, waiting at
    /// most the exporter's own timeout.
    fn drop(&mut self) {
        if let Some(Worker { sender, done }) = self.lock_worker().take() {
            drop(sender);
            let _ = done.recv_timeout(FLUSH_TIMEOUT + Duration::from_secs(1));
        }
    }
}

fn export_loop(
    destination: &Destination,
    inbox: &mpsc::Receiver<Message>,
    note: &Mutex<Option<String>>,
) {
    let mut batch: Vec<SpanData> = Vec::new();
    let export = |batch: &mut Vec<SpanData>| {
        if batch.is_empty() {
            return;
        }
        let spans = std::mem::take(batch);
        if let Err(error) = send(destination, &spans) {
            *note.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
        }
    };
    // The channel closes when the tracer is dropped: export what is left, then stop.
    while let Ok(message) = inbox.recv() {
        match message {
            Message::Span(span) => {
                batch.push(*span);
                if batch.len() >= MAX_BATCH {
                    export(&mut batch);
                }
            }
            Message::Flush => export(&mut batch),
        }
    }
    export(&mut batch);
}

fn send(destination: &Destination, spans: &[SpanData]) -> Result<(), String> {
    match destination {
        Destination::Nowhere => Ok(()),
        Destination::Console => {
            // stderr, never stdout: stdout is the answer and the `run --json` contract.
            for span in spans {
                eprintln!("{}", console_json(span));
            }
            Ok(())
        }
        Destination::Otlp {
            url,
            headers,
            timeout,
        } => post(url, headers, *timeout, &otlp_request(spans)),
    }
}

fn post(
    url: &str,
    headers: &[(String, String)],
    timeout: Duration,
    body: &Value,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("otel: {error}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| format!("otel: {error}"))?;
        let mut request = client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string());
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("otel: export to {url} failed: {error}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "otel: export to {url} failed: HTTP {}",
                response.status().as_u16()
            ))
        }
    })
}

/// The traces endpoint: `otel_endpoint` (as a base, unless it already names
/// `/v1/traces`), else `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` as is, else
/// `OTEL_EXPORTER_OTLP_ENDPOINT` plus `/v1/traces`, else the local collector.
pub fn otlp_url(endpoint: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> String {
    let base = |base: &str| {
        if base.trim_end_matches('/').ends_with("/v1/traces") {
            base.to_owned()
        } else {
            format!("{}/v1/traces", base.trim_end_matches('/'))
        }
    };
    if let Some(endpoint) = endpoint.filter(|endpoint| !endpoint.is_empty()) {
        return base(endpoint);
    }
    if let Some(endpoint) = env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
        return endpoint;
    }
    if let Some(endpoint) = env("OTEL_EXPORTER_OTLP_ENDPOINT") {
        return base(&endpoint);
    }
    "http://localhost:4318/v1/traces".to_owned()
}

/// `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, else `OTEL_EXPORTER_OTLP_HEADERS`: comma-separated
/// `key=value` pairs, percent-decoded, as the OpenTelemetry specification defines them.
pub fn otlp_headers(env: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let Some(text) = env("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
        .filter(|text| !text.trim().is_empty())
        .or_else(|| env("OTEL_EXPORTER_OTLP_HEADERS"))
    else {
        return Vec::new();
    };
    text.split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = percent_decode(key.trim());
            (!key.is_empty()).then(|| (key, percent_decode(value.trim())))
        })
        .collect()
}

/// `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT`, else `OTEL_EXPORTER_OTLP_TIMEOUT`, in
/// milliseconds; 10 seconds by default.
pub fn otlp_timeout(env: &dyn Fn(&str) -> Option<String>) -> Duration {
    [
        "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
        "OTEL_EXPORTER_OTLP_TIMEOUT",
    ]
    .iter()
    .find_map(|name| env(name)?.trim().parse::<u64>().ok())
    .map_or(FLUSH_TIMEOUT, Duration::from_millis)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn resource_attributes() -> Vec<(&'static str, Value)> {
    vec![
        ("service.name", json!("agent86")),
        ("service.version", json!(env!("CARGO_PKG_VERSION"))),
        ("telemetry.sdk.language", json!("rust")),
    ]
}

/// The OTLP/JSON `ExportTraceServiceRequest` for `spans`.
pub fn otlp_request(spans: &[SpanData]) -> Value {
    let spans: Vec<Value> = spans
        .iter()
        .map(|span| {
            let mut out = Map::new();
            out.insert("traceId".into(), json!(span.trace_id));
            out.insert("spanId".into(), json!(span.span_id));
            if let Some(parent) = &span.parent {
                out.insert("parentSpanId".into(), json!(parent));
            }
            out.insert("name".into(), json!(span.name));
            out.insert("kind".into(), json!(1));
            out.insert("startTimeUnixNano".into(), json!(span.start_ns.to_string()));
            out.insert("endTimeUnixNano".into(), json!(span.end_ns.to_string()));
            out.insert("attributes".into(), otlp_attributes(&span.attributes));
            let events: Vec<Value> = span
                .events
                .iter()
                .map(|event| {
                    json!({
                        "timeUnixNano": event.time_ns.to_string(),
                        "name": event.name,
                        "attributes": otlp_attributes(&event.attributes),
                    })
                })
                .collect();
            if !events.is_empty() {
                out.insert("events".into(), Value::Array(events));
            }
            let mut status = Map::new();
            status.insert("code".into(), json!(span.status));
            if let Some(description) = &span.description {
                status.insert("message".into(), json!(description));
            }
            out.insert("status".into(), Value::Object(status));
            Value::Object(out)
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": { "attributes": otlp_attributes(&resource_attributes()) },
            "scopeSpans": [{ "scope": { "name": "agent86" }, "spans": spans }],
        }],
    })
}

/// A span as Python's `ConsoleSpanExporter` prints it (`ReadableSpan.to_json()`).
pub fn console_json(span: &SpanData) -> String {
    let attributes = |attributes: &[(&str, Value)]| -> Value {
        Value::Object(
            attributes
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        )
    };
    let mut status = Map::new();
    status.insert(
        "status_code".into(),
        json!(if span.status == 2 { "ERROR" } else { "UNSET" }),
    );
    if let Some(description) = &span.description {
        status.insert("description".into(), json!(description));
    }
    let document = json!({
        "name": span.name,
        "context": {
            "trace_id": format!("0x{}", span.trace_id),
            "span_id": format!("0x{}", span.span_id),
            "trace_state": "[]",
        },
        "kind": "SpanKind.INTERNAL",
        "parent_id": span.parent.as_ref().map(|parent| format!("0x{parent}")),
        "start_time": iso_time(span.start_ns),
        "end_time": iso_time(span.end_ns),
        "status": Value::Object(status),
        "attributes": attributes(&span.attributes),
        "events": span.events.iter().map(|event| json!({
            "name": event.name,
            "timestamp": iso_time(event.time_ns),
            "attributes": attributes(&event.attributes),
        })).collect::<Vec<_>>(),
        "links": [],
        "resource": {
            "attributes": attributes(&resource_attributes()),
            "schema_url": "",
        },
    });
    python_json_dumps_indented(&document, true, 4)
}

/// Nanoseconds since the epoch as Python's `ns_to_iso_str` writes them.
fn iso_time(ns: u128) -> String {
    let micros = (ns + 500) / 1_000;
    let seconds = (micros / 1_000_000) as i64;
    let micro = (micros % 1_000_000) as u32;
    chrono::DateTime::from_timestamp(seconds, micro * 1_000).map_or_else(String::new, |time| {
        time.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
    })
}

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// `width` random hex digits (16 for a span id, 32 for a trace id), never all zero.
fn random_hex(width: usize) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let mut out = String::new();
    while out.len() < width {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(now_ns());
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        out.push_str(&format!("{:016x}", hasher.finish() | 1));
    }
    out.truncate(width);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_endpoint_follows_config_then_the_standard_variables() {
        assert_eq!(otlp_url(None, &env(&[])), "http://localhost:4318/v1/traces");
        assert_eq!(
            otlp_url(
                None,
                &env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318/")])
            ),
            "http://c:4318/v1/traces"
        );
        assert_eq!(
            otlp_url(
                None,
                &env(&[
                    ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                    ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://t:9/custom"),
                ])
            ),
            "http://t:9/custom"
        );
        let configured = env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318")]);
        assert_eq!(
            otlp_url(Some("https://otel.example"), &configured),
            "https://otel.example/v1/traces"
        );
        assert_eq!(
            otlp_url(Some("https://otel.example/v1/traces"), &configured),
            "https://otel.example/v1/traces"
        );
    }

    #[test]
    fn headers_and_timeout_come_from_the_standard_variables() {
        let headers = otlp_headers(&env(&[(
            "OTEL_EXPORTER_OTLP_HEADERS",
            "api-key=abc%3D, x-team = core ,broken",
        )]));
        assert_eq!(
            headers,
            [
                ("api-key".to_owned(), "abc=".to_owned()),
                ("x-team".to_owned(), "core".to_owned())
            ]
        );
        assert_eq!(otlp_timeout(&env(&[])), Duration::from_secs(10));
        assert_eq!(
            otlp_timeout(&env(&[("OTEL_EXPORTER_OTLP_TIMEOUT", "2500")])),
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn spans_nest_under_the_running_turn_and_skip_nulls() {
        let tracer = Tracer::to(Destination::Nowhere);
        let mut turn = tracer.start("turn", vec![("session.id", json!("s1"))]);
        let model = tracer.start("model_call", vec![("gen_ai.system", Value::Null)]);
        let (turn_trace, turn_id) = {
            let data = turn.0.as_ref().unwrap();
            (data.trace_id.clone(), data.span_id.clone())
        };
        let child = model.0.as_ref().unwrap();
        assert_eq!(child.trace_id, turn_trace);
        assert_eq!(child.parent.as_deref(), Some(turn_id.as_str()));
        assert!(child.attributes.is_empty(), "nulls are dropped");
        assert_eq!((turn_trace.len(), turn_id.len()), (32, 16));
        tracer.end(model);
        turn.set(vec![("session.id", json!("s2"))]);
        assert_eq!(
            turn.0.as_ref().unwrap().attributes,
            [("session.id", json!("s2"))]
        );
        tracer.end(turn);
        // With no turn running, a span starts a trace of its own.
        let orphan = tracer.start("tool_call", Vec::new());
        assert_eq!(orphan.0.as_ref().unwrap().parent, None);
        assert!(!Tracer::default().active());
        assert!(Tracer::default().start("turn", Vec::new()).0.is_none());
    }

    fn sample() -> SpanData {
        let mut span = Span(Some(SpanData {
            name: "model_call",
            trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
            span_id: "b7ad6b7169203331".into(),
            parent: Some("00f067aa0ba902b7".into()),
            start_ns: 1_700_000_000_123_456_789,
            end_ns: 1_700_000_001_000_000_000,
            attributes: Vec::new(),
            events: Vec::new(),
            status: 0,
            description: None,
        }));
        span.set(vec![
            ("gen_ai.request.model", json!("gpt-4o-mini")),
            ("gen_ai.usage.input_tokens", json!(12)),
            ("gen_ai.response.finish_reasons", json!(["stop"])),
            ("agent86.cost_usd", json!(1.44e-05)),
            ("agent86.completed", json!(true)),
        ]);
        span.fail("ProviderError", "HTTP 500");
        let mut data = span.0.unwrap();
        data.events[0].time_ns = 1_700_000_000_900_000_000;
        data
    }

    #[test]
    fn the_otlp_request_carries_ids_attributes_status_and_events() {
        let document = otlp_request(&[sample()]);
        let span = &document["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["parentSpanId"], "00f067aa0ba902b7");
        assert_eq!(span["startTimeUnixNano"], "1700000000123456789");
        assert_eq!(
            span["attributes"][1],
            json!({"key": "gen_ai.usage.input_tokens", "value": {"intValue": "12"}})
        );
        assert_eq!(
            span["attributes"][2]["value"],
            json!({"arrayValue": {"values": [{"stringValue": "stop"}]}})
        );
        assert_eq!(
            span["status"],
            json!({"code": 2, "message": "ProviderError: HTTP 500"})
        );
        assert_eq!(span["events"][0]["name"], "exception");
        assert_eq!(
            document["resourceSpans"][0]["resource"]["attributes"][0],
            json!({"key": "service.name", "value": {"stringValue": "agent86"}})
        );
    }

    #[test]
    fn the_console_shows_spans_as_python_s_sdk_prints_them() {
        let text = console_json(&sample());
        assert!(text.starts_with("{\n    \"name\": \"model_call\",\n    \"context\": {\n        \"trace_id\": \"0x0af7651916cd43dd8448eb211c80319c\","), "{text}");
        assert!(text.contains("\"kind\": \"SpanKind.INTERNAL\""), "{text}");
        assert!(
            text.contains("\"parent_id\": \"0x00f067aa0ba902b7\""),
            "{text}"
        );
        assert!(
            text.contains("\"start_time\": \"2023-11-14T22:13:20.123457Z\""),
            "{text}"
        );
        assert!(
            text.contains(
                "\"status_code\": \"ERROR\",\n        \"description\": \"ProviderError: HTTP 500\""
            ),
            "{text}"
        );
        assert!(text.contains("\"agent86.cost_usd\": 1.44e-05"), "{text}");
        assert!(
            text.contains("\"gen_ai.response.finish_reasons\": [\n            \"stop\"\n        ]"),
            "{text}"
        );
    }
}
