//! OpenAI-compatible provider (Tier 3).
//!
//! One adapter for the Chat Completions API and every service that speaks it:
//! OpenAI, OpenRouter, Groq, Together, vLLM, LM Studio, and other compatible
//! endpoints. Streaming tool calls arrive as fragments across SSE chunks and
//! are accumulated by index into whole [`ToolCall`] values.

use super::{
    retry::{backoff_delay, is_retryable_status, parse_retry_after},
    sse::{SseDecoder, SseEvent},
    ModelProvider, ProviderError, ProviderErrorKind, TextSink,
};
use crate::{
    config::ProviderConfig,
    types::{
        Completion, CompletionRequest, Message, Role, StopReason, ToolCall, ToolSpec, Usage,
        INVALID_TOOL_ARGS_KEY,
    },
};
use futures_util::{future::BoxFuture, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashSet},
    error::Error as _,
    sync::{Mutex, OnceLock},
    time::Duration,
};

const ADAPTER: &str = "openai";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const ERROR_BODY_LIMIT: usize = 400;

/// Model families that reject `temperature`, `top_p`, and `top_k` outright.
const NO_SAMPLING_FAMILIES: &[&str] = &[
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-5",
    "claude-fable-5",
    "fable-5",
];

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

/// Endpoints that rejected `max_completion_tokens` this process.
///
/// OpenAI renamed the output cap and rejects the old `max_tokens` on its
/// reasoning models, while most compatible servers only know the old name. The
/// adapter sends the new name, learns from a 400, and remembers per endpoint.
fn legacy_max_tokens_endpoints() -> &'static Mutex<HashSet<String>> {
    static ENDPOINTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ENDPOINTS.get_or_init(Default::default)
}

fn uses_legacy_max_tokens(url: &str) -> bool {
    legacy_max_tokens_endpoints()
        .lock()
        .map(|endpoints| endpoints.contains(url))
        .unwrap_or(false)
}

fn mark_legacy_max_tokens(url: &str) {
    if let Ok(mut endpoints) = legacy_max_tokens_endpoints().lock() {
        endpoints.insert(url.to_owned());
    }
}

fn is_max_tokens_rejection(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("max_completion_tokens")
        && PARAM_REJECTION_WORDS
            .iter()
            .any(|word| lower.contains(word))
}

