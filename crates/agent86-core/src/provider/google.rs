//! The Google provider (beyond Python): Gemini on the Gemini API's
//! `generateContent`, streamed, under `google:<model>`.
//!
//! Gemini's OpenAI-compatible endpoint loses the thought signature Gemini 3 and
//! later attach to a step's function calls, and the next request in the turn
//! fails without it, so this is an adapter of its own. Facts below were checked
//! against Google's documentation (now at `aistudio.google.com`) on 2026-11-17.
//!
//! - `POST {base_url}/models/{model}:streamGenerateContent?alt=sse` on `v1`, the
//!   stable version, which has function calling, thinking, and `store`. The key
//!   goes in the `x-goog-api-key` header, never in the URL, so it cannot leak into
//!   an error, a trace, or a log.
//! - The full history is sent every time; the stateful Interactions API is not
//!   used, since agent86 keeps the conversation in its own session.
//! - `store: false` on every request unless `[providers.google] store = true`:
//!   since 2026-10-26 Google keeps generateContent requests of new paid projects
//!   in AI Studio's logs for 55 days by default, and a request holds every file
//!   the agent read.
//! - No `temperature`, `top_p`, or `top_k`: Gemini 4 and 3.6+ ignore them (and
//!   will refuse them), and Google advises keeping earlier Gemini 3 models at the
//!   defaults. `thinkingConfig.includeThoughts` asks for thought summaries, which
//!   arrive as parts with `thought: true` and go to the reasoning; the level is
//!   left to each model's default.
//! - Retries follow `retry.rs` (429 and 5xx, never once text was shown); a 402
//!   (prepaid credits used up) is not retried and says so.
//!
//! Tools, as checked against the live API on 2026-11-17 (`tests/google_live.rs`,
//! on `gemini-3.1-flash-lite` and `gemini-4-hydrogen`):
//!
//! - Declared through `parametersJsonSchema`, unchanged: every built-in schema and
//!   MCP-shaped ones (`$ref`/`$defs`, `additionalProperties`, `anyOf` with null,
//!   type arrays, `oneOf`/`const`/`pattern`, `default`, `$schema`) were accepted.
//! - A function name must start with a letter or `_`; a tool whose name does not
//!   (an MCP name such as `1password-get`) is declared as `_<name>`, and the
//!   model's calls are mapped back ([`gemini_name`]).
//! - Every call came back with an `id` and its step's first call with a
//!   `thoughtSignature`. The step sent back without the signature is a 400
//!   ("Function call is missing a thought_signature"); with it, or with
//!   [`SKIP_SIGNATURE`], it passes, and the response passes with or without the
//!   call's id. The adapter keeps each call's signature in memory, keyed by the
//!   call id, and sends it back on the same part, with the id when Google issued
//!   it. Signatures are not saved with the session: Google checks only the
//!   current turn, and a turn never outlives the process yet, so a call with no
//!   signature kept (another provider's, a resumed session's) gets the
//!   placeholder.

use super::openai::error_chain;
use super::retry::{is_retryable_status, parse_retry_after, RetryPolicy};
use super::{DeltaStream, Provider, ProviderError};
use crate::pricing::PriceTable;
use crate::secrets::redact;
use crate::types::{
    Completion, CompletionDelta, CompletionRequest, Message, Role, StopReason, ToolCall, Usage,
    INVALID_TOOL_ARGS_KEY,
};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// The stable Gemini API.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1";
pub const DEFAULT_API_KEY_ENV: &str = "GEMINI_API_KEY";

/// Google's documented thought signature for a function call that carries none
/// (a call from another provider, or from a session saved before signatures
/// were kept); Google validates only the current turn.
pub const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// The prefix of a stop reason that means the provider ended the reply itself
/// (`stopped:SAFETY`); the loop announces it and does not ask again.
pub const STOPPED_PREFIX: &str = "stopped:";

/// How much of an error body is kept.
const ERROR_BODY_LIMIT: usize = 2000;

