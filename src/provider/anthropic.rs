//! Anthropic (Claude) provider, matching the Python `anthropic_provider.py`: the
//! Messages API with native tool use and streaming.
//!
//! Python drives the official `anthropic` SDK, so what a user sees comes from the
//! SDK as much as from the adapter: its retry rules (408, 409, 429, and 5xx, or
//! whatever `x-should-retry` says; `retry-after-ms` or `Retry-After` when it is at
//! most a minute, otherwise 0.5 seconds doubling to 8), its fixed timeouts (5
//! seconds to connect, 600 between chunks; Python deliberately leaves the
//! `[providers.anthropic]` timeouts unwired), and its error text (`Error code: 400 -
//! {...}`, the body printed as Python prints a dict). This adapter speaks the HTTP
//! API directly and reproduces those.

use super::capabilities::{
    is_sampling_rejection, mark_sampling_unsupported, supports_sampling_params,
};
use super::openai::error_chain;
use super::retry::jitter_fraction;
use super::{DeltaStream, Provider, ProviderError};
use crate::observability::python_repr_value;
use crate::pricing::PriceTable;
use crate::pyjson::python_json_dumps;
use crate::secrets::redact;
use crate::types::{
    python_repr, Completion, CompletionDelta, CompletionRequest, Message, Role, StopReason,
    ToolCall, ToolSpec, Usage, INVALID_TOOL_ARGS_KEY,
};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const API_VERSION: &str = "2023-06-01";

/// Used when neither the request nor the provider config names an output cap. The
/// Messages API requires `max_tokens`, so unlike the OpenAI-compatible adapters this
/// one always sends a number; 8192 matches `limits.max_output_tokens`'s fallback.
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// The SDK's own timeouts, which Python keeps: its read timeout is a total-request
/// budget in the SDK's terms, a different meaning from `read_timeout_s`.
pub const SDK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const SDK_READ_TIMEOUT: Duration = Duration::from_secs(600);

const INITIAL_RETRY_DELAY_S: f64 = 0.5;
const MAX_RETRY_DELAY_S: f64 = 8.0;

/// The SDK's `APITimeoutError` text.
const TIMEOUT_MESSAGE: &str = "Request timed out or interrupted. This could be due to a network \
     timeout, dropped connection, or request cancellation. See \
     https://docs.anthropic.com/en/api/errors#long-requests for more details.";

/// Minimum cacheable prefix, in tokens, per model family. A `cache_control`
/// marker on a shorter prefix is silently ignored, so marking below the floor buys
/// nothing and spends one of the four breakpoints. The floor is not monotonic
/// across generations, so it is matched by the longest prefix.
const CACHE_MINIMUM_TOKENS: &[(&str, u64)] = &[
    ("claude-opus-5", 512),
    ("claude-fable-5-1", 512),
    ("claude-mythos-5-1", 512),
    ("claude-fable-5", 512),
    ("claude-mythos-5", 512),
    ("claude-opus-4-8", 1024),
    ("claude-sonnet-5", 1024),
    ("claude-sonnet-4-6", 1024),
    ("claude-sonnet-4-5", 1024),
    ("claude-sonnet-4", 1024),
    ("claude-opus-4-1", 1024),
    ("claude-opus-4", 1024),
    ("claude-opus-4-7", 2048),
    ("claude-haiku-3-5", 2048),
    ("claude-opus-4-6", 4096),
    ("claude-opus-4-5", 4096),
    ("claude-haiku-4-5", 4096),
];

/// Conservative floor for a model the table does not know: over-estimating only
/// skips a marker that might have cached.
const CACHE_MINIMUM_FALLBACK: u64 = 4096;

/// Settings from `[providers.anthropic]` that this adapter uses.
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicConfig {
    pub base_url: Option<String>,
    /// The variable named in the missing-key message.
    pub api_key_env: String,
    /// Standing output cap, used when the request sets none.
    pub max_tokens: Option<u32>,
    /// Mark the stable tools and system prefix as cacheable.
    pub prompt_cache: bool,
    pub connect_timeout: Duration,
    /// The maximum gap between received chunks.
    pub read_timeout: Duration,
    pub max_retries: u32,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_env: DEFAULT_API_KEY_ENV.to_owned(),
            max_tokens: None,
            prompt_cache: true,
            connect_timeout: SDK_CONNECT_TIMEOUT,
            read_timeout: SDK_READ_TIMEOUT,
            max_retries: super::retry::DEFAULT_MAX_RETRIES,
        }
    }
}

