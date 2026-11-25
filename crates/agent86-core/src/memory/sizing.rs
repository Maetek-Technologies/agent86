//! Context sized to the window, beyond Python (slice 2b of the local-model work
//! in `docs/PROJECT_PLAN.md`). Python fits a request only by dropping its oldest
//! messages, and caps every tool result at a fixed 100,000 characters, most of a
//! 32k window in one result. Here, three mechanical steps come first, none of
//! which needs a model call, so none can fail:
//!
//! - [`result_cap`] and [`shorten_middle`]: a tool result is cut, as the loop
//!   records it, to a share of the conversation budget, keeping its start and its
//!   end (where test runners and logs put what matters) and saying how to see
//!   the middle.
//! - [`aged_view`]: once a request passes [`AGE_START_SHARE`] of the budget, tool
//!   results older than the newest [`RECENT_SHARE`] of it are sent as short stubs.
//!   The session and the trace keep them whole; only the request is shortened.
//! - [`Calibration`]: the input tokens each response reports, against what the
//!   estimate said, per model; a model whose tokenizer counts more than the
//!   estimate gets a proportionally smaller budget.
//!
//! The shares are starting points for the eval set to decide, not claims.

use crate::memory::working::count_tokens;
use crate::types::{Message, Role};
use std::collections::HashMap;
use std::sync::Mutex;

/// A tool result may take at most this fraction (one in N) of the
/// conversation budget, in estimated tokens.
pub const RESULT_SHARE: u64 = 3;

/// The smallest cap [`result_cap`] gives, in characters, so a tiny window still
/// sees a useful page of output.
pub const MIN_RESULT_CHARS: usize = 8_000;

/// Aging starts once the conversation's estimate passes one in N of the budget.
pub const AGE_START_SHARE: u64 = 2;

/// The newest tool results, up to one in N of the budget, are never aged.
pub const RECENT_SHARE: u64 = 3;

/// A result no longer than this (characters) is kept whole, however old.
pub const STUB_THRESHOLD_CHARS: usize = 1_200;

/// How much of an aged result's start its stub keeps (characters).
pub const STUB_HEAD_CHARS: usize = 400;

/// Calibration never trusts a ratio above this (a misreport would starve the
/// conversation) and never one below 1 (a prompt cache that reports only the
/// tokens it evaluated would otherwise inflate the budget).
pub const MAX_RATIO: f64 = 2.0;

/// The cap, in characters, on one tool result for a conversation budget of
/// `budget` tokens: [`RESULT_SHARE`] of it at about 4 characters a token, at
/// least [`MIN_RESULT_CHARS`] and never above `ceiling` (the tools' own cap).
pub fn result_cap(budget: u64, ceiling: usize) -> usize {
    let share = usize::try_from(budget.saturating_mul(4) / RESULT_SHARE).unwrap_or(usize::MAX);
    share.max(MIN_RESULT_CHARS).min(ceiling)
}

/// The note left where [`shorten_middle`] cut.
pub fn middle_note(left_out: usize) -> String {
    format!(
        "\n[... {left_out} characters left out of the middle of this result to fit the context \
         window; search_files, or a narrower command (grep, sed -n, head, tail), shows the \
         part you need ...]\n"
    )
}

/// `text` cut to about `max_chars`: two fifths from its start, the rest from its
/// end, and [`middle_note`] between. Unchanged when it already fits.
pub fn shorten_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let head = max_chars * 2 / 5;
    let tail = max_chars - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(count - tail).collect();
    format!("{start}{}{end}", middle_note(count - head - tail))
}

/// The stub an aged result is sent as.
pub fn stub(content: &str) -> String {
    let count = content.chars().count();
    let head: String = content.chars().take(STUB_HEAD_CHARS).collect();
    format!(
        "{head}\n[older result shortened from {count} characters to save context; \
         call the tool again if you need the rest]"
    )
}

/// What [`aged_view`] sends.
#[derive(Debug, Clone, PartialEq)]
pub struct Aged {
    /// The messages, index for index with the input.
    pub messages: Vec<Message>,
    /// How many tool results were sent as stubs.
    pub aged: usize,
}

/// `messages` as a request should carry them under a conversation budget of
/// `budget` tokens: unchanged while their estimate is under [`AGE_START_SHARE`]
/// of it; past that, every tool result longer than [`STUB_THRESHOLD_CHARS`] and
/// older than the newest [`RECENT_SHARE`] of the budget's worth of results is a
/// [`stub`]. The results of the latest step are never aged, whatever their size.
/// Indices are kept, so a pinned index still points at the same message.
pub fn aged_view(messages: &[Message], budget: u64) -> Aged {
    let unchanged = || Aged {
        messages: messages.to_vec(),
        aged: 0,
    };
    if count_tokens(messages) <= budget / AGE_START_SHARE {
        return unchanged();
    }
    // The latest step's results: the tool messages after the last assistant one.
    let latest = messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)
        .map_or(0, |index| index + 1);
    let recent = budget / RECENT_SHARE;
    let mut seen = 0;
    let mut out = messages.to_vec();
    let mut aged = 0;
    for index in (0..messages.len()).rev() {
        let message = &messages[index];
        if message.role != Role::Tool {
            continue;
        }
        seen += count_tokens(std::slice::from_ref(message));
        if index >= latest || seen <= recent {
            continue;
        }
        if message.content.chars().count() > STUB_THRESHOLD_CHARS {
            out[index].content = stub(&message.content);
            aged += 1;
        }
    }
    if aged == 0 {
        return unchanged();
    }
    Aged {
        messages: out,
        aged,
    }
}

