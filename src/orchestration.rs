//! A single text-only turn: the slice of the ReAct loop this milestone needs.
//!
//! There are no tools yet, so a turn is exactly one model call and one step.
//! The record shapes match the Python reference so `run --json` keeps its
//! field names from the beginning.

use crate::{
    provider::{ModelProvider, ProviderError},
    types::{CompletionRequest, Message, Role, Usage},
};
use serde::Serialize;
use std::time::Instant;

/// What one user turn cost and did.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TurnSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: f64,
    pub steps: u64,
    pub tool_calls: u64,
    pub duration_s: f64,
    pub compactions: u64,
    pub continuations: u64,
}

impl TurnSummary {
    fn add_usage(&mut self, usage: &Usage) {
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens;
        self.cache_creation_tokens += usage.cache_creation_tokens;
        self.cost_usd += usage.cost_usd;
    }
}

/// The `run --json` payload, field for field.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunOutput {
    pub session_id: String,
    pub output: String,
    pub steps: u64,
    pub usage: Usage,
    pub turn: Option<TurnSummary>,
}

/// A twelve-hex-digit session identifier, the reference format.
///
/// Sessions are not persisted yet, so the identifier is ephemeral.
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

/// Run one text-only turn for `goal`, passing streamed text to `on_text`.
pub async fn run_turn(
    provider: &dyn ModelProvider,
    goal: &str,
    on_text: &mut (dyn FnMut(&str) + Send),
) -> Result<RunOutput, ProviderError> {
    let started = Instant::now();
    let request = CompletionRequest {
        model: provider.model().to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: goal.to_owned(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: None,
        stream: true,
    };

    let completion = provider.stream(&request, on_text).await?;

    let mut turn = TurnSummary {
        steps: 1,
        duration_s: started.elapsed().as_secs_f64(),
        ..TurnSummary::default()
    };
    turn.add_usage(&completion.usage);

    Ok(RunOutput {
        session_id: new_session_id(),
        output: completion.text,
        steps: 1,
        usage: completion.usage,
        turn: Some(turn),
    })
}

/// The one-line stderr summary printed after a plain `run`.
///
/// Example: `— 1 step · 0 tools · 1.2k in / 64 out · cost n/a (unpriced model) · 0.8s`.
/// There is no price table yet, so every model reports as unpriced rather than
/// claiming a cost of zero.
pub fn format_turn_summary(turn: &TurnSummary) -> String {
    let cached = turn.cache_read_tokens + turn.cache_creation_tokens;
    let mut tokens = format!(
        "{} in / {} out",
        human_tokens(turn.input_tokens),
        human_tokens(turn.output_tokens)
    );
    if cached > 0 {
        tokens.push_str(&format!(" ({} cached)", human_tokens(cached)));
    }
    let mut parts = vec![
        plural(turn.steps, "step"),
        plural(turn.tool_calls, "tool"),
        tokens,
        "cost n/a (unpriced model)".to_owned(),
        format!("{:.1}s", turn.duration_s),
    ];
    if turn.compactions > 0 {
        parts.push(plural(turn.compactions, "compaction"));
    }
    if turn.continuations > 0 {
        parts.push(plural(turn.continuations, "continuation"));
    }
    format!("— {}", parts.join(" · "))
}

/// Compact token count: 512 -> "512", 1234 -> "1.2k", 32768 -> "33k".
pub fn human_tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 10_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{}k", (count as f64 / 1_000.0).round())
    }
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
    fn session_ids_are_twelve_hex_digits() {
        let id = new_session_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_counts_render_compactly() {
        assert_eq!(human_tokens(512), "512");
        assert_eq!(human_tokens(1_234), "1.2k");
        assert_eq!(human_tokens(32_768), "33k");
    }

    #[test]
    fn the_turn_line_matches_the_reference_layout() {
        let turn = TurnSummary {
            input_tokens: 4_100,
            output_tokens: 612,
            cache_read_tokens: 1_900,
            steps: 1,
            duration_s: 8.24,
            ..TurnSummary::default()
        };
        assert_eq!(
            format_turn_summary(&turn),
            "— 1 step · 0 tools · 4.1k in / 612 out (1.9k cached) · cost n/a (unpriced model) · 8.2s"
        );
    }
}