/// Calls remembered before those no longer in the history are dropped.
const REMEMBERED_CALLS: usize = 4096;

/// What the adapter keeps about a call Gemini made.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallMeta {
    signature: Option<String>,
    /// The id is Google's, so it goes back with the call and its response.
    google_id: bool,
}

/// Settings from `[providers.google]` this adapter uses.
#[derive(Debug, Clone, PartialEq)]
pub struct GoogleConfig {
    pub base_url: Option<String>,
    pub api_key_env: String,
    /// The section name, which is also the keyring account.
    pub config_name: String,
    pub max_tokens: Option<u32>,
    pub connect_timeout: Duration,
    /// The longest gap between streamed chunks.
    pub read_timeout: Duration,
    pub max_retries: u32,
    /// Let Google keep the requests (`store`); off unless the section says so.
    pub store: bool,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_env: DEFAULT_API_KEY_ENV.to_owned(),
            config_name: "google".to_owned(),
            max_tokens: None,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(1200),
            max_retries: super::retry::DEFAULT_MAX_RETRIES,
            store: false,
        }
    }
}

pub struct GoogleProvider {
    model: String,
    url: String,
    api_key: String,
    config: GoogleConfig,
    client: reqwest::Client,
    pricing: PriceTable,
    price_ref: String,
    /// Each call's signature and id kind, by call id, for sending the step back.
    calls: Mutex<HashMap<String, CallMeta>>,
    /// For ids made up for calls Gemini sent without one (Gemini 2.5).
    next_id: AtomicU64,
}

impl fmt::Debug for GoogleProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleProvider")
            .field("model", &self.model)
            .field("url", &self.url)
            .field("api_key", &"***redacted***")
            .field("config", &self.config)
            .finish()
    }
}