pub struct AnthropicProvider {
    model: String,
    url: String,
    api_key: String,
    config: AnthropicConfig,
    client: reqwest::Client,
    pricing: PriceTable,
    /// The name usage is priced under, normally the `provider:model` ref.
    price_ref: String,
}

impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("model", &self.model)
            .field("url", &self.url)
            .field("api_key", &"***redacted***")
            .field("config", &self.config)
            .field("price_ref", &self.price_ref)
            .finish()
    }
}

/// Map an Anthropic `stop_reason` onto the shared [`StopReason`] vocabulary: the
/// four that already match pass through, and anything else (`pause_turn`,
/// `refusal`, a value added later) is `other`.
pub fn normalize_stop_reason(raw: Option<&str>) -> Option<StopReason> {
    Some(match raw? {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::Other,
    })
}

/// The minimum cacheable prefix length for `model`, by the longest matching prefix.
pub fn cache_minimum_tokens(model: &str) -> u64 {
    let ident = model
        .split_once(':')
        .map_or(model, |(_, rest)| rest)
        .trim()
        .to_lowercase();
    CACHE_MINIMUM_TOKENS
        .iter()
        .filter(|(family, _)| ident.starts_with(family))
        .max_by_key(|(family, _)| family.len())
        .map_or(CACHE_MINIMUM_FALLBACK, |&(_, minimum)| minimum)
}

/// A rough token count (about four characters a token) for the cacheability
/// decision; a wrong guess only costs a marker the API ignores.
fn approx_tokens(text: &str) -> u64 {
    text.chars().count() as u64 / 4
}

/// What Python's f-string makes of a parsed JSON body: a string as it is, anything
/// else as Python prints the equivalent object.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_repr_value(other),
    }
}

/// The SDK's `APIStatusError` text for a failed response.
fn status_error_text(status: u16, body: &str) -> String {
    let text = body.trim();
    match serde_json::from_str::<Value>(text) {
        Ok(parsed) => format!("Error code: {status} - {}", python_str(&parsed)),
        Err(_) if text.is_empty() => format!("Error code: {status}"),
        Err(_) => text.to_owned(),
    }
}

/// Whether the SDK retries a response with this status and these headers.
fn should_retry(status: u16, headers: &reqwest::header::HeaderMap) -> bool {
    match header(headers, "x-should-retry").as_deref() {
        Some("true") => true,
        Some("false") => false,
        _ => matches!(status, 408 | 409 | 429) || status >= 500,
    }
}