/// Reported against estimated input tokens, per model (see the module docs).
#[derive(Debug, Default)]
pub struct Calibration {
    ratios: Mutex<HashMap<String, f64>>,
}

impl Calibration {
    /// Fold in one response: `estimated` tokens sent (the estimate of the whole
    /// request), `reported` by the provider. A response reporting nothing is
    /// ignored. The ratio is smoothed (half the old, half the new) and clamped to
    /// 1 ..= [`MAX_RATIO`].
    pub fn observe(&self, model: &str, estimated: u64, reported: u64) {
        if estimated == 0 || reported == 0 {
            return;
        }
        let sample = (reported as f64 / estimated as f64).clamp(1.0, MAX_RATIO);
        if let Ok(mut ratios) = self.ratios.lock() {
            let ratio = ratios.entry(model.to_owned()).or_insert(sample);
            *ratio = (*ratio + sample) / 2.0;
        }
    }

    /// The model's ratio; 1 until a response has been seen.
    pub fn ratio(&self, model: &str) -> f64 {
        self.ratios
            .lock()
            .ok()
            .and_then(|ratios| ratios.get(model).copied())
            .unwrap_or(1.0)
    }
}

/// `budget` (in estimated tokens) for a model whose real count runs `ratio`
/// times the estimate.
pub fn calibrated(budget: u64, ratio: f64) -> u64 {
    if ratio <= 1.0 {
        return budget;
    }
    (budget as f64 / ratio) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn the_result_cap_follows_the_budget_between_its_floor_and_ceiling() {
        assert_eq!(result_cap(16_500, 100_000), 22_000);
        assert_eq!(result_cap(1_024, 100_000), MIN_RESULT_CHARS);
        assert_eq!(result_cap(115_000, 100_000), 100_000);
        // The tools' own cap wins over the floor.
        assert_eq!(result_cap(1_024, 5_000), 5_000);
    }

    #[test]
    fn shortening_keeps_the_start_and_more_of_the_end() {
        let text = format!(
            "{}{}{}",
            "a".repeat(100),
            "m".repeat(1_000),
            "z".repeat(100)
        );
        let short = shorten_middle(&text, 100);
        assert!(short.starts_with(&"a".repeat(40)));
        assert!(short.ends_with(&"z".repeat(60)));
        assert!(short.contains("[... 1100 characters left out of the middle"));
        assert!(!short.contains("mmm"));
        assert_eq!(shorten_middle("short", 100), "short");
    }

    #[test]
    fn old_large_results_age_once_the_budget_is_half_spent() {
        let big = "x".repeat(4_000); // 1,000 tokens
        let messages = vec![
            message(Role::User, "goal"),
            message(Role::Assistant, ""),
            message(Role::Tool, &big),
            message(Role::Tool, "small"),
            message(Role::Assistant, ""),
            message(Role::Tool, &big),
            message(Role::Assistant, ""),
            message(Role::Tool, &big),
        ];
        // About 3,000 tokens, over half of a 5,000-token budget. The newest third
        // of it (1,666 tokens) holds only the latest result, so both older large
        // results age and the small one stays.
        let aged = aged_view(&messages, 5_000);
        assert_eq!(aged.aged, 2);
        assert!(aged.messages[2]
            .content
            .contains("older result shortened from 4000"));
        assert_eq!(aged.messages[3].content, "small");
        assert!(aged.messages[5].content.starts_with(&"x".repeat(400)));
        assert_eq!(aged.messages[7].content, big);
        assert_eq!(aged.messages.len(), messages.len());

        // With a recent share that reaches the one before, only the oldest ages.
        assert_eq!(aged_view(&messages, 6_000).aged, 1);
        // Under half the budget, nothing changes.
        assert_eq!(aged_view(&messages, 7_000).aged, 0);
    }

    #[test]
    fn the_latest_steps_results_are_never_aged() {
        let big = "x".repeat(8_000);
        let messages = vec![
            message(Role::User, "goal"),
            message(Role::Assistant, ""),
            message(Role::Tool, &big),
            message(Role::Tool, &big),
        ];
        assert_eq!(aged_view(&messages, 1_500).aged, 0);
    }

    #[test]
    fn calibration_only_ever_shrinks_the_budget() {
        let calibration = Calibration::default();
        assert_eq!(calibration.ratio("m"), 1.0);
        calibration.observe("m", 1_000, 1_400);
        assert!((calibration.ratio("m") - 1.4).abs() < 1e-9);
        calibration.observe("m", 1_000, 1_000);
        assert!((calibration.ratio("m") - 1.2).abs() < 1e-9);
        // A cache that reports only what it evaluated never raises the budget.
        calibration.observe("other", 10_000, 200);
        assert_eq!(calibration.ratio("other"), 1.0);
        calibration.observe("wild", 100, 10_000);
        assert_eq!(calibration.ratio("wild"), MAX_RATIO);
        assert_eq!(calibrated(12_000, 1.2), 10_000);
        assert_eq!(calibrated(12_000, 1.0), 12_000);
    }
}