impl GoogleProvider {
    pub fn new(
        model: impl Into<String>,
        config: GoogleConfig,
        api_key: Option<String>,
    ) -> Result<Self, ProviderError> {
        let Some(api_key) = api_key.filter(|key| !key.is_empty()) else {
            return Err(ProviderError::MissingApiKey {
                env: config.api_key_env.clone(),
                account: config.config_name.clone(),
            });
        };
        let model = model.into();
        // `models/gemini-...`, as Google's own list names them, is the same model.
        let bare = model.strip_prefix("models/").unwrap_or(&model).to_owned();
        let base = config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}/models/{bare}:streamGenerateContent?alt=sse");
        let client = build_client(&config)?;
        Ok(Self {
            price_ref: bare.clone(),
            model: bare,
            url,
            api_key,
            config,
            client,
            pricing: PriceTable::builtin(),
            calls: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        })
    }

    /// Price usage with `pricing`, looked up under `price_ref` (the full ref).
    pub fn with_pricing(mut self, pricing: PriceTable, price_ref: impl Into<String>) -> Self {
        self.pricing = pricing;
        self.price_ref = price_ref.into();
        self
    }

    /// The streaming endpoint, which never carries the key.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The `generateContent` body for `request`.
    pub fn payload(&self, request: &CompletionRequest) -> Value {
        self.prepare(request).0
    }

    /// The body, and the declared name of each renamed tool to its own name.
    fn prepare(&self, request: &CompletionRequest) -> (Value, HashMap<String, String>) {
        let mut declared: HashMap<String, String> = HashMap::new();
        let mut taken: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
        let mut rename = |name: &str| -> String {
            if let Some(known) = declared.get(name) {
                return known.clone();
            }
            let wire = gemini_name(name, &taken);
            if wire != name {
                taken.push(wire.clone());
            }
            declared.insert(name.to_owned(), wire.clone());
            wire
        };
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": rename(&tool.name),
                    "description": tool.description,
                    "parametersJsonSchema": tool.parameters,
                })
            })
            .collect();
        let calls = {
            let mut calls = self.calls.lock().unwrap_or_else(|p| p.into_inner());
            if calls.len() > REMEMBERED_CALLS {
                let live: std::collections::HashSet<&str> = request
                    .messages
                    .iter()
                    .flat_map(|message| message.tool_calls.iter().map(|call| call.id.as_str()))
                    .collect();
                calls.retain(|id, _| live.contains(id.as_str()));
            }
            calls.clone()
        };
        let (system, contents) = to_contents_with(&request.messages, &calls, &mut rename);
        let mut generation = json!({"thinkingConfig": {"includeThoughts": true}});
        let cap = request
            .max_tokens
            .filter(|&cap| cap > 0)
            .or(self.config.max_tokens.filter(|&cap| cap > 0));
        if let Some(cap) = cap {
            generation["maxOutputTokens"] = json!(cap);
        }
        let mut body = json!({
            "contents": contents,
            "generationConfig": generation,
            "store": self.config.store,
        });
        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        if !declarations.is_empty() {
            body["tools"] = json!([{"functionDeclarations": declarations}]);
            body["toolConfig"] = json!({"functionCallingConfig": {"mode": "AUTO"}});
        }
        let back = declared
            .into_iter()
            .filter(|(own, wire)| own != wire)
            .map(|(own, wire)| (wire, own))
            .collect();
        (body, back)
    }

    fn redact(&self, text: &str) -> String {
        redact(text, &[self.api_key.as_str()])
    }

    fn http_error(&self, status: u16, body: &str) -> ProviderError {
        let body: String = self
            .redact(body.trim())
            .chars()
            .take(ERROR_BODY_LIMIT)
            .collect();
        match status {
            400 if body.contains("thought_signature") => ProviderError::HttpHint {
                status,
                body,
                hint: "agent86 sent back a tool call without the signature Gemini gave it; \
                       this is a bug, please report it at \
                       https://github.com/Maetek-Technologies/agent86/issues"
                    .to_owned(),
            },
            402 => ProviderError::HttpHint {
                status,
                body,
                hint: "the Google project's prepaid credits are used up; add credits in \
                       Google AI Studio's billing settings"
                    .to_owned(),
            },
            _ => ProviderError::Http { status, body },
        }
    }

    fn connect_error(&self, error: &reqwest::Error) -> ProviderError {
        if error.is_timeout() && error.is_connect() {
            ProviderError::ConnectTimeout {
                endpoint: self.url.clone(),
                seconds: self.config.connect_timeout.as_secs_f64(),
            }
        } else if error.is_timeout() {
            ProviderError::ReadTimeout {
                endpoint: self.url.clone(),
                seconds: self.config.read_timeout.as_secs_f64(),
            }
        } else if error.is_connect() {
            ProviderError::Connect {
                endpoint: self.url.clone(),
                detail: self.redact(&error_chain(error)),
            }
        } else {
            ProviderError::StreamInterrupted {
                provider: "google".to_owned(),
                endpoint: self.url.clone(),
                model: self.model.clone(),
                detail: self.redact(&error_chain(error)),
            }
        }
    }
}

fn build_client(config: &GoogleConfig) -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout)
        .build()
        .map_err(|error| ProviderError::Client {
            detail: error_chain(&error),
        })
}

impl Provider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn fresh(&self) -> Result<std::sync::Arc<dyn Provider>, ProviderError> {
        Ok(std::sync::Arc::new(Self {
            model: self.model.clone(),
            url: self.url.clone(),
            api_key: self.api_key.clone(),
            config: self.config.clone(),
            client: build_client(&self.config)?,
            pricing: self.pricing.clone(),
            price_ref: self.price_ref.clone(),
            calls: Mutex::new(self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()),
            next_id: AtomicU64::new(self.next_id.load(Ordering::Relaxed)),
        }))
    }

    fn stream(&self, request: CompletionRequest) -> DeltaStream<'_> {
        let (body, names) = self.prepare(&request);
        let state = StreamState {
            provider: self,
            body: body.to_string(),
            names,
            policy: RetryPolicy::new(self.config.max_retries),
            response: None,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            turn: TurnState::default(),
            finished: false,
        };
        Box::pin(futures_util::stream::unfold(
            state,
            |mut state| async move {
                let item = state.next_delta().await?;
                Some((item, state))
            },
        ))
    }
}

