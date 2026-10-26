//! OpenAI-compatible provider, matching the Python `openai_provider.py`.
//!
//! One adapter for the Chat Completions API and every service that speaks it:
//! OpenAI, OpenRouter, Groq, Together, vLLM, LM Studio, and other local servers.
//! Streaming tool calls arrive as fragments across SSE chunks and are
//! accumulated by index into whole [`ToolCall`] values.

use super::capabilities::supports_sampling_params;
use super::retry::{is_retryable_status, parse_retry_after, RetryPolicy, DEFAULT_MAX_RETRIES};
use super::{DeltaStream, Provider, ProviderError};
use crate::pricing::PriceTable;
use crate::secrets::redact;
use crate::types::{
    Completion, CompletionDelta, CompletionRequest, Message, Role, StopReason, ToolCall, ToolSpec,
    Usage, INVALID_TOOL_ARGS_KEY,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// Error bodies are truncated to this many characters, as in the Python implementation.
const ERROR_BODY_LIMIT: usize = 400;

/// Words a server uses when it refuses a parameter name outright.
const PARAM_REJECTION_WORDS: &[&str] = &[
    "unsupported",
    "unrecognized",
    "unrecognised",
    "not supported",
    "unknown",
    "unexpected",
    "invalid",
    "extra field",
    "additional propert",
    "not permitted",
    "no longer",
    "deprecat",
];

/// Settings from a `[providers.<name>]` block that this adapter uses.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiConfig {
    /// Endpoint root, with or without the `/v1` suffix. Defaults to OpenAI.
    pub base_url: Option<String>,
    /// The environment variable named in config, used in the missing-key message.
    pub api_key_env: Option<String>,
    /// The config section name, which is also the keyring account for its key.
    pub config_name: String,
    /// Whether construction fails without an API key. Keyless local endpoints set this to false.
    pub require_key: bool,
    /// Standing output cap, used when the request does not set one.
    pub max_tokens: Option<u32>,
    pub connect_timeout: Duration,
    /// The maximum gap between received chunks, not a total-request deadline.
    pub read_timeout: Duration,
    pub max_retries: u32,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_env: None,
            config_name: "openai".to_owned(),
            require_key: true,
            max_tokens: None,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(300),
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

pub struct OpenAiProvider {
    model: String,
    url: String,
    api_key: Option<String>,
    config: OpenAiConfig,
    client: reqwest::Client,
    pricing: PriceTable,
    /// The name usage is priced under, normally the `provider:model` ref.
    price_ref: String,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("model", &self.model)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "***redacted***"))
            .field("config", &self.config)
            .field("price_ref", &self.price_ref)
            .finish()
    }
}

impl OpenAiProvider {
    pub fn new(
        model: impl Into<String>,
        config: OpenAiConfig,
        api_key: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.filter(|key| !key.is_empty());
        if config.require_key && api_key.is_none() {
            return Err(ProviderError::MissingApiKey {
                env: config
                    .api_key_env
                    .clone()
                    .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned()),
                account: config.config_name.clone(),
            });
        }

        let base = config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        // Tolerate a base_url given with or without the /v1 suffix.
        let version = if base.ends_with("/v1") { "" } else { "/v1" };
        let url = format!("{base}{version}/chat/completions");

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

    /// Price usage with `pricing`, looked up under `price_ref`.
    ///
    /// Python prices with the bare model id, so a `[pricing.models]` override keyed by
    /// the `provider:model` ref marks the model as priced without applying its rate.
    /// Pricing under the ref tries the ref first and then the bare id, which gives
    /// the same result as Python in every other case.
    pub fn with_pricing(mut self, pricing: PriceTable, price_ref: impl Into<String>) -> Self {
        self.pricing = pricing;
        self.price_ref = price_ref.into();
        self
    }