fn supports_sampling_params(model: &str) -> bool {
    let ident = model
        .split_once(':')
        .map_or(model, |(_, rest)| rest)
        .trim()
        .to_lowercase();
    !NO_SAMPLING_FAMILIES
        .iter()
        .any(|family| ident.starts_with(family))
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

fn stop_reason_name(reason: StopReason) -> String {
    match serde_json::to_value(reason) {
        Ok(Value::String(name)) => name,
        _ => "other".to_owned(),
    }
}

pub struct OpenAiProvider {
    model: String,
    config: ProviderConfig,
    url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(
        model: &str,
        config: ProviderConfig,
        api_key: Option<String>,
        require_key: bool,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.filter(|key| !key.is_empty());
        if require_key && api_key.is_none() {
            let env = config.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
            return Err(ProviderError::new(
                ProviderErrorKind::MissingApiKey,
                format!("No API key found. Set the {env} environment variable."),
            ));
        }

        let base = config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        // Tolerate a base_url given with or without the /v1 suffix.
        let suffix = if base.ends_with("/v1") { "" } else { "/v1" };
        let url = format!("{base}{suffix}/chat/completions");
        if reqwest::Url::parse(&url).is_err() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidConfig,
                format!("The provider base_url {base:?} is not a valid URL."),
            ));
        }

        let connect = timeout("connect_timeout_s", config.connect_timeout_s)?;
        let read = timeout("read_timeout_s", config.read_timeout_s)?;
        let client = reqwest::Client::builder()
            .connect_timeout(connect)
            .read_timeout(read)
            .build()
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::InvalidConfig,
                    format!(
                        "Could not construct the HTTP client — {}",
                        error_chain(&err)
                    ),
                )
            })?;

        Ok(Self {
            model: model.to_owned(),
            config,
            url,
            api_key,
            client,
        })
    }

    /// The fully resolved chat-completions endpoint.
    pub fn url(&self) -> &str {
        &self.url
    }

    fn payload(&self, request: &CompletionRequest) -> Map<String, Value> {
        let mut payload = Map::new();
        payload.insert("model".into(), json!(self.model));
        payload.insert("messages".into(), to_messages(&request.messages));
        payload.insert("stream".into(), json!(true));
        payload.insert("stream_options".into(), json!({"include_usage": true}));
        if supports_sampling_params(&self.model) {
            payload.insert("temperature".into(), json!(request.temperature));
        }
        // The request wins, then the provider section. No cap at all is sent otherwise:
        // a guessed ceiling would truncate long generations on local servers.
        if let Some(max_tokens) = request.max_tokens.or(self.config.max_tokens) {
            let key = if uses_legacy_max_tokens(&self.url) {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            payload.insert(key.into(), json!(max_tokens));
        }
        if !request.tools.is_empty() {
            payload.insert("tools".into(), to_tools(&request.tools));
            payload.insert("tool_choice".into(), json!("auto"));
        }
        payload
    }

    async fn stream_inner(
        &self,
        request: &CompletionRequest,
        on_text: TextSink<'_>,
    ) -> Result<Completion, ProviderError> {
        let mut payload = self.payload(request);
        let max_retries = self.config.max_retries;
        // The parameter-name swap is a deterministic correction, not a flaky endpoint, so
        // it does not consume the retry budget and stays available with retries disabled.
        let mut attempt = 0;
        let mut swapped = false;

        loop {
            let body = serde_json::to_vec(&payload).unwrap_or_default();
            let mut builder = self
                .client
                .post(&self.url)
                .header(CONTENT_TYPE, "application/json")
                .body(body);
            if let Some(key) = &self.api_key {
                builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
            }

            let response = match builder.send().await {
                Ok(response) => response,
                Err(err) => {
                    if err.is_connect() && attempt < max_retries {
                        tokio::time::sleep(backoff_delay(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(self.request_error(&err));
                }
            };

            let status = response.status().as_u16();
            if status != 200 {
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let text = response.text().await.unwrap_or_default();
                if status == 400
                    && !swapped
                    && payload.contains_key("max_completion_tokens")
                    && is_max_tokens_rejection(&text)
                {
                    swapped = true;
                    mark_legacy_max_tokens(&self.url);
                    if let Some(value) = payload.remove("max_completion_tokens") {
                        payload.insert("max_tokens".into(), value);
                    }
                    continue;
                }
                if is_retryable_status(status) && attempt < max_retries {
                    let delay = parse_retry_after(retry_after.as_deref())
                        .unwrap_or_else(|| backoff_delay(attempt));
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                let detail: String = text.trim().chars().take(ERROR_BODY_LIMIT).collect();
                return Err(ProviderError::new(
                    ProviderErrorKind::Http,
                    format!("HTTP {status}: {detail}"),
                ));
            }

            let mut emitted = false;
            match self.read_stream(response, on_text, &mut emitted).await {
                Ok(completion) => return Ok(completion),
                Err(StreamFailure::Body(err))
                    if !emitted && !err.is_timeout() && attempt < max_retries =>
                {
                    // The connection died before any text reached the consumer, so a
                    // second attempt cannot duplicate anything.
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    attempt += 1;
                }
                Err(StreamFailure::Body(err)) => return Err(self.body_error(&err)),
                Err(StreamFailure::Provider(err)) => return Err(err),
            }
        }
    }

    async fn read_stream(
        &self,
        response: reqwest::Response,
        on_text: TextSink<'_>,
        emitted: &mut bool,
    ) -> Result<Completion, StreamFailure> {
        let mut state = StreamState::default();
        let mut decoder = SseDecoder::default();
        let mut chunks = response.bytes_stream();
        let mut done = false;

        'read: while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(StreamFailure::Body)?;
            for event in decoder.feed(&chunk) {
                match event {
                    SseEvent::Done => {
                        done = true;
                        break 'read;
                    }
                    SseEvent::Data(data) => self.apply(&mut state, &data, on_text, emitted)?,
                }
            }
        }
        if !done {
            match decoder.finish() {
                Some(SseEvent::Done) => done = true,
                Some(SseEvent::Data(data)) => self.apply(&mut state, &data, on_text, emitted)?,
                None => {}
            }
        }
        if !done && state.finish_reason.is_none() {
            return Err(StreamFailure::Provider(ProviderError::new(
                ProviderErrorKind::TruncatedStream,
                format!(
                    "{ADAPTER}: {url} ended the stream for model '{model}' before the model \
                     finished (no finish_reason and no [DONE]). The partial response was \
                     discarded; retry, or check the endpoint and network.",
                    url = self.url,
                    model = self.model
                ),
            )));
        }
        Ok(state.into_completion(&self.model))
    }

    fn apply(
        &self,
        state: &mut StreamState,
        data: &str,
        on_text: TextSink<'_>,
        emitted: &mut bool,
    ) -> Result<(), StreamFailure> {
        if data.is_empty() {
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|err| {
            StreamFailure::Provider(self.malformed(&format!(
                "could not parse the SSE 'data:' line as JSON ({err})"
            )))
        })?;
        let Some(chunk) = chunk.as_object() else {
            return Err(StreamFailure::Provider(
                self.malformed("the SSE 'data:' line is JSON but not an object"),
            ));
        };

        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            return Err(StreamFailure::Provider(ProviderError::new(
                ProviderErrorKind::Stream,
                format!(
                    "{ADAPTER}: {url} reported an error mid-stream for model '{model}': {detail}",
                    url = self.url,
                    model = self.model
                ),
            )));
        }

        if let Some(usage) = chunk.get("usage").and_then(Value::as_object) {
            update_count(&mut state.prompt_tokens, usage.get("prompt_tokens"));
            update_count(&mut state.completion_tokens, usage.get("completion_tokens"));
            // The cached slice is a SUBSET of prompt_tokens, which is the Usage convention.
            if let Some(details) = usage.get("prompt_tokens_details") {
                update_count(&mut state.cached_tokens, details.get("cached_tokens"));
            }
        }

        let choices = chunk.get("choices").and_then(Value::as_array);
        for choice in choices.into_iter().flatten() {
            let delta = choice.get("delta");
            if let Some(piece) = delta
                .and_then(|delta| delta.get("content"))
                .and_then(Value::as_str)
                .filter(|piece| !piece.is_empty())
            {
                state.text.push_str(piece);
                *emitted = true;
                on_text(piece);
            }
            let fragments = delta
                .and_then(|delta| delta.get("tool_calls"))
                .and_then(Value::as_array);
            for fragment in fragments.into_iter().flatten() {
                state.accumulate(fragment);
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                state.finish_reason = Some(reason.to_owned());
            }
        }
        Ok(())
    }

    fn malformed(&self, detail: &str) -> ProviderError {
        ProviderError::new(
            ProviderErrorKind::MalformedStream,
            format!(
                "{ADAPTER}: {url} returned a malformed streaming chunk for model '{model}' — \
                 {detail}. The stream was probably truncated, or the endpoint is not \
                 OpenAI-compatible.",
                url = self.url,
                model = self.model
            ),
        )
    }

    fn request_error(&self, err: &reqwest::Error) -> ProviderError {
        if err.is_timeout() && err.is_connect() {
            ProviderError::new(
                ProviderErrorKind::ConnectTimeout,
                format!(
                    "Timed out connecting to {url} after {seconds}s (connect_timeout_s). Raise \
                     connect_timeout_s in the [providers.*] block for this endpoint to allow \
                     more time.",
                    url = self.url,
                    seconds = format_seconds(self.config.connect_timeout_s)
                ),
            )
        } else if err.is_timeout() {
            self.read_timeout_error()
        } else if err.is_connect() {
            ProviderError::new(
                ProviderErrorKind::Connect,
                format!("Cannot reach {}: {}", self.url, error_chain(err)),
            )
        } else {
            self.body_error(err)
        }
    }

    fn body_error(&self, err: &reqwest::Error) -> ProviderError {
        if err.is_timeout() {
            return self.read_timeout_error();
        }
        ProviderError::new(
            ProviderErrorKind::Transport,
            format!(
                "{ADAPTER}: the connection to {url} failed while streaming model '{model}' — \
                 {detail}. The server closed the stream early; retry, or check the endpoint \
                 and network.",
                url = self.url,
                model = self.model,
                detail = error_chain(err)
            ),
        )
    }

    fn read_timeout_error(&self) -> ProviderError {
        ProviderError::new(
            ProviderErrorKind::ReadTimeout,
            format!(
                "Timed out waiting for {url} after {seconds}s with no data received \
                 (read_timeout_s). This is the maximum gap between streamed chunks, not the \
                 total generation time — a long but still-progressing generation is not \
                 affected. Raise read_timeout_s in the [providers.*] block for this endpoint \
                 if this endpoint is just slow to respond.",
                url = self.url,
                seconds = format_seconds(self.config.read_timeout_s)
            ),
        )
    }
}

impl ModelProvider for OpenAiProvider {
    fn model(&self) -> &str {
        &self.model
    }

    fn stream<'a>(
        &'a self,
        request: &'a CompletionRequest,
        on_text: TextSink<'a>,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(async move {
            self.stream_inner(request, on_text)
                .await
                .map_err(|err| err.redacted(self.api_key.as_deref()))
        })
    }
}