fn header(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The server's requested delay in seconds: `retry-after-ms`, then `Retry-After`
/// as a number. HTTP dates are not parsed and fall back to backoff.
fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<f64> {
    let parse = |name| header(headers, name).and_then(|value| value.trim().parse::<f64>().ok());
    parse("retry-after-ms")
        .map(|ms| ms / 1000.0)
        .or_else(|| parse("retry-after"))
}

/// The SDK's delay before retry number `retries_done + 1`: the server's delay when
/// it is a reasonable one, otherwise 0.5 seconds doubling to 8, less up to a quarter.
fn retry_delay(retries_done: u32, retry_after: Option<f64>) -> Duration {
    if let Some(seconds) = retry_after.filter(|&s| s > 0.0 && s <= 60.0) {
        return Duration::from_secs_f64(seconds);
    }
    let sleep =
        (INITIAL_RETRY_DELAY_S * 2f64.powi(retries_done.min(1000) as i32)).min(MAX_RETRY_DELAY_S);
    Duration::from_secs_f64((sleep * (1.0 - 0.25 * jitter_fraction())).max(0.0))
}

impl AnthropicProvider {
    pub fn new(
        model: impl Into<String>,
        config: AnthropicConfig,
        api_key: Option<String>,
    ) -> Result<Self, ProviderError> {
        let Some(api_key) = api_key.filter(|key| !key.is_empty()) else {
            return Err(ProviderError::Message(format!(
                "No Anthropic API key found. Set the {} environment variable or store a key \
                 in the OS keyring via /config model.",
                config.api_key_env
            )));
        };
        let base = config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}/v1/messages");
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()
            .map_err(|error| ProviderError::Client {
                detail: error_chain(&error),
            })?;
        let model = model.into();
        Ok(Self {
            price_ref: model.clone(),
            model,
            url,
            api_key,
            config,
            client,
            pricing: PriceTable::builtin(),
        })
    }

    /// Price usage with `pricing`, looked up under `price_ref` (see the OpenAI adapter).
    pub fn with_pricing(mut self, pricing: PriceTable, price_ref: impl Into<String>) -> Self {
        self.pricing = pricing;
        self.price_ref = price_ref.into();
        self
    }

    /// The output cap for this call: the request's, then the config's, then the floor.
    fn max_tokens(&self, request: &CompletionRequest) -> u32 {
        request
            .max_tokens
            .filter(|&cap| cap > 0)
            .or(self.config.max_tokens.filter(|&cap| cap > 0))
            .unwrap_or(DEFAULT_MAX_TOKENS)
    }

    /// The streaming Messages API payload for `request`.
    ///
    /// `temperature` is left out for models that removed it (sending it is a 400
    /// with no replacement value); the gate is on the model, never on the value.
    pub fn payload(&self, request: &CompletionRequest) -> Value {
        let (system, messages) = to_messages(&request.messages);
        let mut payload = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": self.max_tokens(request),
        });
        let system = system.filter(|text| !text.is_empty());
        if let Some(system) = system {
            payload["system"] = json!(system);
        }
        let mut tools = to_tools(&request.tools);
        if self.config.prompt_cache {
            self.apply_prompt_cache(&mut payload, system, &mut tools);
        }
        if !tools.is_empty() {
            payload["tools"] = Value::Array(tools);
        }
        if supports_sampling_params(&self.model) {
            payload["temperature"] = json!(request.temperature);
        }
        payload["stream"] = json!(true);
        payload
    }

    /// Mark the stable prefix (the tool definitions, then the system prompt) as
    /// cacheable. The API renders tools, then system, then messages, and caching
    /// matches a prefix, so a marker on the last tool caches the tool list and one
    /// on the system block caches both. Each marker is placed only when its prefix
    /// is long enough to cache, so at most two of the four breakpoints are used.
    fn apply_prompt_cache(&self, payload: &mut Value, system: Option<&str>, tools: &mut [Value]) {
        let minimum = cache_minimum_tokens(&self.model);
        // The tool list is rendered as JSON, so its serialized length is what counts.
        let tools_tokens = if tools.is_empty() {
            0
        } else {
            approx_tokens(&python_json_dumps(&Value::Array(tools.to_vec()), true))
        };
        if tools_tokens >= minimum {
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        let Some(system) = system else {
            return;
        };
        if tools_tokens + approx_tokens(system) < minimum {
            return;
        }
        payload["system"] = json!([
            {"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}
        ]);
    }

    fn redact(&self, text: &str) -> String {
        redact(text, &[&self.api_key])
    }

    /// An SDK `APIError`, as Python reports it.
    fn api_error(&self, text: &str) -> ProviderError {
        ProviderError::Message(self.redact(&format!("Anthropic API error: {text}")))
    }

    /// A failure the SDK does not convert (a dropped stream, a malformed event),
    /// as Python's loop words it when it halts the turn.
    fn stream_failed(&self, detail: &str) -> ProviderError {
        ProviderError::Message(self.redact(&format!(
            "The 'anthropic' provider failed while streaming model {} — {detail}. The turn was \
             halted and the session saved; retry the request, or check the endpoint and network.",
            python_repr(&self.model)
        )))
    }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn stream(&self, request: CompletionRequest) -> DeltaStream<'_> {
        let state = StreamState {
            provider: self,
            payload: self.payload(&request),
            sampling_dropped: false,
            retries: 0,
            response: None,
            buffer: Vec::new(),
            event: None,
            data: Vec::new(),
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

fn to_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect()
}

/// Split out the system prompt and convert the rest to the Messages API's shape:
/// a tool result is a user message holding a `tool_result` block, and an
/// assistant's tool calls are `tool_use` blocks after its text.
fn to_messages(messages: &[Message]) -> (Option<&str>, Vec<Value>) {
    let mut system = None;
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::System => system = Some(message.content.as_str()),
            Role::Tool => out.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id,
                    "content": message.content,
                }],
            })),
            Role::Assistant if !message.tool_calls.is_empty() => {
                let mut content = Vec::new();
                if !message.content.is_empty() {
                    content.push(json!({"type": "text", "text": message.content}));
                }
                for call in &message.tool_calls {
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                out.push(json!({"role": "assistant", "content": content}));
            }
            role => out.push(json!({"role": role, "content": message.content})),
        }
    }
    (system, out)
}

