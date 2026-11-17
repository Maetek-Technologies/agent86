//! The live Gemini check for 1.0.24 (opt-in, never in CI): what the real API
//! accepts, before the Google adapter's tool support is built on it.
//!
//! Runs only with `AGENT86_TEST_GEMINI_LIVE=1`. The key comes from
//! `GEMINI_API_KEY`, else the OS keyring (service `agent86`, account `google`,
//! where `agent86 keys set google` puts it); it goes only in the
//! `x-goog-api-key` header and is redacted from everything printed. The model is
//! `AGENT86_TEST_GEMINI_MODEL`, else `gemini-3.1-flash-lite`. Every request has
//! `store: false` and sends only tool schemas and short prompts.
//!
//! ```text
//! AGENT86_TEST_GEMINI_LIVE=1 cargo test --test google_live -- --nocapture
//! ```

use agent86::secrets::{redact, resolve_api_key, OsKeyring};
use agent86::tools::default_registry;
use serde_json::{json, Value};
use std::time::Duration;

const BASE: &str = "https://generativelanguage.googleapis.com/v1";
const PLACEHOLDER: &str = "skip_thought_signature_validator";

struct Live {
    client: reqwest::Client,
    key: String,
    model: String,
}

impl Live {
    /// POST `body` to `generateContent`; the status and the body, redacted. A 429
    /// is waited out and tried again, twice.
    async fn generate(&self, body: &Value) -> (u16, Value) {
        let url = format!("{BASE}/models/{}:generateContent", self.model);
        for attempt in 0..3 {
            let response = self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.key)
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .expect("the request is sent");
            let status = response.status().as_u16();
            let text = redact(&response.text().await.unwrap_or_default(), &[&self.key]);
            if status == 429 && attempt < 2 {
                tokio::time::sleep(Duration::from_secs(20)).await;
                continue;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            return (
                status,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            );
        }
        unreachable!()
    }
}

fn error_message(body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .map_or_else(|| body.to_string(), str::to_owned)
        .chars()
        .take(300)
        .collect()
}

fn request(declarations: Value, prompt: &str) -> Value {
    json!({
        "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        "tools": [{"functionDeclarations": declarations}],
        "generationConfig": {"maxOutputTokens": 64},
        "store": false,
    })
}

/// Schemas as MCP servers write them, with what pydantic leaves out.
fn mcp_samples() -> Vec<(&'static str, Value)> {
    vec![
        (
            "ref_and_defs",
            json!({"type": "object", "properties": {"item": {"$ref": "#/$defs/Item"}},
                   "$defs": {"Item": {"type": "object", "properties": {"name": {"type": "string"}},
                                      "required": ["name"]}}}),
        ),
        (
            "additional_false_and_default",
            json!({"type": "object", "properties": {"query": {"type": "string", "default": "x"},
                                                    "limit": {"type": "integer", "minimum": 1}},
                   "required": ["query"], "additionalProperties": false}),
        ),
        (
            "schema_title_anyof_null",
            json!({"$schema": "http://json-schema.org/draft-07/schema#", "title": "Args",
                   "type": "object",
                   "properties": {"path": {"anyOf": [{"type": "string"}, {"type": "null"}],
                                           "title": "Path"}}}),
        ),
        (
            "type_array_with_null",
            json!({"type": "object", "properties": {"tags": {"type": ["array", "null"],
                                                             "items": {"type": "string"}}}}),
        ),
        ("bare_object", json!({"type": "object"})),
        (
            "empty_properties",
            json!({"type": "object", "properties": {}}),
        ),
        (
            "additional_schema_map",
            json!({"type": "object", "properties": {"env": {"type": "object",
                                                            "additionalProperties": {"type": "string"}}}}),
        ),
        (
            "oneof_const_pattern",
            json!({"type": "object", "properties": {"mode": {"oneOf": [{"const": "a"}, {"const": "b"}]},
                                                    "id": {"type": "string", "pattern": "^[a-z]+$"}}}),
        ),
    ]
}