enum StreamFailure {
    Body(reqwest::Error),
    Provider(ProviderError),
}

#[derive(Default)]
struct ToolFragment {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamState {
    text: String,
    tool_fragments: BTreeMap<u64, ToolFragment>,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    finish_reason: Option<String>,
}

impl StreamState {
    fn accumulate(&mut self, fragment: &Value) {
        let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0);
        let slot = self.tool_fragments.entry(index).or_default();
        if let Some(id) = fragment
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            slot.id = id.to_owned();
        }
        let function = fragment.get("function");
        if let Some(name) = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            slot.name = name.to_owned();
        }
        if let Some(arguments) = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
        {
            slot.arguments.push_str(arguments);
        }
    }

    fn into_completion(self, model: &str) -> Completion {
        let tool_calls: Vec<ToolCall> = self
            .tool_fragments
            .into_iter()
            .filter(|(_, slot)| !slot.name.is_empty())
            .map(|(index, slot)| assemble(index, slot))
            .collect();
        let stop_reason =
            normalize_stop_reason(self.finish_reason.as_deref(), !tool_calls.is_empty())
                .map(stop_reason_name);
        Completion {
            text: self.text,
            tool_calls,
            usage: Usage {
                input_tokens: self.prompt_tokens,
                output_tokens: self.completion_tokens,
                cost_usd: 0.0,
                cache_read_tokens: self.cached_tokens,
                cache_creation_tokens: 0,
            },
            stop_reason,
            model: Some(model.to_owned()),
        }
    }
}