/// The system text and the `contents` for `messages`.
///
/// System messages become `systemInstruction`, in order. User turns are `user`
/// contents and assistant turns `model` contents, an assistant's tool calls as
/// `functionCall` parts. The results that follow a step are `functionResponse`
/// parts in one `user` content, in call order, after all of the step's calls, as
/// Google requires. Consecutive contents of one role are merged by joining their
/// part lists, never by joining parts, so a signed part stays apart. A model
/// content's first function call carries [`SKIP_SIGNATURE`], as there is no
/// signature kept for any call here; see [`GoogleProvider`] for the calls it made.
pub fn to_contents(messages: &[Message]) -> (String, Vec<Value>) {
    to_contents_with(messages, &HashMap::new(), &mut |name: &str| name.to_owned())
}

/// [`to_contents`] with the calls Gemini made (their signatures go back on their
/// own parts, with Google's ids) and the declared name of each tool. A step none
/// of whose calls has a signature kept gets the placeholder on its first call.
fn to_contents_with(
    messages: &[Message],
    calls: &HashMap<String, CallMeta>,
    rename: &mut dyn FnMut(&str) -> String,
) -> (String, Vec<Value>) {
    let mut system: Vec<&str> = Vec::new();
    let mut contents: Vec<(String, Vec<Value>)> = Vec::new();
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut push = |role: &str, parts: Vec<Value>| {
        if parts.is_empty() {
            return;
        }
        match contents.last_mut() {
            Some((last, existing)) if last == role => existing.extend(parts),
            _ => contents.push((role.to_owned(), parts)),
        }
    };
    for message in messages {
        match message.role {
            Role::System => {
                if !message.content.trim().is_empty() {
                    system.push(&message.content);
                }
            }
            Role::User => {
                let parts = text_part(&message.content).into_iter().collect();
                push("user", parts);
            }
            Role::Assistant => {
                let mut parts: Vec<Value> = text_part(&message.content).into_iter().collect();
                let signed = message.tool_calls.iter().any(|call| {
                    calls
                        .get(&call.id)
                        .is_some_and(|meta| meta.signature.is_some())
                });
                for (index, call) in message.tool_calls.iter().enumerate() {
                    names.insert(call.id.as_str(), call.name.as_str());
                    let args = if call.invalid_arguments().is_some() {
                        Map::new()
                    } else {
                        call.arguments.clone()
                    };
                    let meta = calls.get(&call.id);
                    let mut function = json!({"name": rename(&call.name), "args": args});
                    if meta.is_some_and(|meta| meta.google_id) {
                        function["id"] = json!(call.id);
                    }
                    let mut part = json!({"functionCall": function});
                    match meta.and_then(|meta| meta.signature.as_deref()) {
                        Some(signature) => part["thoughtSignature"] = json!(signature),
                        None if index == 0 && !signed => {
                            part["thoughtSignature"] = json!(SKIP_SIGNATURE);
                        }
                        None => {}
                    }
                    parts.push(part);
                }
                push("model", parts);
            }
            Role::Tool => {
                let name = message
                    .name
                    .as_deref()
                    .or_else(|| {
                        message
                            .tool_call_id
                            .as_deref()
                            .and_then(|id| names.get(id).copied())
                    })
                    .unwrap_or("tool");
                let mut response = json!({
                    "name": rename(name),
                    "response": {"result": message.content},
                });
                if let Some(id) = message
                    .tool_call_id
                    .as_deref()
                    .filter(|id| calls.get(*id).is_some_and(|meta| meta.google_id))
                {
                    response["id"] = json!(id);
                }
                push("user", vec![json!({ "functionResponse": response })]);
            }
        }
    }
    let contents = contents
        .into_iter()
        .map(|(role, parts)| json!({"role": role, "parts": parts}))
        .collect();
    (system.join("\n\n"), contents)
}

fn text_part(text: &str) -> Option<Value> {
    (!text.is_empty()).then(|| json!({"text": text}))
}

