//! The unified model interface.
//!
//! Every backend implements [`Provider::stream`]; [`Provider::complete`] is
//! derived from it, so one API round-trip yields both live text and the
//! authoritative result. Streamed and non-streamed calls therefore always agree
//! on the final text.

pub mod capabilities;
pub mod catalog;
pub mod ollama;
pub mod openai;
pub mod remote_windows;
pub mod retry;

use crate::config::{Config, ProviderConfig};
use crate::pricing::PriceTable;
use crate::secrets::{resolve_api_key, OsKeyring, SecretStore};
use crate::types::{Completion, CompletionDelta, CompletionRequest, ModelRef};
use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, StreamExt};
use ollama::{OllamaConfig, OllamaProvider};
use openai::{OpenAiConfig, OpenAiProvider};
use std::time::Duration;
use thiserror::Error;

/// Providers with a dedicated adapter in Python that the Rust port does not have yet.
const NOT_PORTED: &[&str] = &["anthropic", "llamacpp"];

/// Construct the provider that serves `reference`, e.g. `openrouter:openai/gpt-4o-mini`.
///
/// Any provider section with a `base_url` is treated as OpenAI-compatible, so
/// OpenRouter, Groq, Together, vLLM, and LM Studio work with only a config block.
/// A key is required for `openai` itself, and for other sections only when they
/// name an `api_key_env`; a `base_url` without one is a keyless local endpoint.
///
/// API keys come from the environment variable named by `api_key_env`, then
/// from the OS keyring (service `agent86`, account = the provider section name).
pub fn provider_for_ref(
    reference: &ModelRef,
    config: &Config,
) -> Result<Box<dyn Provider>, ProviderError> {
    provider_for_ref_with_store(reference, config, &OsKeyring)
}

/// [`provider_for_ref`] with an explicit secret store, so tests never touch the OS keyring.
pub fn provider_for_ref_with_store(
    reference: &ModelRef,
    config: &Config,
    store: &dyn SecretStore,
) -> Result<Box<dyn Provider>, ProviderError> {
    build_provider(reference, config, |name, section| {
        resolve_api_key(name, section.api_key_env.as_deref(), store)
    })
}

/// [`provider_for_ref`] with a key that was typed but not stored yet: the
/// connection test sends it as it is, skipping the environment and the keyring,
/// as Python's `provider_for_ref(..., api_key=key)` does.
pub fn provider_for_ref_with_key(
    reference: &ModelRef,
    config: &Config,
    api_key: &str,
) -> Result<Box<dyn Provider>, ProviderError> {
    build_provider(reference, config, |_, _| Some(api_key.to_owned()))
}

fn build_provider(
    reference: &ModelRef,
    config: &Config,
    api_key: impl FnOnce(&str, &ProviderConfig) -> Option<String>,
) -> Result<Box<dyn Provider>, ProviderError> {
    let name = reference.provider.as_str();
    let section = config.providers.get(name).cloned().unwrap_or_default();

    // Ollama has its own adapter (and no key), whatever its section says.
    if name == "ollama" {
        let seconds = durations(name);
        let provider = OllamaProvider::new(
            reference.model.clone(),
            OllamaConfig {
                base_url: section.base_url.clone(),
                num_ctx: section.num_ctx,
                max_tokens: section.max_tokens,
                connect_timeout: seconds("connect_timeout_s", section.connect_timeout_s)?,
                read_timeout: seconds("read_timeout_s", section.read_timeout_s)?,
                max_retries: section.max_retries,
            },
        )?;
        return Ok(Box::new(provider));
    }

    let require_key = match name {
        "openai" | "openai-compatible" => true,
        _ if NOT_PORTED.contains(&name) => {
            return Err(ProviderError::NotPorted {
                provider: name.to_owned(),
            })
        }
        _ if section.base_url.is_some() => section.api_key_env.is_some(),
        _ => {
            return Err(ProviderError::UnknownProvider {
                provider: name.to_owned(),
            })
        }
    };

    let api_key = api_key(name, &section);
    let openai_config = openai_config(name, &section, require_key)?;
    let provider = OpenAiProvider::new(reference.model.clone(), openai_config, api_key)?
        .with_pricing(PriceTable::from_config(config), reference.to_string());
    Ok(Box::new(provider))
}

fn openai_config(
    name: &str,
    section: &ProviderConfig,
    require_key: bool,
) -> Result<OpenAiConfig, ProviderError> {
    let seconds = durations(name);
    Ok(OpenAiConfig {
        base_url: section.base_url.clone(),
        api_key_env: section.api_key_env.clone(),
        config_name: name.to_owned(),
        require_key,
        max_tokens: section.max_tokens,
        connect_timeout: seconds("connect_timeout_s", section.connect_timeout_s)?,
        read_timeout: seconds("read_timeout_s", section.read_timeout_s)?,
        max_retries: section.max_retries,
    })
}

