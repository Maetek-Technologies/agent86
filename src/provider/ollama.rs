//! Ollama provider, matching the Python `ollama_provider.py`: local models through
//! the `/api/chat` endpoint, streamed as NDJSON (one JSON object per line).
//!
//! Tools are passed through for models that support them (llama3.1, qwen2.5, ...).
//! Local inference has no token cost, so usage carries token counts at cost 0.0,
//! a real zero rather than a missing price.

use super::openai::error_chain;
use super::retry::{is_retryable_status, parse_retry_after, RetryPolicy};
use super::{DeltaStream, Provider, ProviderError};
use crate::secrets::redact;
use crate::types::{
    Completion, CompletionDelta, CompletionRequest, Message, Role, StopReason, ToolCall, ToolSpec,
    Usage, INVALID_TOOL_ARGS_KEY,
};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";
const START_HINT: &str = "Is it running? Start it with 'ollama serve'.";

/// Settings from `[providers.ollama]` that this adapter uses.
#[derive(Debug, Clone, PartialEq)]
pub struct OllamaConfig {
    pub base_url: Option<String>,
    /// The context window to ask for; Ollama's own default is small.
    pub num_ctx: Option<u32>,
    /// Standing output cap (`num_predict`), used when the request sets none.
    pub max_tokens: Option<u32>,
    pub connect_timeout: Duration,
    /// The maximum gap between received lines, not a total-request deadline.
    pub read_timeout: Duration,
    pub max_retries: u32,
}

#[derive(Debug)]
pub struct OllamaProvider {
    model: String,
    base_url: String,
    config: OllamaConfig,
    client: reqwest::Client,
}

/// Map an Ollama `done_reason` onto the shared [`StopReason`] vocabulary.
///
/// `load` and `unload` are lifecycle events rather than a finished answer. Ollama
/// reports `stop` even on a turn that asked for tools, so `has_tool_calls`
/// corrects that to `tool_use`.
pub fn normalize_stop_reason(raw: Option<&str>, has_tool_calls: bool) -> Option<StopReason> {
    let Some(raw) = raw else {
        return has_tool_calls.then_some(StopReason::ToolUse);
    };
    let mapped = match raw {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        _ => StopReason::Other,
    };
    if has_tool_calls && mapped == StopReason::EndTurn {
        Some(StopReason::ToolUse)
    } else {
        Some(mapped)
    }
}

/// Python's `repr` of a model name, as the error messages quote it.
fn py_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// Seconds as Python's `:g` writes them (`10`, `0.5`).
fn seconds(duration: Duration) -> String {
    crate::mcp::format_g(duration.as_secs_f64())
}