fn assemble(index: u64, slot: ToolFragment) -> ToolCall {
    let arguments = if slot.arguments.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(&slot.arguments) {
            Ok(Value::Object(arguments)) => arguments,
            // Carry the raw text instead of {}: an empty object would send the model to
            // fix a missing field when the actual bug is its own JSON.
            _ => {
                let mut invalid = Map::new();
                invalid.insert(INVALID_TOOL_ARGS_KEY.into(), Value::String(slot.arguments));
                invalid
            }
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
}

/// Keep the previous count when a usage chunk reports 0 or omits the field.
fn update_count(count: &mut u64, value: Option<&Value>) {
    if let Some(value) = value.and_then(Value::as_u64).filter(|value| *value > 0) {
        *count = value;
    }
}

fn to_messages(messages: &[Message]) -> Value {
    Value::Array(
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
                    "content": if message.content.is_empty() {
                        Value::Null
                    } else {
                        Value::String(message.content.clone())
                    },
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
            .collect(),
    )
}

fn to_tools(tools: &[ToolSpec]) -> Value {
    Value::Array(
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
            .collect(),
    )
}

fn timeout(field: &str, seconds: f64) -> Result<Duration, ProviderError> {
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidConfig,
                format!("{field} must be a positive number of seconds, got {seconds}."),
            )
        })
}

/// Render seconds like Python's `:g` for the values a config holds (10.0 -> "10").
fn format_seconds(seconds: f64) -> String {
    if seconds.fract() == 0.0 && seconds.abs() < 1e15 {
        format!("{}", seconds as i64)
    } else {
        format!("{seconds}")
    }
}