    /// The resolved Chat Completions endpoint.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Build the streaming Chat Completions payload for `request`.
    pub fn payload(&self, request: &CompletionRequest) -> Value {
        let mut payload = json!({
            "model": self.model,
            "messages": to_messages(&request.messages),
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if supports_sampling_params(&self.model) {
            payload["temperature"] = json!(request.temperature);
        }
        let max_tokens = request
            .max_tokens
            .filter(|&cap| cap > 0)
            .or(self.config.max_tokens.filter(|&cap| cap > 0));
        if let Some(cap) = max_tokens {
            // New name first, unless this endpoint has already refused it this session.
            let key = if uses_legacy_max_tokens(&self.url) {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            payload[key] = json!(cap);
        }
        if !request.tools.is_empty() {
            payload["tools"] = to_tools(&request.tools);
            payload["tool_choice"] = json!("auto");
        }
        payload
    }

    fn redact(&self, text: &str) -> String {
        redact(text, &[self.api_key.as_deref().unwrap_or_default()])
    }

    fn connect_error(&self, error: &reqwest::Error) -> ProviderError {
        if error.is_timeout() && error.is_connect() {
            ProviderError::ConnectTimeout {
                endpoint: self.url.clone(),
                seconds: self.config.connect_timeout.as_secs_f64(),
            }
        } else if error.is_timeout() {
            self.read_timeout_error()
        } else if error.is_connect() {
            ProviderError::Connect {
                endpoint: self.url.clone(),
                detail: self.redact(&error_chain(error)),
            }
        } else {
            self.interrupted_error(error)
        }
    }

    fn read_timeout_error(&self) -> ProviderError {
        ProviderError::ReadTimeout {
            endpoint: self.url.clone(),
            seconds: self.config.read_timeout.as_secs_f64(),
        }
    }

    fn interrupted_error(&self, error: &reqwest::Error) -> ProviderError {
        ProviderError::StreamInterrupted {
            provider: self.name().to_owned(),
            endpoint: self.url.clone(),
            model: self.model.clone(),
            detail: self.redact(&error_chain(error)),
        }
    }
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn stream(&self, request: CompletionRequest) -> DeltaStream<'_> {
        let state = StreamState {
            provider: self,
            payload: self.payload(&request),
            policy: RetryPolicy::new(self.config.max_retries),
            swapped: false,
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

/// Map an OpenAI `finish_reason` onto the shared [`StopReason`] vocabulary.
///
/// `has_tool_calls` covers compatible servers that report a plain `stop` on a
/// turn that did emit tool calls.
pub fn normalize_stop_reason(raw: Option<&str>, has_tool_calls: bool) -> Option<StopReason> {
    let Some(raw) = raw else {
        return has_tool_calls.then_some(StopReason::ToolUse);
    };
    let mapped = match raw {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" | "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::Other,
    };
    if has_tool_calls && mapped == StopReason::EndTurn {
        Some(StopReason::ToolUse)
    } else {
        Some(mapped)
    }
}

/// True when an error body says `max_completion_tokens` is not a parameter it knows.
pub fn is_max_tokens_rejection(body: &str) -> bool {
    let low = body.to_lowercase();
    low.contains("max_completion_tokens") && PARAM_REJECTION_WORDS.iter().any(|w| low.contains(w))
}

/// Endpoints that rejected `max_completion_tokens` during this process.
fn legacy_max_tokens_endpoints() -> &'static Mutex<HashSet<String>> {
    static ENDPOINTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ENDPOINTS.get_or_init(Mutex::default)
}

/// True when `url` has already said it only understands `max_tokens`.
pub fn uses_legacy_max_tokens(url: &str) -> bool {
    legacy_max_tokens_endpoints()
        .lock()
        .is_ok_and(|endpoints| endpoints.contains(url))
}

fn mark_legacy_max_tokens(url: &str) {
    if let Ok(mut endpoints) = legacy_max_tokens_endpoints().lock() {
        endpoints.insert(url.to_owned());
    }
}

fn to_tools(tools: &[ToolSpec]) -> Value {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect()
}

fn to_messages(messages: &[Message]) -> Value {
    messages
        .iter()
        .map(|message| match message.role {
            Role::Tool => json!({
                "role": "tool",
                "tool_call_id": message.tool_call_id,
                "content": message.content,
            }),
            Role::Assistant if !message.tool_calls.is_empty() => json!({
                "role": "assistant",
                "content": (!message.content.is_empty()).then_some(&message.content),
                "tool_calls": message.tool_calls.iter().map(|call| json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": Value::Object(call.arguments.clone()).to_string(),
                    },
                })).collect::<Vec<_>>(),
            }),
            role => json!({"role": role, "content": message.content}),
        })
        .collect()
}

/// Accumulators for one attempt. A retry must not inherit half a response.
#[derive(Default)]
struct TurnState {
    text: String,
    tool_fragments: BTreeMap<u64, ToolFragment>,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    finish_reason: Option<String>,
    emitted: bool,
}

#[derive(Default)]
struct ToolFragment {
    id: String,
    name: String,
    arguments: String,
}

struct StreamState<'a> {
    provider: &'a OpenAiProvider,
    payload: Value,
    policy: RetryPolicy,
    swapped: bool,
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