#[tokio::test]
async fn gemini_accepts_our_schemas_and_wants_its_signatures_back() {
    if std::env::var("AGENT86_TEST_GEMINI_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT86_TEST_GEMINI_LIVE=1 to run the live Gemini check");
        return;
    }
    let key = resolve_api_key("google", Some("GEMINI_API_KEY"), &OsKeyring)
        .expect("a key in GEMINI_API_KEY or the keyring (agent86 keys set google)");
    let live = Live {
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap(),
        key,
        model: std::env::var("AGENT86_TEST_GEMINI_MODEL")
            .unwrap_or_else(|_| "gemini-3.1-flash-lite".to_owned()),
    };
    println!("model: {} on {BASE}", live.model);

    // 1. Each schema alone, through parametersJsonSchema.
    let specs = default_registry(true, true, true, true).specs();
    let mut declarations = Vec::new();
    let mut refused = Vec::new();
    println!("\n-- schemas, one per request (parametersJsonSchema)");
    let mut cases: Vec<(String, Value)> = specs
        .iter()
        .map(|spec| (spec.name.clone(), spec.parameters.clone()))
        .collect();
    cases.extend(
        mcp_samples()
            .into_iter()
            .map(|(name, schema)| (format!("mcp_{name}"), schema)),
    );
    cases.push(("1starts_with_digit".to_owned(), json!({"type": "object"})));
    for (name, schema) in &cases {
        let declaration = json!({"name": name, "description": "A test tool.",
                                 "parametersJsonSchema": schema});
        let (status, body) = live
            .generate(&request(
                json!([declaration]),
                "Reply with the single word ok.",
            ))
            .await;
        if status == 200 {
            println!("ok    {name}");
        } else {
            println!("{status}   {name}: {}", error_message(&body));
            refused.push(name.clone());
        }
        if specs.iter().any(|spec| &spec.name == name) {
            declarations.push(declaration);
        }
    }

    // 2. Every built-in tool together, as the adapter will send them.
    println!(
        "\n-- all {} built-in tools in one request",
        declarations.len()
    );
    let (status, body) = live
        .generate(&request(
            json!(declarations),
            "Reply with the single word ok.",
        ))
        .await;
    println!(
        "{status}   {}",
        if status == 200 {
            "ok".to_owned()
        } else {
            error_message(&body)
        }
    );
    assert_eq!(status, 200, "the built-in tools together");

    // 3. A forced call, to see the id and the thought signature.
    println!("\n-- a forced call to list_dir");
    let mut forced = request(
        json!(declarations),
        "List the files in the current directory.",
    );
    forced["toolConfig"] =
        json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["list_dir"]}});
    forced["generationConfig"]["maxOutputTokens"] = json!(1024);
    let (status, body) = live.generate(&forced).await;
    assert_eq!(status, 200, "{}", error_message(&body));
    let parts = body
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let calls: Vec<&Value> = parts
        .iter()
        .filter(|part| part.get("functionCall").is_some())
        .collect();
    let first = calls.first().expect("a forced call comes back as a call");
    let id = first.pointer("/functionCall/id").and_then(Value::as_str);
    let signed: Vec<bool> = calls
        .iter()
        .map(|part| part.get("thoughtSignature").is_some())
        .collect();
    println!(
        "calls: {}, id on the first: {}, signed: {signed:?}, other parts: {}, finishReason: {}",
        calls.len(),
        id.is_some(),
        parts.len() - calls.len(),
        body.pointer("/candidates/0/finishReason")
            .unwrap_or(&Value::Null),
    );
    println!(
        "usage: {}",
        body.get("usageMetadata").unwrap_or(&Value::Null)
    );

    // 4. The step sent back: as returned, without the signature, with the
    // placeholder, and without the call id in the response.
    let response_part = |with_id: bool| {
        let mut response = json!({"name": "list_dir", "response": {"result": "a.txt\nb.txt"}});
        if let (true, Some(id)) = (with_id, id) {
            response["id"] = json!(id);
        }
        json!({"functionResponse": response})
    };
    let follow_up = |model_parts: Vec<Value>, with_id: bool| {
        let mut body = request(
            json!(declarations),
            "List the files in the current directory.",
        );
        let contents = body["contents"].as_array_mut().unwrap();
        contents.push(json!({"role": "model", "parts": model_parts}));
        contents.push(json!({"role": "user", "parts": [response_part(with_id)]}));
        body["generationConfig"]["maxOutputTokens"] = json!(256);
        body
    };
    let stripped: Vec<Value> = parts
        .iter()
        .map(|part| {
            let mut part = part.clone();
            if let Some(object) = part.as_object_mut() {
                object.remove("thoughtSignature");
            }
            part
        })
        .collect();
    let placeholder: Vec<Value> = stripped
        .iter()
        .map(|part| {
            let mut part = part.clone();
            if part.get("functionCall").is_some() {
                part["thoughtSignature"] = json!(PLACEHOLDER);
            }
            part
        })
        .collect();
    println!("\n-- the step sent back");
    let mut results = Vec::new();
    for (label, model_parts, with_id) in [
        ("as returned", parts.clone(), true),
        ("signature removed", stripped, true),
        ("placeholder signature", placeholder, true),
        ("as returned, response without id", parts.clone(), false),
    ] {
        let (status, body) = live.generate(&follow_up(model_parts, with_id)).await;
        let detail = if status == 200 {
            format!(
                "finishReason {}",
                body.pointer("/candidates/0/finishReason")
                    .unwrap_or(&Value::Null)
            )
        } else {
            error_message(&body)
        };
        println!("{status}   {label}: {detail}");
        results.push((label, status));
    }
    assert_eq!(results[0].1, 200, "the step as returned");
    assert_eq!(results[2].1, 200, "the placeholder Google documents");

    println!("\nrefused schemas: {refused:?}");
}
