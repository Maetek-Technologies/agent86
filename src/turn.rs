//! The record of one turn: the `run --json` document, the per-turn summary,
//! and the summary line printed on stderr. The loop itself is in `agent`.

use crate::pricing::format_cost;
use crate::types::Usage;
use serde::Serialize;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

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
    /// Fold one model call's usage in.
    pub fn add_usage(&mut self, usage: &Usage) {
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