/// An error and its sources, joined so the root cause is visible.
fn error_chain(err: &reqwest::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_follow_the_python_mapping() {
        let cases = [
            (Some("stop"), false, Some(StopReason::EndTurn)),
            (Some("stop"), true, Some(StopReason::ToolUse)),
            (Some("tool_calls"), false, Some(StopReason::ToolUse)),
            (Some("function_call"), false, Some(StopReason::ToolUse)),
            (Some("length"), false, Some(StopReason::MaxTokens)),
            (Some("content_filter"), false, Some(StopReason::Other)),
            (Some("something_new"), false, Some(StopReason::Other)),
            (None, false, None),
            (None, true, Some(StopReason::ToolUse)),
        ];
        for (raw, tools, expected) in cases {
            assert_eq!(
                normalize_stop_reason(raw, tools),
                expected,
                "{raw:?}/{tools}"
            );
        }
    }

    #[test]
    fn sampling_params_are_omitted_for_models_that_removed_them() {
        assert!(supports_sampling_params("gpt-4o-mini"));
        assert!(supports_sampling_params("anthropic/claude-3.7-sonnet"));
        assert!(!supports_sampling_params("claude-opus-4-8"));
        assert!(!supports_sampling_params(
            "openrouter:Claude-Sonnet-5-latest"
        ));
    }

    #[test]
    fn max_tokens_rejections_are_recognised() {
        assert!(is_max_tokens_rejection(
            "Unrecognized request argument supplied: max_completion_tokens"
        ));
        assert!(!is_max_tokens_rejection(
            "max_completion_tokens is too large"
        ));
        assert!(!is_max_tokens_rejection("unknown model"));
    }

    #[test]
    fn tool_fragments_assemble_by_index_and_keep_invalid_json() {
        let mut state = StreamState::default();
        for fragment in [
            json!({"index": 1, "id": "b", "function": {"name": "second", "arguments": "{oops"}}),
            json!({"index": 0, "id": "a", "function": {"name": "first", "arguments": "{\"x\":"}}),
            json!({"index": 0, "function": {"arguments": " 1}"}}),
            json!({"index": 2, "function": {"arguments": "{}"}}),
        ] {
            state.accumulate(&fragment);
        }
        let completion = state.into_completion("m");
        assert_eq!(
            completion.tool_calls.len(),
            2,
            "nameless fragments are dropped"
        );
        assert_eq!(completion.tool_calls[0].name, "first");
        assert_eq!(completion.tool_calls[0].arguments.get("x"), Some(&json!(1)));
        assert_eq!(completion.tool_calls[1].invalid_arguments(), Some("{oops"));
        assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn base_urls_gain_v1_only_when_missing() {
        let build = |base: &str| {
            let config = ProviderConfig {
                base_url: Some(base.to_owned()),
                ..ProviderConfig::default()
            };
            OpenAiProvider::new("m", config, None, false)
                .unwrap()
                .url()
                .to_owned()
        };
        assert_eq!(
            build("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            build("http://localhost:1234"),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    fn a_missing_key_names_the_env_var() {
        let config = ProviderConfig {
            api_key_env: Some("OPENROUTER_API_KEY".into()),
            ..ProviderConfig::default()
        };
        let err = OpenAiProvider::new("m", config, Some(String::new()), true)
            .err()
            .unwrap();
        assert_eq!(err.kind(), ProviderErrorKind::MissingApiKey);
        assert!(err.message().contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn non_positive_timeouts_are_configuration_errors() {
        for bad in [0.0, -1.0, f64::NAN] {
            let config = ProviderConfig {
                read_timeout_s: bad,
                ..ProviderConfig::default()
            };
            let err = OpenAiProvider::new("m", config, None, false).err().unwrap();
            assert_eq!(err.kind(), ProviderErrorKind::InvalidConfig);
        }
    }

    #[test]
    fn seconds_render_like_python_g_format() {
        assert_eq!(format_seconds(10.0), "10");
        assert_eq!(format_seconds(0.5), "0.5");
    }
}