/// The name a tool is declared under: its own when Gemini takes it (a letter or
/// `_` first, then letters, digits, `_`, `.`, `:`, `-`, at most 128), else with a
/// `_` in front, more while that is taken by another tool in `taken`.
pub fn gemini_name(name: &str, taken: &[String]) -> String {
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
    if starts_well {
        return name.to_owned();
    }
    let mut wire = format!("_{name}");
    while taken.contains(&wire) {
        wire.insert(0, '_');
    }
    wire
}

/// What a `finishReason` (or a blocked prompt's `blockReason`) means for the turn.
pub fn stop_reason(reason: Option<&str>, has_tool_calls: bool) -> Option<String> {
    let wire = |reason: StopReason| {
        serde_json::to_value(reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
    };
    match reason {
        None if has_tool_calls => wire(StopReason::ToolUse),
        None => None,
        Some("STOP") if has_tool_calls => wire(StopReason::ToolUse),
        Some("STOP") => wire(StopReason::EndTurn),
        // A Gemini 4 reply cut at its per-request cap ends with either.
        Some("MAX_TOKENS" | "CONTINUATION") => wire(StopReason::MaxTokens),
        Some(other) => Some(format!("{STOPPED_PREFIX}{other}")),
    }
}

/// Accumulators for one attempt; a retry starts over.
#[derive(Default)]
struct TurnState {
    text: String,
    thinking: String,
    tool_calls: Vec<ToolCall>,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    finish_reason: Option<String>,
    emitted: bool,
}

struct StreamState<'a> {
    provider: &'a GoogleProvider,
    body: String,
    /// Declared name to the tool's own, for renamed tools.
    names: HashMap<String, String>,
    policy: RetryPolicy,
    response: Option<reqwest::Response>,
    buffer: Vec<u8>,
    pending: VecDeque<CompletionDelta>,
    turn: TurnState,
    finished: bool,
}

