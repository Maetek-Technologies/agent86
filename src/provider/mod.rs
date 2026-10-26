//! The unified model interface and provider factory (Tier 3).
//!
//! Every backend implements [`ModelProvider::stream`]. [`complete`] is derived
//! from it, so streamed and non-streamed calls share one code path and produce
//! the same final text, tool calls, and usage.

mod openai;
mod retry;
mod sse;

pub use openai::{normalize_stop_reason, OpenAiProvider};

use crate::{
    config::{Config, ProviderConfig},
    types::{Completion, CompletionRequest, ModelRef},
};
use futures_util::future::BoxFuture;
use std::fmt;

/// Receives each text fragment as it arrives from the model.
pub type TextSink<'a> = &'a mut (dyn FnMut(&str) + Send);

/// Adapter behind one provider's API.
pub trait ModelProvider: Send + Sync {
    /// The concrete model this instance targets.
    fn model(&self) -> &str;

    /// Stream one completion, passing text fragments to `on_text` as they arrive.
    ///
    /// The returned [`Completion`] is authoritative: its text is the
    /// concatenation of every fragment passed to `on_text`.
    fn stream<'a>(
        &'a self,
        request: &'a CompletionRequest,
        on_text: TextSink<'a>,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>>;
}

/// Run a completion without observing the stream.
pub async fn complete(
    provider: &dyn ModelProvider,
    request: &CompletionRequest,
) -> Result<Completion, ProviderError> {
    provider.stream(request, &mut |_| {}).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The `provider:model` reference names no usable provider.
    UnknownProvider,
    /// The provider exists in the reference implementation but is not ported yet.
    NotPorted,
    /// The provider needs an API key and none was found.
    MissingApiKey,
    /// The provider configuration cannot be used (for example, a bad URL).
    InvalidConfig,
    /// The endpoint answered with a non-success HTTP status.
    Http,
    /// The endpoint reported an error inside an otherwise successful stream.
    Stream,
    /// The endpoint could not be reached.
    Connect,
    /// The connection did not complete within `connect_timeout_s`.
    ConnectTimeout,
    /// No data arrived within `read_timeout_s`.
    ReadTimeout,
    /// A streamed chunk was not valid JSON.
    MalformedStream,
    /// The stream ended before the model finished its turn.
    TruncatedStream,
    /// The connection failed part-way through the response.
    Transport,
}

/// A provider could not be constructed, reached, or understood.
///
/// Messages are written for the user and never contain the API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    kind: ProviderErrorKind,
    message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// Replace every occurrence of `secret` in the message.
    pub(crate) fn redacted(mut self, secret: Option<&str>) -> Self {
        if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
            self.message = self.message.replace(secret, "***");
        }
        self
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

/// Resolve an API key from the environment variable a provider section names.
///
/// An unset or empty variable is treated as no key. The OS keyring lookup that
/// the reference implementation performs after the environment is not ported yet.
pub fn resolve_api_key(api_key_env: Option<&str>) -> Option<String> {
    let name = api_key_env.filter(|name| !name.is_empty())?;
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Construct the provider that serves `reference`.
pub fn provider_for_ref(
    reference: &ModelRef,
    config: &Config,
) -> Result<Box<dyn ModelProvider>, ProviderError> {
    let section = config.provider(&reference.provider);
    let api_key = resolve_api_key(section.api_key_env.as_deref());
    build_provider(reference, section, api_key)
}

/// The provider dispatch table, with the key already resolved.
pub fn build_provider(
    reference: &ModelRef,
    section: ProviderConfig,
    api_key: Option<String>,
) -> Result<Box<dyn ModelProvider>, ProviderError> {
    match reference.provider.as_str() {
        "anthropic" | "ollama" | "llamacpp" => Err(ProviderError::new(
            ProviderErrorKind::NotPorted,
            format!(
                "The '{}' provider is not available in the Rust port yet. Use an \
                 OpenAI-compatible provider: openai, openrouter, groq, or a \
                 [providers.<name>] block with a base_url.",
                reference.provider
            ),
        )),
        "openai" | "openai-compatible" => Ok(Box::new(OpenAiProvider::new(
            &reference.model,
            section,
            api_key,
            true,
        )?)),
        // Any other section with a base_url is an OpenAI-compatible endpoint. A key is
        // required only when the section names an env var for one; a base_url with no
        // api_key_env is a keyless local endpoint.
        _ if section.base_url.is_some() => {
            let require_key = section.api_key_env.is_some();
            Ok(Box::new(OpenAiProvider::new(
                &reference.model,
                section,
                api_key,
                require_key,
            )?))
        }
        _ => Err(ProviderError::new(
            ProviderErrorKind::UnknownProvider,
            format!(
                "Unknown provider '{name}'. Built-in: anthropic, openai, ollama, llamacpp. \
                 For an OpenAI-compatible endpoint, add a [providers.{name}] block with a \
                 base_url (e.g. OpenRouter, Groq, Together) to your config.",
                name = reference.provider
            ),
        )),
    }
}