impl OllamaProvider {
    pub fn new(model: impl Into<String>, config: OllamaConfig) -> Result<Self, ProviderError> {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_owned();
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()
            .map_err(|error| ProviderError::Client {
                detail: error_chain(&error),
            })?;
        Ok(Self {
            model: model.into(),
            base_url,
            config,
            client,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/api/chat", self.base_url)
    }

    /// The output cap for this call (`options.num_predict`), or none, which leaves
    /// Ollama's own default (generate until the model stops): this is the user's own
    /// hardware, where an invented ceiling costs nothing but a truncated answer.
    fn max_tokens(&self, request: &CompletionRequest) -> Option<u32> {
        request
            .max_tokens
            .filter(|&cap| cap > 0)
            .or(self.config.max_tokens.filter(|&cap| cap > 0))
    }

    /// The streaming `/api/chat` payload for `request`.
    pub fn payload(&self, request: &CompletionRequest) -> Value {
        let mut options = Map::new();
        options.insert("temperature".to_owned(), json!(request.temperature));
        if let Some(num_ctx) = self.config.num_ctx.filter(|&n| n > 0) {
            // Room for the prompt and the answer; Ollama's small default window
            // otherwise fills with tool observations and cuts the answer short.
            options.insert("num_ctx".to_owned(), json!(num_ctx));
        }
        if let Some(cap) = self.max_tokens(request) {
            options.insert("num_predict".to_owned(), json!(cap));
        }
        let mut payload = json!({
            "model": self.model,
            "messages": to_messages(&request.messages),
            "stream": true,
            "options": options,
        });
        if !request.tools.is_empty() {
            payload["tools"] = to_tools(&request.tools);
        }
        payload
    }

    fn message(&self, text: String) -> ProviderError {
        ProviderError::Message(redact(&text, &[]))
    }

    fn cannot_reach(&self) -> ProviderError {
        self.message(format!(
            "Cannot reach Ollama at {}. {START_HINT}",
            self.base_url
        ))
    }

    fn connect_timeout(&self) -> ProviderError {
        self.message(format!(
            "Timed out connecting to {} after {}s (connect_timeout_s). {START_HINT} Raise \
             connect_timeout_s in [providers.ollama] to allow more time.",
            self.endpoint(),
            seconds(self.config.connect_timeout)
        ))
    }

    fn read_timeout(&self) -> ProviderError {
        self.message(format!(
            "Timed out waiting for {} after {}s with no data received (read_timeout_s). This is \
             the maximum gap between streamed chunks, not the total generation time — a long \
             but still-progressing generation is not affected. Raise read_timeout_s in \
             [providers.ollama] if this endpoint is just slow to respond.",
            self.endpoint(),
            seconds(self.config.read_timeout)
        ))
    }

    fn interrupted(&self, error: &reqwest::Error) -> ProviderError {
        self.message(format!(
            "ollama: the connection to {} failed while streaming model {} — {}. The server \
             closed the stream early; retry, or check that 'ollama serve' is healthy.",
            self.endpoint(),
            py_repr(&self.model),
            error_chain(error)
        ))
    }

    /// What a transport failure means, and whether Python would retry it (a
    /// refused or timed-out connect, or a stream cut off; never a read timeout).
    fn transport_error(&self, error: &reqwest::Error) -> (ProviderError, bool) {
        if error.is_timeout() && error.is_connect() {
            (self.connect_timeout(), true)
        } else if error.is_timeout() {
            (self.read_timeout(), false)
        } else if error.is_connect() {
            (self.cannot_reach(), true)
        } else {
            (self.interrupted(error), true)
        }
    }
}

impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn stream(&self, request: CompletionRequest) -> DeltaStream<'_> {
        let state = StreamState {
            provider: self,
            payload: self.payload(&request),
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

/// Ollama's message shape: tool results carry the tool's name, and an assistant's
/// tool calls carry their arguments as an object.
fn to_messages(messages: &[Message]) -> Value {
    messages
        .iter()
        .map(|message| match message.role {
            Role::Tool => json!({
                "role": "tool",
                "content": message.content,
                "name": message.name,
            }),
            Role::Assistant if !message.tool_calls.is_empty() => json!({
                "role": "assistant",
                "content": message.content,
                "tool_calls": message.tool_calls.iter().map(|call| json!({
                    "function": {"name": call.name, "arguments": call.arguments},
                })).collect::<Vec<_>>(),
            }),
            role => json!({"role": role, "content": message.content}),
        })
        .collect()
}

/// Ollama sends tool arguments as an object; a JSON string is tolerated too. Bad
/// JSON is carried raw, so the loop can report it rather than hand the tool junk.
fn as_arguments(arguments: Option<&Value>) -> Map<String, Value> {
    match arguments {
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(arguments)) => arguments,
            Ok(other) => Map::from_iter([("value".to_owned(), other)]),
            Err(_) => Map::from_iter([(
                INVALID_TOOL_ARGS_KEY.to_owned(),
                Value::String(text.clone()),
            )]),
        },
        _ => Map::new(),
    }
}

/// Accumulators for one attempt. A retry must not inherit half a response.
#[derive(Default)]
struct TurnState {
    text: String,
    tool_calls: Vec<ToolCall>,
    prompt_tokens: u64,
    eval_tokens: u64,
    done_reason: Option<String>,
    emitted: bool,
}

struct StreamState<'a> {
    provider: &'a OllamaProvider,
    payload: Value,
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
                    self.finish();
                }
                Err(error) => {
                    let (failure, retryable) = self.provider.transport_error(&error);
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
            let sent = provider
                .client
                .post(provider.endpoint())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(self.payload.to_string())
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(error) => {
                    let (failure, retryable) = provider.transport_error(&error);
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
            return Err(provider.message(format!("Ollama returned HTTP {status}: {}", body.trim())));
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
        if line.is_empty() {
            return Ok(());
        }
        let provider = self.provider;
        let chunk: Value = serde_json::from_str(line).map_err(|error| {
            // A truncated line, which would otherwise name neither the server nor the model.
            provider.message(format!(
                "ollama: {} returned a malformed NDJSON line for model {} — {error}. The stream \
                 was probably truncated; retry, or check the Ollama server log.",
                provider.endpoint(),
                py_repr(&provider.model)
            ))
        })?;
        if let Some(error) = chunk.get("error") {
            let error = match error {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            return Err(provider.message(format!("Ollama error: {error}")));
        }
        let message = chunk.get("message");
        if let Some(piece) = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .filter(|piece| !piece.is_empty())
        {
            self.turn.text.push_str(piece);
            self.turn.emitted = true;
            self.pending.push_back(CompletionDelta {
                text: piece.to_owned(),
                ..CompletionDelta::default()
            });
        }
        let calls = message
            .and_then(|m| m.get("tool_calls"))
            .and_then(Value::as_array);
        for call in calls.into_iter().flatten() {
            let function = call.get("function");
            let id = format!("ollama-{}", self.turn.tool_calls.len());
            self.turn.tool_calls.push(ToolCall {
                id,
                name: function
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: as_arguments(function.and_then(|f| f.get("arguments"))),
            });
        }
        if chunk.get("done").and_then(Value::as_bool).unwrap_or(false) {
            let count = |key: &str| chunk.get(key).and_then(Value::as_u64).unwrap_or(0);
            self.turn.prompt_tokens = count("prompt_eval_count");
            self.turn.eval_tokens = count("eval_count");
            self.turn.done_reason = chunk
                .get("done_reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        Ok(())
    }

    /// Queue the final delta carrying the assembled completion.
    fn finish(&mut self) {
        self.finished = true;
        self.response = None;
        let turn = std::mem::take(&mut self.turn);
        let stop_reason =
            normalize_stop_reason(turn.done_reason.as_deref(), !turn.tool_calls.is_empty());
        self.pending.push_back(CompletionDelta {
            text: String::new(),
            done: true,
            completion: Some(Completion {
                text: turn.text,
                tool_calls: turn.tool_calls,
                usage: Usage {
                    input_tokens: turn.prompt_tokens,
                    output_tokens: turn.eval_tokens,
                    cost_usd: 0.0,
                    ..Usage::default()
                },
                stop_reason: stop_reason.map(|reason| match serde_json::to_value(reason) {
                    Ok(Value::String(text)) => text,
                    _ => String::new(),
                }),
                model: Some(self.provider.model.clone()),
            }),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_reasons_normalize_like_python() {
        assert_eq!(
            normalize_stop_reason(Some("stop"), false),
            Some(StopReason::EndTurn)
        );
        assert_eq!(
            normalize_stop_reason(Some("stop"), true),
            Some(StopReason::ToolUse)
        );
        assert_eq!(
            normalize_stop_reason(Some("length"), true),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            normalize_stop_reason(Some("load"), false),
            Some(StopReason::Other)
        );
        assert_eq!(
            normalize_stop_reason(Some("unload"), false),
            Some(StopReason::Other)
        );
        assert_eq!(
            normalize_stop_reason(Some("new"), false),
            Some(StopReason::Other)
        );
        assert_eq!(normalize_stop_reason(None, true), Some(StopReason::ToolUse));
        assert_eq!(normalize_stop_reason(None, false), None);
    }

    #[test]
    fn arguments_may_be_an_object_or_a_json_string() {
        let object = json!({"path": "a"});
        assert_eq!(
            as_arguments(Some(&object)),
            object.as_object().unwrap().clone()
        );
        assert_eq!(
            as_arguments(Some(&json!("{\"path\": \"b\"}")))["path"],
            json!("b")
        );
        assert_eq!(as_arguments(Some(&json!("[1]")))["value"], json!([1]));
        assert_eq!(
            as_arguments(Some(&json!("{oops")))[INVALID_TOOL_ARGS_KEY],
            json!("{oops")
        );
        assert!(as_arguments(Some(&json!(3))).is_empty());
        assert!(as_arguments(None).is_empty());
    }

    #[test]
    fn model_names_are_quoted_as_python_repr_does() {
        assert_eq!(py_repr("llama3.1:8b"), "'llama3.1:8b'");
        assert_eq!(py_repr("it's"), "\"it's\"");
    }
}