impl StreamState<'_> {
    async fn next_delta(&mut self) -> Option<Result<CompletionDelta, ProviderError>> {
        if let Some(delta) = self.pending.pop_front() {
            return Some(Ok(delta));
        }
        if self.finished {
            return None;
        }
        let result = self.advance().await;
        if result.is_err() {
            self.finished = true;
            self.response = None;
        }
        result.map(|()| self.pending.pop_front()).transpose()
    }

    async fn advance(&mut self) -> Result<(), ProviderError> {
        while self.pending.is_empty() && !self.finished {
            if self.response.is_none() {
                self.response = Some(self.connect().await?);
            }
            if let Some(line) = self.take_line() {
                self.handle_line(&line)?;
                continue;
            }
            let Some(response) = self.response.as_mut() else {
                continue;
            };
            match response.chunk().await {
                Ok(Some(bytes)) => self.buffer.extend_from_slice(&bytes),
                Ok(None) => {
                    if !self.buffer.is_empty() {
                        let line = std::mem::take(&mut self.buffer);
                        self.handle_line(&line)?;
                    }
                    self.finish();
                }
                Err(error) => {
                    let failure = self.provider.connect_error(&error);
                    let retryable = !matches!(failure, ProviderError::ReadTimeout { .. });
                    match self.policy.next_delay(self.turn.emitted, None) {
                        Some(delay) if retryable => {
                            tokio::time::sleep(delay).await;
                            self.response = None;
                            self.turn = TurnState::default();
                        }
                        _ => return Err(failure),
                    }
                }
            }
        }
        Ok(())
    }

    async fn connect(&mut self) -> Result<reqwest::Response, ProviderError> {
        let provider = self.provider;
        loop {
            let request = provider
                .client
                .post(&provider.url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header("x-goog-api-key", &provider.api_key)
                .body(self.body.clone());
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    let failure = provider.connect_error(&error);
                    let retryable = matches!(
                        failure,
                        ProviderError::Connect { .. }
                            | ProviderError::ConnectTimeout { .. }
                            | ProviderError::StreamInterrupted { .. }
                    );
                    match self.policy.next_delay(false, None) {
                        Some(delay) if retryable => {
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        _ => return Err(failure),
                    }
                }
            };
            let status = response.status().as_u16();
            if status == 200 {
                return Ok(response);
            }
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after);
            let body = response.text().await.unwrap_or_default();
            if is_retryable_status(status) {
                if let Some(delay) = self.policy.next_delay(false, retry_after) {
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            return Err(provider.http_error(status, &body));
        }
    }

    fn take_line(&mut self) -> Option<Vec<u8>> {
        let end = self.buffer.iter().position(|&byte| byte == b'\n')?;
        let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
        line.pop();
        Some(line)
    }

    fn handle_line(&mut self, line: &[u8]) -> Result<(), ProviderError> {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches('\r');
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let chunk: Value =
            serde_json::from_str(data).map_err(|error| ProviderError::MalformedChunk {
                provider: "google".to_owned(),
                endpoint: self.provider.url.clone(),
                model: self.provider.model.clone(),
                detail: error.to_string(),
            })?;
        if let Some(error) = chunk.get("error").filter(|error| error.is_object()) {
            let status = error
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|code| u16::try_from(code).ok())
                .unwrap_or(500);
            return Err(self.provider.http_error(status, data));
        }
        if let Some(usage) = chunk.get("usageMetadata") {
            let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            // Thinking is billed as output; tool-use prompts as input.
            self.turn.input_tokens = count("promptTokenCount") + count("toolUsePromptTokenCount");
            self.turn.output_tokens = count("candidatesTokenCount") + count("thoughtsTokenCount");
            self.turn.cached_tokens = count("cachedContentTokenCount");
        }
        if let Some(reason) = chunk
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            self.turn.finish_reason = Some(reason.to_owned());
        }
        let candidate = chunk.pointer("/candidates/0");
        let parts = candidate
            .and_then(|candidate| candidate.pointer("/content/parts"))
            .and_then(Value::as_array);
        for part in parts.into_iter().flatten() {
            self.part(part);
        }
        if let Some(reason) = candidate
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(Value::as_str)
        {
            if reason == "MALFORMED_FUNCTION_CALL" {
                // The model is told its call was malformed, as other adapters do
                // with bad JSON, when the response carries the call's text.
                if let Some(message) = candidate
                    .and_then(|candidate| candidate.get("finishMessage"))
                    .and_then(Value::as_str)
                {
                    let mut arguments = Map::new();
                    arguments.insert(INVALID_TOOL_ARGS_KEY.to_owned(), json!(message));
                    let n = self.provider.next_id.fetch_add(1, Ordering::Relaxed);
                    let id = format!("call_g{n}");
                    self.turn.tool_calls.push(ToolCall {
                        id,
                        name: "unknown".to_owned(),
                        arguments,
                    });
                }
            }
            self.turn.finish_reason = Some(reason.to_owned());
        }
        Ok(())
    }

    fn part(&mut self, part: &Value) {
        if let Some(call) = part.get("functionCall") {
            let wire = call.get("name").and_then(Value::as_str).unwrap_or_default();
            let name = self.names.get(wire).map_or(wire, String::as_str).to_owned();
            let arguments = call
                .get("args")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let google = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            let id = google.map_or_else(
                || {
                    let n = self.provider.next_id.fetch_add(1, Ordering::Relaxed);
                    format!("call_g{n}")
                },
                str::to_owned,
            );
            let meta = CallMeta {
                signature: part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .filter(|signature| !signature.is_empty())
                    .map(str::to_owned),
                google_id: google.is_some(),
            };
            self.provider
                .calls
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.clone(), meta);
            self.turn.tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
            return;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let thought = part.get("thought").and_then(Value::as_bool) == Some(true);
        self.turn.emitted = true;
        if thought {
            self.turn.thinking.push_str(text);
            self.pending.push_back(CompletionDelta {
                thinking: text.to_owned(),
                ..CompletionDelta::default()
            });
        } else {
            self.turn.text.push_str(text);
            self.pending.push_back(CompletionDelta {
                text: text.to_owned(),
                ..CompletionDelta::default()
            });
        }
    }

    fn finish(&mut self) {
        self.finished = true;
        self.response = None;
        let turn = std::mem::take(&mut self.turn);
        let has_calls = !turn.tool_calls.is_empty();
        let usage = Usage {
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            cache_read_tokens: turn.cached_tokens,
            ..Usage::default()
        };
        self.pending.push_back(CompletionDelta {
            done: true,
            completion: Some(Completion {
                text: turn.text,
                tool_calls: turn.tool_calls,
                usage: self
                    .provider
                    .pricing
                    .priced_usage(&self.provider.price_ref, usage),
                stop_reason: stop_reason(turn.finish_reason.as_deref(), has_calls),
                model: Some(self.provider.model.clone()),
                thinking: turn.thinking,
            }),
            ..CompletionDelta::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    fn message(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn names_gemini_refuses_get_an_underscore_and_stay_unique() {
        let taken = vec!["_1tool".to_owned()];
        assert_eq!(gemini_name("read_file", &taken), "read_file");
        assert_eq!(gemini_name("_private", &taken), "_private");
        assert_eq!(gemini_name("1password-get", &taken), "_1password-get");
        assert_eq!(gemini_name("-dash", &taken), "_-dash");
        assert_eq!(gemini_name("1tool", &taken), "__1tool", "past a taken name");
    }

    #[test]
    fn finish_reasons_map_and_the_rest_stop_the_turn() {
        assert_eq!(
            stop_reason(Some("STOP"), false).as_deref(),
            Some("end_turn")
        );
        assert_eq!(stop_reason(Some("STOP"), true).as_deref(), Some("tool_use"));
        assert_eq!(stop_reason(None, true).as_deref(), Some("tool_use"));
        assert_eq!(stop_reason(None, false), None);
        for cut in ["MAX_TOKENS", "CONTINUATION"] {
            assert_eq!(stop_reason(Some(cut), false).as_deref(), Some("max_tokens"));
        }
        for stopped in [
            "SAFETY",
            "RECITATION",
            "PROHIBITED_CONTENT",
            "OTHER",
            "SOMETHING_NEW",
        ] {
            assert_eq!(
                stop_reason(Some(stopped), false),
                Some(format!("stopped:{stopped}"))
            );
        }
    }

    #[test]
    fn contents_merge_by_role_and_keep_calls_before_their_results() {
        let mut assistant = message(Role::Assistant, "Looking.");
        assistant.tool_calls = vec![
            ToolCall {
                id: "a".into(),
                name: "read_file".into(),
                arguments: json!({"path": "x"}).as_object().unwrap().clone(),
            },
            ToolCall {
                id: "b".into(),
                name: "list_dir".into(),
                arguments: Map::new(),
            },
        ];
        let mut first = message(Role::Tool, "contents of x");
        first.tool_call_id = Some("a".into());
        first.name = Some("read_file".into());
        let mut second = message(Role::Tool, "a b c");
        second.tool_call_id = Some("b".into());
        let (system, contents) = to_contents(&[
            message(Role::System, "You are a test."),
            message(Role::User, "Hi"),
            message(Role::User, "again"),
            assistant,
            first,
            second,
        ]);
        assert_eq!(system, "You are a test.");
        assert_eq!(
            Value::Array(contents),
            json!([
                {"role": "user", "parts": [{"text": "Hi"}, {"text": "again"}]},
                {"role": "model", "parts": [
                    {"text": "Looking."},
                    {"functionCall": {"name": "read_file", "args": {"path": "x"}},
                     "thoughtSignature": SKIP_SIGNATURE},
                    {"functionCall": {"name": "list_dir", "args": {}}},
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "read_file", "response": {"result": "contents of x"}}},
                    {"functionResponse": {"name": "list_dir", "response": {"result": "a b c"}}},
                ]},
            ])
        );
    }
}
