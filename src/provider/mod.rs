//! The unified model interface.
//!
//! Every backend implements [`Provider::stream`]; [`Provider::complete`] is
//! derived from it, so one API round-trip yields both live text and the
//! authoritative result. Streamed and non-streamed calls therefore always agree
//! on the final text.

pub mod capabilities;
pub mod openai;
pub mod retry;

use crate::types::{Completion, CompletionDelta, CompletionRequest};
use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, StreamExt};
use thiserror::Error;

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
    #[error("No API key found. Set the {env} environment variable.")]
    MissingApiKey { env: String },

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
}