/// A `[providers.<name>]` timeout as a duration, refusing negative or absurd values.
fn durations(name: &str) -> impl Fn(&str, f64) -> Result<Duration, ProviderError> + '_ {
    move |field, value| {
        Duration::try_from_secs_f64(value).map_err(|_| ProviderError::InvalidSetting {
            provider: name.to_owned(),
            detail: format!("{field} must be a non-negative number of seconds, not {value}"),
        })
    }
}

/// A stream of text deltas whose final item carries `done = true` and the full completion.
pub type DeltaStream<'a> = BoxStream<'a, Result<CompletionDelta, ProviderError>>;

/// Adapter behind one provider's API.
pub trait Provider: Send + Sync {
    /// Short adapter id, e.g. `openai` for every OpenAI-compatible endpoint.
    fn name(&self) -> &str;

    /// The concrete model this instance targets.
    fn model(&self) -> &str;

    /// Stream text deltas; the final delta carries `done = true` and the full completion.
    ///
    /// After any text delta has been yielded, a failure is surfaced as an error and
    /// never retried, because a retry would duplicate text the caller already saw.
    fn stream(&self, request: CompletionRequest) -> DeltaStream<'_>;

    /// Consume [`Provider::stream`] and return the assembled completion.
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<Completion, ProviderError>> {
        Box::pin(collect_completion(self.model(), self.stream(request)))
    }
}

/// Assemble a completion from a delta stream, matching the Python `complete()`.
pub async fn collect_completion(
    model: &str,
    mut deltas: DeltaStream<'_>,
) -> Result<Completion, ProviderError> {
    let mut text = String::new();
    let mut last = None;
    while let Some(delta) = deltas.next().await {
        let delta = delta?;
        text.push_str(&delta.text);
        if delta.done && delta.completion.is_some() {
            last = delta.completion;
        }
    }
    Ok(match last {
        None => Completion {
            text,
            model: Some(model.to_owned()),
            ..Completion::default()
        },
        Some(mut completion) => {
            if completion.text.is_empty() {
                completion.text = text;
            }
            completion
        }
    })
}

/// A provider could not be constructed or reached.
///
/// Every message is safe to show: response bodies and transport details are
/// redacted before they are stored here.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ProviderError {
    #[error(
        "Unknown provider '{provider}'. Built-in: anthropic, openai, ollama, llamacpp. \
         For an OpenAI-compatible endpoint, add a [providers.{provider}] block with a base_url \
         (e.g. OpenRouter, Groq, Together) to your config."
    )]
    UnknownProvider { provider: String },

    #[error(
        "The '{provider}' provider is not available in the Rust port yet. \
         Use an OpenAI-compatible provider such as openrouter, openai, or groq."
    )]
    NotPorted { provider: String },

    #[error("Invalid [providers.{provider}] setting: {detail}")]
    InvalidSetting { provider: String, detail: String },

    #[error(
        "No API key found. Set the {env} environment variable or store a key in the OS keyring (service \"agent86\", account \"{account}\")."
    )]
    MissingApiKey { env: String, account: String },

    #[error("Could not construct the HTTP client: {detail}")]
    Client { detail: String },

    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("Cannot reach {endpoint}: {detail}")]
    Connect { endpoint: String, detail: String },

    #[error(
        "Timed out connecting to {endpoint} after {seconds}s (connect_timeout_s). \
         Raise connect_timeout_s in the [providers.*] block for this endpoint to allow more time."
    )]
    ConnectTimeout { endpoint: String, seconds: f64 },

    #[error(
        "Timed out waiting for {endpoint} after {seconds}s with no data received (read_timeout_s). \
         This is the maximum gap between streamed chunks, not the total generation time — \
         a long but still-progressing generation is not affected. \
         Raise read_timeout_s in the [providers.*] block for this endpoint if this endpoint is just slow to respond."
    )]
    ReadTimeout { endpoint: String, seconds: f64 },

    #[error(
        "{provider}: {endpoint} returned a malformed streaming chunk for model {model:?} — \
         could not parse the SSE 'data:' line as JSON ({detail}). \
         The stream was probably truncated, or the endpoint is not OpenAI-compatible."
    )]
    MalformedChunk {
        provider: String,
        endpoint: String,
        model: String,
        detail: String,
    },

    #[error(
        "{provider}: the connection to {endpoint} failed while streaming model {model:?} — {detail}. \
         The server closed the stream early; retry, or check the endpoint and network."
    )]
    StreamInterrupted {
        provider: String,
        endpoint: String,
        model: String,
        detail: String,
    },

    /// A message already worded as Python's adapter words it (Ollama's).
    #[error("{0}")]
    Message(String),
}