/// One content block of the message being streamed.
enum Block {
    Text(String),
    /// Extended thinking. The adapter never asks for it, but a block that arrives is
    /// read; `redacted_thinking` carries nothing readable and stays `Other`.
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
        json: String,
    },
    Other,
}

/// Accumulators for one attempt. A retry must not inherit half a response.
#[derive(Default)]
struct TurnState {
    started: bool,
    blocks: Vec<Block>,
    stop_reason: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    emitted: bool,
}

fn count(object: &Value, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64)
}

impl TurnState {
    fn read_usage(&mut self, usage: &Value) {
        if let Some(tokens) = count(usage, "input_tokens") {
            self.input_tokens = tokens;
        }
        if let Some(tokens) = count(usage, "output_tokens") {
            self.output_tokens = tokens;
        }
        if let Some(tokens) = count(usage, "cache_read_input_tokens") {
            self.cache_read_tokens = tokens;
        }
        if let Some(tokens) = count(usage, "cache_creation_input_tokens") {
            self.cache_creation_tokens = tokens;
        }
    }
}

/// The tool's input: the streamed JSON once it is complete, or the block's own.
/// Arguments that are not valid JSON are carried raw, as the other adapters do.
fn tool_input(input: &Value, json: &str) -> Map<String, Value> {
    if json.is_empty() {
        return input.as_object().cloned().unwrap_or_default();
    }
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Object(arguments)) => arguments,
        Ok(Value::Null) => Map::new(),
        _ => Map::from_iter([(
            INVALID_TOOL_ARGS_KEY.to_owned(),
            Value::String(json.to_owned()),
        )]),
    }
}

/// The events the SDK parses; anything else but `ping` and `error` is skipped.
const MESSAGE_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

