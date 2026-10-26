//! A single text-only turn: the slice of the Python orchestration loop that
//! `agent86 run` needs before tools and the ReAct loop are ported.
//!
//! The Python loop also continues answers cut off by `max_tokens` and applies
//! the egress guardrail; neither is ported yet.

use crate::pricing::format_cost;
use crate::provider::{Provider, ProviderError};
use crate::types::{CompletionRequest, Message, Role, Usage};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Instant;

/// What one user turn cost and did, matching the Python `TurnSummary` fields and order.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TurnSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    #[serde(serialize_with = "crate::pyjson::python_float")]
    pub cost_usd: f64,
    pub steps: u64,
    pub tool_calls: u64,
    #[serde(serialize_with = "crate::pyjson::python_float")]
    pub duration_s: f64,
    pub compactions: u64,
    pub continuations: u64,
}

impl TurnSummary {
    fn add_usage(&mut self, usage: &Usage) {
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cost_usd += usage.cost_usd;
        self.cache_read_tokens += usage.cache_read_tokens;
        self.cache_creation_tokens += usage.cache_creation_tokens;
    }
}

/// The `run --json` document. Field names and order are the scripting contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunOutput {
    pub session_id: String,
    pub output: String,
    pub steps: u64,
    pub usage: Usage,
    pub turn: Option<TurnSummary>,
}

/// A new ephemeral session id: 12 lowercase hex characters, like Python's `uuid4().hex[:12]`.
pub fn new_session_id() -> String {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    format!("{:012x}", hasher.finish() & 0xffff_ffff_ffff)
}

/// Run one text-only turn, passing each streamed text delta to `on_text`.
///
/// The request is `[system, user]`, the same shape as Python's first request of a turn.
pub async fn run_text_turn(
    provider: &dyn Provider,
    system: Message,
    goal: &str,
    max_tokens: u32,
    mut on_text: impl FnMut(&str),
) -> Result<RunOutput, ProviderError> {
    let started = Instant::now();
    let request = CompletionRequest {
        model: provider.model().to_owned(),
        messages: vec![
            system,
            Message {
                role: Role::User,
                content: goal.to_owned(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            },
        ],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: Some(max_tokens),
        stream: true,
    };

    let mut output = String::new();
    let mut usage = Usage::default();
    let mut stream = provider.stream(request);
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        if !delta.text.is_empty() {
            output.push_str(&delta.text);
            on_text(&delta.text);
        }
        if let Some(completion) = delta.completion.filter(|_| delta.done) {
            usage = completion.usage;
        }
    }

    let mut turn = TurnSummary {
        steps: 1,
        ..TurnSummary::default()
    };
    turn.add_usage(&usage);
    turn.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;

    Ok(RunOutput {
        session_id: new_session_id(),
        output,
        steps: 1,
        usage,
        turn: Some(turn),
    })
}

/// Compact token count: 512 -> `512`, 1234 -> `1.2k`, 32768 -> `33k`.
pub fn human_tokens(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else if count < 10_000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        // Python's round() rounds half to even.
        format!("{}k", (count as f64 / 1000.0).round_ties_even())
    }
}

/// One line summarising a finished turn, matching the Python `format_turn_summary`.
///
/// `priced` says whether a price is known for the turn's model; without one the
/// cost reads "n/a" rather than a misleading `$0.0000`.
pub fn format_turn_summary(summary: &TurnSummary, priced: bool) -> String {
    let cached = summary.cache_read_tokens + summary.cache_creation_tokens;
    let mut tokens = format!(
        "{} in / {} out",
        human_tokens(summary.input_tokens),
        human_tokens(summary.output_tokens)
    );
    if cached > 0 {
        tokens.push_str(&format!(" ({} cached)", human_tokens(cached)));
    }
    let mut parts = vec![
        plural(summary.steps, "step"),
        plural(summary.tool_calls, "tool"),
        tokens,
        format_cost(summary.cost_usd, priced),
        format!("{:.1}s", summary.duration_s),
    ];
    if summary.compactions > 0 {
        parts.push(plural(summary.compactions, "compaction"));
    }
    if summary.continuations > 0 {
        parts.push(plural(summary.continuations, "continuation"));
    }
    format!("— {}", parts.join(" · "))
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_are_compacted_like_python() {
        assert_eq!(human_tokens(512), "512");
        assert_eq!(human_tokens(1234), "1.2k");
        assert_eq!(human_tokens(32768), "33k");
        assert_eq!(human_tokens(12500), "12k");
        assert_eq!(human_tokens(1250), "1.2k");
    }

    #[test]
    fn summaries_drop_empty_cache_and_counters() {
        let summary = TurnSummary {
            input_tokens: 4100,
            output_tokens: 612,
            steps: 1,
            duration_s: 8.24,
            ..TurnSummary::default()
        };
        assert_eq!(
            format_turn_summary(&summary, false),
            "— 1 step · 0 tools · 4.1k in / 612 out · cost n/a (unpriced model) · 8.2s"
        );

        let cached = TurnSummary {
            cache_read_tokens: 1900,
            steps: 3,
            tool_calls: 1,
            continuations: 2,
            ..summary
        };
        assert_eq!(
            format_turn_summary(&TurnSummary { cost_usd: 0.01234, ..cached }, true),
            "— 3 steps · 1 tool · 4.1k in / 612 out (1.9k cached) · $0.0123 · 8.2s · 2 continuations"
        );
    }

    #[test]
    fn session_ids_are_twelve_hex_characters() {
        let id = new_session_id();
        assert_eq!(id.len(), 12);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
