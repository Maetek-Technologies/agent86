//! Provider-neutral types shared across the agent harness.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{fmt, ops::Add, str::FromStr};
use thiserror::Error;

/// Sentinel used when a provider returns tool arguments that are not valid JSON.
pub const INVALID_TOOL_ARGS_KEY: &str = "__invalid_json__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    Init,
    Plan,
    Execute,
    Verify,
    Done,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Auto,
    Ask,
    Deny,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    /// A mode name from `/mode`, as Python's `parse_mode`: trimmed, any case.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// The next mode in Python's cycle: ask, auto, deny, ask.
    pub fn cycle(self) -> Self {
        match self {
            Self::Ask => Self::Auto,
            Self::Auto => Self::Deny,
            Self::Deny => Self::Ask,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Other,
}

/// A parsed `provider:model` reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    /// Parse a model reference, splitting only on the first colon.
    pub fn parse(reference: &str) -> Result<Self, ModelRefParseError> {
        let (provider, model) =
            reference
                .split_once(':')
                .ok_or_else(|| ModelRefParseError::MissingSeparator {
                    reference: reference.to_owned(),
                })?;
        let provider = provider.trim();
        let model = model.trim();

        if provider.is_empty() || model.is_empty() {
            return Err(ModelRefParseError::EmptyComponent {
                reference: reference.to_owned(),
            });
        }

        Ok(Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.provider, self.model)
    }
}

impl FromStr for ModelRef {
    type Err = ModelRefParseError;

    fn from_str(reference: &str) -> Result<Self, Self::Err> {
        Self::parse(reference)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelRefParseError {
    #[error(
        "Model reference {} must be 'provider:model' (e.g. 'anthropic:claude-opus-4-8').",
        python_repr(reference)
    )]
    MissingSeparator { reference: String },
    #[error(
        "Model reference {} has an empty provider or model.",
        python_repr(reference)
    )]
    EmptyComponent { reference: String },
}

/// Quote a string the way Python's `repr()` does, so messages match the reference.
pub fn python_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub side_effecting: bool,
}

impl ToolSpec {
    /// Construct a tool specification with an empty JSON-Schema object.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            side_effecting: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

impl ToolCall {
    /// Return the malformed argument payload intercepted from a provider.
    pub fn invalid_arguments(&self) -> Option<&str> {
        self.arguments.get(INVALID_TOOL_ARGS_KEY)?.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub ok: bool,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, serialize_with = "crate::pyjson::python_float")]
    pub cost_usd: f64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

impl Add for Usage {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        Self {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cost_usd: self.cost_usd + other.cost_usd,
            cache_read_tokens: self.cache_read_tokens + other.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens + other.cache_creation_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub temperature: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The model's reasoning, kept apart from `text` (Ollama's `thinking`, the
    /// OpenAI-compatible `reasoning_content`/`reasoning`, Anthropic thinking blocks).
    /// Beyond Python, which drops it: it is never sent back or saved, only shown and,
    /// when the model answers with nothing else, offered in place of the answer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionDelta {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<Completion>,
    /// A piece of the model's reasoning; see [`Completion::thinking`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub index: u64,
    pub phase: AgentPhase,
    #[serde(default)]
    pub thought: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ToolResult>,
    #[serde(default)]
    pub usage: Usage,
}

const fn default_true() -> bool {
    true
}