    /// Read until at least one delta is pending or the turn is complete.
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
                    // A final line without a newline is still a line.
                    if !self.buffer.is_empty() {
                        let line = std::mem::take(&mut self.buffer);
                        self.handle_line(&line)?;
                    }
                    if !self.finished {
                        self.finish();
                    }
                }
                Err(error) => {
                    let failure = self.provider.connect_error(&error);
                    let retryable = !matches!(failure, ProviderError::ReadTimeout { .. });
                    match self.policy.next_delay(self.turn.emitted, None) {
                        Some(delay) if retryable => {
                            tokio::time::sleep(delay).await;
                            self.response = None;
                        }
                        _ => return Err(failure),
                    }
                }
            }
        }
        Ok(())
    }

    /// Send the request, retrying transient failures, and return a 200 response.
    async fn connect(&mut self) -> Result<reqwest::Response, ProviderError> {
        let provider = self.provider;
        loop {
            self.turn = TurnState::default();
            self.buffer.clear();

            let mut request = provider
                .client
                .post(&provider.url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(self.payload.to_string());
            if let Some(key) = &provider.api_key {
                request = request.bearer_auth(key);
            }

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

            if status == 400
                && !self.swapped
                && self.payload.get("max_completion_tokens").is_some()
                && is_max_tokens_rejection(&body)
            {
                // This endpoint speaks the older dialect. Retry once under the old
                // name without spending the transient-failure budget.
                self.swapped = true;
                mark_legacy_max_tokens(&provider.url);
                if let Some(object) = self.payload.as_object_mut() {
                    if let Some(cap) = object.remove("max_completion_tokens") {
                        object.insert("max_tokens".to_owned(), cap);
                    }
                }
                continue;
            }

            if is_retryable_status(status) {
                if let Some(delay) = self.policy.next_delay(false, retry_after) {
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }

            let body: String = body.trim().chars().take(ERROR_BODY_LIMIT).collect();
            return Err(ProviderError::Http {
                status,
                body: provider.redact(&body),
            });
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
        if data == "[DONE]" {
            self.finish();
            return Ok(());
        }

        let chunk: Value =
            serde_json::from_str(data).map_err(|error| ProviderError::MalformedChunk {
                provider: self.provider.name().to_owned(),
                endpoint: self.provider.url.clone(),
                model: self.provider.model.clone(),
                detail: error.to_string(),
            })?;

        if let Some(usage) = chunk.get("usage").and_then(Value::as_object) {
            let turn = &mut self.turn;
            turn.prompt_tokens = counter(usage, "prompt_tokens").unwrap_or(turn.prompt_tokens);
            turn.completion_tokens =
                counter(usage, "completion_tokens").unwrap_or(turn.completion_tokens);
            // The cached slice is a subset of prompt_tokens, the convention Usage follows.
            if let Some(details) = usage
                .get("prompt_tokens_details")
                .and_then(Value::as_object)
            {
                turn.cached_tokens =
                    counter(details, "cached_tokens").unwrap_or(turn.cached_tokens);
            }
        }

        let choices = chunk.get("choices").and_then(Value::as_array);
        for choice in choices.into_iter().flatten() {
            let delta = choice.get("delta");
            if let Some(piece) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
                if !piece.is_empty() {
                    self.turn.text.push_str(piece);
                    self.turn.emitted = true;
                    self.pending.push_back(CompletionDelta {
                        text: piece.to_owned(),
                        ..CompletionDelta::default()
                    });
                }
            }
            let fragments = delta
                .and_then(|d| d.get("tool_calls"))
                .and_then(Value::as_array);
            for fragment in fragments.into_iter().flatten() {
                self.accumulate(fragment);
            }
            match choice.get("finish_reason") {
                Some(Value::String(reason)) if !reason.is_empty() => {
                    self.turn.finish_reason = Some(reason.clone());
                }
                Some(other @ (Value::Number(_) | Value::Bool(true))) => {
                    self.turn.finish_reason = Some(other.to_string());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn accumulate(&mut self, fragment: &Value) {
        let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0);
        let slot = self.turn.tool_fragments.entry(index).or_default();
        if let Some(id) = non_empty_str(fragment.get("id")) {
            slot.id = id.to_owned();
        }
        let function = fragment.get("function");
        if let Some(name) = non_empty_str(function.and_then(|f| f.get("name"))) {
            slot.name = name.to_owned();
        }
        if let Some(arguments) = non_empty_str(function.and_then(|f| f.get("arguments"))) {
            slot.arguments.push_str(arguments);
        }
    }

    /// Queue the final delta carrying the assembled completion.
    fn finish(&mut self) {
        self.finished = true;
        self.response = None;
        let turn = std::mem::take(&mut self.turn);
        let tool_calls = assemble_tool_calls(turn.tool_fragments);
        let stop_reason =
            normalize_stop_reason(turn.finish_reason.as_deref(), !tool_calls.is_empty());
        self.pending.push_back(CompletionDelta {
            text: String::new(),
            done: true,
            completion: Some(Completion {
                text: turn.text,
                tool_calls,
                usage: self.provider.pricing.priced_usage(
                    &self.provider.price_ref,
                    Usage {
                        input_tokens: turn.prompt_tokens,
                        output_tokens: turn.completion_tokens,
                        cache_read_tokens: turn.cached_tokens,
                        ..Usage::default()
                    },
                ),
                stop_reason: stop_reason.map(stop_reason_wire),
                model: Some(self.provider.model.clone()),
            }),
        });
    }
}

fn assemble_tool_calls(fragments: BTreeMap<u64, ToolFragment>) -> Vec<ToolCall> {
    fragments
        .into_iter()
        .filter(|(_, slot)| !slot.name.is_empty())
        .map(|(index, slot)| {
            let arguments = if slot.arguments.trim().is_empty() {
                Map::new()
            } else {
                match serde_json::from_str(&slot.arguments) {
                    Ok(Value::Object(arguments)) => arguments,
                    // Carry the raw text so the loop can report the model's bad JSON.
                    _ => Map::from_iter([(
                        INVALID_TOOL_ARGS_KEY.to_owned(),
                        Value::String(slot.arguments.clone()),
                    )]),
                }
            };
            ToolCall {
                id: if slot.id.is_empty() {
                    format!("call_{index}")
                } else {
                    slot.id
                },
                name: slot.name,
                arguments,
            }
        })
        .collect()
}

/// A token counter where zero or null keeps the previous value, as in Python's `x or prev`.
fn counter(object: &Map<String, Value>, key: &str) -> Option<u64> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|&count| count != 0)
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn stop_reason_wire(reason: StopReason) -> String {
    match serde_json::to_value(reason) {
        Ok(Value::String(text)) => text,
        _ => String::new(),
    }
}

/// An error and its sources, joined like a Python exception message.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reasons_normalize_like_python() {
        assert_eq!(
            normalize_stop_reason(Some("stop"), false),
            Some(StopReason::EndTurn)
        );
        assert_eq!(
            normalize_stop_reason(Some("stop"), true),
            Some(StopReason::ToolUse)
        );
        assert_eq!(
            normalize_stop_reason(Some("length"), false),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            normalize_stop_reason(Some("content_filter"), false),
            Some(StopReason::Other)
        );
        assert_eq!(
            normalize_stop_reason(Some("surprise"), false),
            Some(StopReason::Other)
        );
        assert_eq!(normalize_stop_reason(None, true), Some(StopReason::ToolUse));
        assert_eq!(normalize_stop_reason(None, false), None);
    }

    #[test]
    fn max_tokens_rejections_need_the_parameter_and_a_refusal_word() {
        assert!(is_max_tokens_rejection(
            "Unrecognized request argument supplied: max_completion_tokens"
        ));
        assert!(!is_max_tokens_rejection(
            "max_completion_tokens is too large"
        ));
        assert!(!is_max_tokens_rejection("invalid model"));
    }

    #[test]
    fn malformed_tool_arguments_are_carried_raw() {
        let mut fragments = BTreeMap::new();
        fragments.insert(
            1,
            ToolFragment {
                id: String::new(),
                name: "read_file".to_owned(),
                arguments: "{\"path\": ".to_owned(),
            },
        );
        fragments.insert(
            0,
            ToolFragment {
                id: "call-a".to_owned(),
                name: "list_dir".to_owned(),
                arguments: "[1]".to_owned(),
            },
        );
        fragments.insert(2, ToolFragment::default());

        let calls = assemble_tool_calls(fragments);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call-a");
        assert_eq!(calls[0].invalid_arguments(), Some("[1]"));
        assert_eq!(calls[1].id, "call_1");
        assert_eq!(calls[1].invalid_arguments(), Some("{\"path\": "));
    }
}