struct StreamState<'a> {
    provider: &'a AnthropicProvider,
    payload: Value,
    /// Python retries once without `temperature` when the API refuses it.
    sampling_dropped: bool,
    /// SDK retries spent on this attempt.
    retries: u32,
    response: Option<reqwest::Response>,
    buffer: Vec<u8>,
    /// The event being read: its `event:` name and `data:` lines.
    event: Option<String>,
    data: Vec<String>,
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

    /// Read until at least one delta is pending or the response has ended.
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
                    self.dispatch()?;
                    if self.response.is_some() {
                        self.finish()?;
                    }
                }
                Err(error) => {
                    // The SDK never retries once the stream is open, and these
                    // errors reach Python's loop unconverted.
                    let detail = if error.is_timeout() {
                        "ReadTimeout: The read operation timed out".to_owned()
                    } else {
                        format!("ReadError: {}", error_chain(&error))
                    };
                    return Err(self.provider.stream_failed(&detail));
                }
            }
        }
        Ok(())
    }

    /// Send the request with the SDK's retries, and return a 200 response.
    async fn connect(&mut self) -> Result<reqwest::Response, ProviderError> {
        let provider = self.provider;
        loop {
            self.turn = TurnState::default();
            self.buffer.clear();
            self.event = None;
            self.data.clear();

            let request = provider
                .client
                .post(&provider.url)
                .header("x-api-key", &provider.api_key)
                .header("anthropic-version", API_VERSION)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .body(self.payload.to_string());

            let failure = match request.send().await {
                Ok(response) if response.status().as_u16() == 200 => return Ok(response),
                Ok(response) => {
                    let status = response.status().as_u16();
                    let headers = response.headers().clone();
                    let body = response.text().await.unwrap_or_default();
                    if should_retry(status, &headers) && self.retries < provider.config.max_retries
                    {
                        let delay = retry_delay(self.retries, retry_after_seconds(&headers));
                        self.retries += 1;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    status_error_text(status, &body)
                }
                Err(error) => {
                    if self.retries < provider.config.max_retries {
                        let delay = retry_delay(self.retries, None);
                        self.retries += 1;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    if error.is_timeout() {
                        TIMEOUT_MESSAGE.to_owned()
                    } else {
                        "Connection error.".to_owned()
                    }
                }
            };
            if self.retry_without_sampling(&failure) {
                continue;
            }
            return Err(provider.api_error(&failure));
        }
    }

    /// A model released after the capability table was written says itself that
    /// it refuses `temperature`; that is only safe to act on before any text.
    fn retry_without_sampling(&mut self, failure: &str) -> bool {
        if self.sampling_dropped || self.turn.emitted || !is_sampling_rejection(failure) {
            return false;
        }
        mark_sampling_unsupported(&self.provider.model);
        if let Some(object) = self.payload.as_object_mut() {
            object.remove("temperature");
        }
        self.sampling_dropped = true;
        self.retries = 0;
        self.response = None;
        true
    }

    fn take_line(&mut self) -> Option<Vec<u8>> {
        let end = self.buffer.iter().position(|&byte| byte == b'\n')?;
        let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
        line.pop();
        Some(line)
    }

    /// One line of the event stream; a blank line ends an event.
    fn handle_line(&mut self, line: &[u8]) -> Result<(), ProviderError> {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<(), ProviderError> {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data).join("\n");
        let Some(event) = event else {
            return Ok(());
        };
        if event == "error" {
            let text = match serde_json::from_str::<Value>(&data) {
                Ok(body) => python_str(&body),
                Err(_) if data.is_empty() => "Error code: 200".to_owned(),
                Err(_) => data,
            };
            if self.retry_without_sampling(&text) {
                return Ok(());
            }
            return Err(self.provider.api_error(&text));
        }
        if !MESSAGE_EVENTS.contains(&event.as_str()) {
            return Ok(());
        }
        let body: Value = serde_json::from_str(&data).map_err(|error| {
            self.provider
                .stream_failed(&format!("JSONDecodeError: {error}"))
        })?;
        let kind = body
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(&event)
            .to_owned();
        self.accumulate(&kind, &body)
    }

    /// Fold one event into the message, as the SDK's `accumulate_event` does, and
    /// queue any text it carries.
    fn accumulate(&mut self, kind: &str, body: &Value) -> Result<(), ProviderError> {
        let turn = &mut self.turn;
        if !turn.started {
            if kind != "message_start" {
                return Err(self.provider.stream_failed(&format!(
                    "RuntimeError: Unexpected event order, got {kind} before \"message_start\""
                )));
            }
            turn.started = true;
            if let Some(usage) = body.pointer("/message/usage") {
                turn.read_usage(usage);
            }
            return Ok(());
        }
        match kind {
            "content_block_start" => {
                let block = &body["content_block"];
                turn.blocks.push(match block["type"].as_str() {
                    Some("text") => Block::Text(block["text"].as_str().unwrap_or("").to_owned()),
                    Some("thinking") => {
                        Block::Thinking(block["thinking"].as_str().unwrap_or("").to_owned())
                    }
                    Some("tool_use") => Block::ToolUse {
                        id: block["id"].as_str().unwrap_or("").to_owned(),
                        name: block["name"].as_str().unwrap_or("").to_owned(),
                        input: block["input"].clone(),
                        json: String::new(),
                    },
                    _ => Block::Other,
                });
            }
            "content_block_delta" => {
                let index = body["index"].as_u64().unwrap_or(0) as usize;
                let Some(block) = turn.blocks.get_mut(index) else {
                    return Err(self
                        .provider
                        .stream_failed("IndexError: list index out of range"));
                };
                let delta = &body["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or("");
                        if let Block::Text(accumulated) = block {
                            accumulated.push_str(text);
                        }
                        if !text.is_empty() {
                            turn.emitted = true;
                            self.pending.push_back(CompletionDelta {
                                text: text.to_owned(),
                                ..CompletionDelta::default()
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        let thinking = delta["thinking"].as_str().unwrap_or("");
                        if let Block::Thinking(accumulated) = block {
                            accumulated.push_str(thinking);
                        }
                        if !thinking.is_empty() {
                            turn.emitted = true;
                            self.pending.push_back(CompletionDelta {
                                thinking: thinking.to_owned(),
                                ..CompletionDelta::default()
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Block::ToolUse { json, .. } = block {
                            json.push_str(delta["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                turn.stop_reason = body["delta"]["stop_reason"].as_str().map(str::to_owned);
                turn.read_usage(&body["usage"]);
            }
            _ => {}
        }
        Ok(())
    }

    /// The response ended: assemble the final completion.
    fn finish(&mut self) -> Result<(), ProviderError> {
        self.finished = true;
        self.response = None;
        let turn = std::mem::take(&mut self.turn);
        if !turn.started {
            // The SDK asserts that a message arrived.
            return Err(self.provider.stream_failed("AssertionError: "));
        }
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tool_calls = Vec::new();
        for block in turn.blocks {
            match block {
                Block::Text(piece) => text.push_str(&piece),
                Block::Thinking(piece) => thinking.push_str(&piece),
                Block::ToolUse {
                    id,
                    name,
                    input,
                    json,
                } => tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: tool_input(&input, &json),
                }),
                Block::Other => {}
            }
        }
        // Anthropic reports `input_tokens` as the uncached remainder; Usage counts the
        // whole prompt, with the cache fields a breakdown of it.
        let usage = Usage {
            input_tokens: turn.input_tokens + turn.cache_read_tokens + turn.cache_creation_tokens,
            output_tokens: turn.output_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_creation_tokens: turn.cache_creation_tokens,
            ..Usage::default()
        };
        let provider = self.provider;
        let stop_reason = normalize_stop_reason(turn.stop_reason.as_deref());
        self.pending.push_back(CompletionDelta {
            text: String::new(),
            done: true,
            completion: Some(Completion {
                text,
                tool_calls,
                usage: provider.pricing.priced_usage(&provider.price_ref, usage),
                stop_reason: stop_reason.map(|reason| match serde_json::to_value(reason) {
                    Ok(Value::String(text)) => text,
                    _ => String::new(),
                }),
                model: Some(provider.model.clone()),
                thinking,
            }),
            ..CompletionDelta::default()
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_floors_match_the_longest_prefix() {
        assert_eq!(cache_minimum_tokens("anthropic:claude-opus-4-8"), 1024);
        assert_eq!(cache_minimum_tokens("claude-opus-4-7"), 2048);
        assert_eq!(cache_minimum_tokens("claude-opus-4-5-20251101"), 4096);
        assert_eq!(cache_minimum_tokens("Claude-Opus-5"), 512);
        assert_eq!(cache_minimum_tokens("claude-3-haiku"), 4096);
    }

    #[test]
    fn stop_reasons_outside_the_vocabulary_are_other() {
        assert_eq!(normalize_stop_reason(None), None);
        assert_eq!(
            normalize_stop_reason(Some("stop_sequence")),
            Some(StopReason::StopSequence)
        );
        assert_eq!(
            normalize_stop_reason(Some("refusal")),
            Some(StopReason::Other)
        );
    }

    #[test]
    fn status_errors_read_as_the_sdk_prints_them() {
        assert_eq!(
            status_error_text(
                401,
                " {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\
                 \"message\":\"invalid x-api-key\"},\"request_id\":null}\n"
            ),
            "Error code: 401 - {'type': 'error', 'error': {'type': 'authentication_error', \
             'message': 'invalid x-api-key'}, 'request_id': None}"
        );
        assert_eq!(status_error_text(502, "Bad Gateway\n"), "Bad Gateway");
        assert_eq!(status_error_text(503, "  "), "Error code: 503");
        assert_eq!(status_error_text(500, "\"oops\""), "Error code: 500 - oops");
    }

    #[test]
    fn retry_delays_follow_the_sdk() {
        assert_eq!(retry_delay(0, Some(2.5)), Duration::from_secs_f64(2.5));
        for (retries, ceiling) in [(0, 0.5), (1, 1.0), (5, 8.0)] {
            // The server's delay is ignored when it is zero or over a minute.
            for after in [None, Some(0.0), Some(61.0)] {
                let delay = retry_delay(retries, after).as_secs_f64();
                assert!(
                    delay <= ceiling && delay >= ceiling * 0.75,
                    "{retries}: {delay}"
                );
            }
        }
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "3".parse().unwrap());
        assert_eq!(retry_after_seconds(&headers), Some(3.0));
        headers.insert("retry-after-ms", "1500".parse().unwrap());
        assert_eq!(retry_after_seconds(&headers), Some(1.5));
    }

    #[test]
    fn the_sdk_retries_conflicts_timeouts_rate_limits_and_server_errors() {
        let mut headers = reqwest::header::HeaderMap::new();
        for status in [408, 409, 429, 500, 529] {
            assert!(should_retry(status, &headers), "{status}");
        }
        assert!(!should_retry(400, &headers));
        headers.insert("x-should-retry", "true".parse().unwrap());
        assert!(should_retry(400, &headers));
        headers.insert("x-should-retry", "false".parse().unwrap());
        assert!(!should_retry(529, &headers));
    }
}
