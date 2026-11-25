//! Working memory: context-window management, ported from Python's
//! `memory/working.py`.
//!
//! Keeps the conversation inside a token budget derived from the model's real
//! context window rather than a flat number: the window, minus what the prompt
//! already spends (system prompt and tool schemas), minus the headroom the
//! response needs. What happens when the budget is exceeded is the caller's
//! choice: [`fit`] drops the oldest span, and [`compaction_cut`] picks the prefix
//! the loop replaces with a summary instead (`[limits] compaction`).
//!
//! Trimming never leaves an orphan tool result at the head of the window, which
//! would break providers that require a preceding tool use.

use crate::pyjson::python_json_dumps;
use crate::types::{Message, Role, ToolSpec};
use serde_json::{json, Value};

/// Never hand the model less conversation than this, whatever the arithmetic
/// says. A budget that collapses to about zero (a small window, a large tool
/// catalogue) would trim every turn to a single message and make the agent
/// amnesiac; an honest context error from the provider is more useful.
pub const MIN_CONVERSATION_TOKENS: u64 = 1_024;

/// Messages at the tail that compaction never touches: the current user turn
/// plus enough recent context that the model does not lose its thread.
pub const KEEP_RECENT: usize = 6;

/// Prefixed to the replacement message, so a human reading a resumed session
/// (and the model reading its own history) can see the span was compacted, not
/// forgotten.
pub const SUMMARY_HEADER: &str = "[Conversation summary \u{2014} earlier turns compacted]";

/// Rough ceiling for the summary itself.
pub const SUMMARY_MAX_TOKENS: u32 = 600;

/// The summarizer's instructions, written for a cheap model: concrete,
/// sectioned, and explicit that identifiers are copied, never paraphrased.
pub const SUMMARY_SYSTEM_PROMPT: &str = "You are compacting the earlier part of an agent's conversation so it fits in a context window. Write a dense factual summary of the transcript below, under these headings:\nGOAL: what the user is ultimately trying to achieve.\nDECISIONS: choices already made and why, including anything ruled out.\nFACTS: what was discovered \u{2014} file paths, identifiers, commands, versions, numbers, error messages.\nOPEN: what is still unfinished or unverified.\n\nRules: reproduce every file path, identifier, command, URL and number EXACTLY as it appears \u{2014} never paraphrase, abbreviate or 'correct' one. Record only what the transcript says; invent nothing. Omit pleasantries and narration. Be terse: aim for under 600 tokens. Output the summary only, with no preamble.";

/// Beyond Python: the header of a summary written *inside* a turn, appended to
/// the turn's own message, which stays verbatim above it. It covers the earlier
/// turns and this task's earlier steps alike.
pub const PROGRESS_HEADER: &str = "[Progress so far \u{2014} earlier messages compacted]";

/// Per-message cap when rendering the transcript for the summarizer, so one huge
/// tool observation cannot be the whole summarization request.
const TRANSCRIPT_MESSAGE_CHARS: usize = 2_000;

/// Beyond Python: the extra room a message holding an earlier summary gets in the
/// transcript, so a second compaction reads the first summary whole (it may run to
/// `SUMMARY_MAX_TOKENS * 2` tokens, about 4,800 characters) instead of losing its
/// end, which is where `OPEN` is.
const SUMMARY_ALLOWANCE_CHARS: usize = 6_000;

/// Beyond Python: what the summarizer is told when the span holds an earlier
/// summary, so facts carried from compaction to compaction are not worn away.
pub const STACKING_NOTE: &str = "The transcript contains an earlier summary. Carry every goal, decision, fact, and open item from it into the new summary, updated by what came after it.";

/// Beyond Python: the second try when a summary comes back empty (a thinking
/// model can spend the whole first cap reasoning).
pub const SUMMARY_RETRY_NOTE: &str = "Write the summary now, directly, without deliberating.";

/// The user-visible line for a successful compaction, as Python's
/// `compacted_notice`.
pub fn compacted_notice(count: usize) -> String {
    format!("\n[compacted {count} messages into a summary]\n")
}

/// The line when summarizing failed and the oldest messages were dropped.
pub fn compaction_failed_notice(count: usize) -> String {
    format!("\n[compaction failed; dropped {count} messages]\n")
}

/// Beyond Python: the line when fitting a request left messages out of it (Python
/// trims silently).
pub fn trimmed_notice(count: usize) -> String {
    format!("\n[context full; {count} earlier messages left out of this request]\n")
}

/// Python's `len` of a string: code points.
fn chars(text: &str) -> u64 {
    text.chars().count() as u64
}

/// Rough token estimate (about 4 characters a token), as Python's
/// `ModelProvider.count_tokens`, which no provider overrides. Tool names, result
/// names, and tool-call arguments count, because they are on the wire.
pub fn count_tokens(messages: &[Message]) -> u64 {
    let mut total = 0;
    for message in messages {
        total += chars(&message.content);
        total += message.name.as_deref().map_or(0, chars);
        for call in &message.tool_calls {
            total += chars(&call.name);
            total += chars(&python_json_dumps(
                &Value::Object(call.arguments.clone()),
                false,
            ));
        }
    }
    total / 4
}

/// Rough token cost of the tool catalogue as it goes on the wire, as Python's
/// `count_spec_tokens`.
pub fn count_spec_tokens(specs: &[ToolSpec]) -> u64 {
    if specs.is_empty() {
        return 0;
    }
    let payload: Vec<Value> = specs
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.description,
                "parameters": spec.parameters,
            })
        })
        .collect();
    chars(&python_json_dumps(&Value::Array(payload), false)) / 4
}

/// Tokens of conversation that fit, given everything else the request spends,
/// as Python's `conversation_budget`: the window minus the prompt overhead,
/// minus the reserve and output cap (together clamped to half the window, so a
/// small local model is not starved), floored at [`MIN_CONVERSATION_TOKENS`],
/// and only then clamped by `hard_cap` (`[limits] max_context_tokens`; `0` is no
/// cap). The floor comes before the cap so it never overrules a cap the user set.
pub fn conversation_budget(
    window: u64,
    overhead_tokens: u64,
    reserve_tokens: u64,
    output_tokens: u64,
    hard_cap: u64,
) -> u64 {
    // `int(window * 0.5)` truncates.
    let reserve = (reserve_tokens + output_tokens).min(window / 2);
    let budget = window
        .saturating_sub(overhead_tokens)
        .saturating_sub(reserve)
        .max(MIN_CONVERSATION_TOKENS);
    if hard_cap > 0 {
        budget.min(hard_cap)
    } else {
        budget
    }
}

/// True when `messages` already fit and nothing needs to be dropped.
pub fn fits(messages: &[Message], budget: u64) -> bool {
    messages.is_empty() || count_tokens(messages) <= budget
}

/// The largest recent suffix of `messages` that fits the budget, without a
/// leading tool result, as Python's `WorkingMemory.fit`. The newest message is
/// always kept, even when it alone is over budget.
pub fn fit(messages: &[Message], budget: u64) -> &[Message] {
    if fits(messages, budget) {
        return messages;
    }
    let mut start = messages.len();
    while start > 0 {
        let trial = &messages[start - 1..];
        if start < messages.len() && count_tokens(trial) > budget {
            break;
        }
        start -= 1;
    }
    // Drop any leading tool-result orphans left by the cut.
    while messages.len() - start > 1 && messages[start].role == Role::Tool {
        start += 1;
    }
    &messages[start..]
}

/// What [`fit_pinned`] sends: the messages, and how many were left out.
#[derive(Debug, Clone, PartialEq)]
pub struct Fitted {
    pub messages: Vec<Message>,
    pub dropped: usize,
    /// Whether the pinned message was kept only because it was pinned.
    pub pinned: bool,
}

/// Beyond Python: [`fit`], except that the message at `pinned` (the turn's own
/// user message, or a sub-agent's task) is never the part left out. When the
/// suffix that fits starts after it, it is sent first and the newest messages
/// that fit beside it follow, so a long turn never loses its goal. Python's `fit`
/// drops it first, silently.
///
/// What follows the pinned message never opens with a tool result: the call that
/// produced it is kept too, even over budget, since an orphan result is a shape
/// providers refuse.
pub fn fit_pinned(messages: &[Message], budget: u64, pinned: Option<usize>) -> Fitted {
    let suffix = fit(messages, budget);
    let start = messages.len() - suffix.len();
    let Some(pin) = pinned.filter(|&pin| pin < start) else {
        return Fitted {
            messages: suffix.to_vec(),
            dropped: start,
            pinned: false,
        };
    };
    let head = &messages[pin];
    let rest = &messages[pin + 1..];
    let mut tail_start = if rest.is_empty() {
        0
    } else {
        let room = budget.saturating_sub(count_tokens(std::slice::from_ref(head)));
        rest.len() - fit(rest, room).len()
    };
    while tail_start > 0 && rest.get(tail_start).is_some_and(|m| m.role == Role::Tool) {
        tail_start -= 1;
    }
    let mut kept = Vec::with_capacity(rest.len() - tail_start + 1);
    kept.push(head.clone());
    kept.extend(rest[tail_start..].iter().cloned());
    Fitted {
        dropped: messages.len() - kept.len(),
        messages: kept,
        pinned: true,
    }
}

/// Where [`compaction_plan`] cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// Replace `messages[..cut]` with a summary, as Python does; nothing of the
    /// current turn is touched.
    Earlier(usize),
    /// Beyond Python: replace everything before `cut` but the turn's own message
    /// (at `goal`), which is kept verbatim with the summary appended under
    /// [`PROGRESS_HEADER`].
    WithinTurn { goal: usize, cut: usize },
}

impl Cut {
    /// The index of the first message kept as it was.
    pub fn cut(self) -> usize {
        match self {
            Self::Earlier(cut) | Self::WithinTurn { cut, .. } => cut,
        }
    }

    /// How many messages the summary stands in for.
    pub fn replaced(self) -> usize {
        match self {
            Self::Earlier(cut) => cut,
            Self::WithinTurn { cut, .. } => cut - 1,
        }
    }
}

/// Beyond Python: where to compact, reaching inside the current turn when that
/// is the only way to make room. Python never touches the current turn
/// ([`compaction_cut`]'s `protect_from`), so a long single turn, which is what a
/// task on a small local model is, never compacts at all and `fit` drops its
/// oldest messages instead, the goal first.
///
/// The last `keep_recent` messages and everything from `protect_from` on (a
/// continuation in progress) are never compacted, and an assistant message with
/// tool calls is never separated from its results. When the cut lands past the
/// turn's message at `goal` with something of the turn's own before it, the cut
/// is [`Cut::WithinTurn`]; otherwise it falls back to Python's cut before `goal`.
pub fn compaction_plan(
    messages: &[Message],
    budget: u64,
    keep_recent: usize,
    goal: usize,
    protect_from: Option<usize>,
) -> Option<Cut> {
    if fits(messages, budget) {
        return None;
    }
    let mut cut = messages.len().saturating_sub(keep_recent);
    if let Some(protect_from) = protect_from {
        cut = cut.min(protect_from);
    }
    while cut > 0 && messages[cut].role == Role::Tool {
        cut -= 1;
    }
    let earlier = compaction_cut(messages, budget, keep_recent, Some(goal.min(cut)));
    if goal < messages.len() && cut > goal + 1 {
        // Python's cut is still preferred when it alone makes room: the turn as it
        // stands, plus a summary of the earlier turns at its ceiling, fits.
        let earlier_suffices = earlier > 0
            && count_tokens(&messages[goal..]) + u64::from(SUMMARY_MAX_TOKENS) * 2 <= budget;
        if !earlier_suffices {
            return Some(Cut::WithinTurn { goal, cut });
        }
    }
    (earlier > 0).then_some(Cut::Earlier(earlier))
}

/// Beyond Python: the history after a [`Cut::WithinTurn`] compaction: the turn's
/// message (`goal`, its text as the user sent it) with the summary under
/// [`PROGRESS_HEADER`], then everything kept.
pub fn apply_progress(goal: &str, summary: &str, rest: Vec<Message>) -> Vec<Message> {
    let mut compacted = Vec::with_capacity(rest.len() + 1);
    compacted.push(Message {
        role: Role::User,
        content: format!("{goal}\n\n{PROGRESS_HEADER}\n{}", python_strip(summary)),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    });
    compacted.extend(rest);
    compacted
}

/// Whether a message carries a summary an earlier compaction wrote.
pub fn holds_summary(message: &Message) -> bool {
    message.content.contains(SUMMARY_HEADER) || message.content.contains(PROGRESS_HEADER)
}

/// Beyond Python: the summarizer's user message for `messages`: the transcript,
/// after [`STACKING_NOTE`] when the span holds an earlier summary, cut to
/// `max_chars` by [`fit_transcript`].
pub fn summary_request_text(messages: &[Message], max_chars: usize) -> String {
    let transcript = render_transcript(messages);
    let text = if messages.iter().any(holds_summary) {
        format!("{STACKING_NOTE}\n\n{transcript}")
    } else {
        transcript
    };
    fit_transcript(&text, max_chars)
}

/// Beyond Python: a transcript cut to `max_chars` so the summary request fits
/// the summarizer's own window. The start (the goal, an earlier summary) and the
/// end (the latest state) matter most, so a third of the room goes to the start,
/// the rest to the end, with how much was left out between them.
pub fn fit_transcript(text: &str, max_chars: usize) -> String {
    let length = text.chars().count();
    if length <= max_chars {
        return text.to_owned();
    }
    let head_chars = max_chars / 3;
    let tail_chars = max_chars - head_chars;
    let head: String = text.chars().take(head_chars).collect();
    let tail: String = text.chars().skip(length - tail_chars).collect();
    format!(
        "{head}\n... [{} chars of transcript left out] ...\n{tail}",
        length - head_chars - tail_chars
    )
}

/// Where to split `messages` into a compactable prefix and a kept suffix, as
/// Python's `WorkingMemory.compaction_cut`; `0` when nothing may be compacted.
///
/// The last `keep_recent` messages are never compacted, nor is anything from
/// `protect_from` on (the current user turn), and an assistant message with
/// tool calls is never separated from its results: a cut that lands on a tool
/// result walks back until the first kept message opens a block of its own.
pub fn compaction_cut(
    messages: &[Message],
    budget: u64,
    keep_recent: usize,
    protect_from: Option<usize>,
) -> usize {
    if fits(messages, budget) {
        return 0;
    }
    let mut cut = messages.len().saturating_sub(keep_recent);
    if let Some(protect_from) = protect_from {
        cut = cut.min(protect_from);
    }
    while cut > 0 && messages[cut].role == Role::Tool {
        cut -= 1;
    }
    cut
}

fn clip(text: &str, limit: usize) -> String {
    let length = text.chars().count();
    if length <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head} ... [{} more chars]", length - limit)
}

fn role_label(role: Role) -> String {
    serde_json::to_value(role)
        .ok()
        .and_then(|value| value.as_str().map(str::to_uppercase))
        .unwrap_or_default()
}

/// A span of conversation as plain text for the summarizer, as Python's
/// `render_transcript`. Tool calls and results are rendered explicitly; they are
/// usually where the facts worth keeping are.
pub fn render_transcript(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        let label = role_label(message.role);
        if message.role == Role::Tool {
            let name = message
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or("?");
            lines.push(format!(
                "{label} result of {name}: {}",
                clip(&message.content, TRANSCRIPT_MESSAGE_CHARS)
            ));
            continue;
        }
        if !message.content.is_empty() {
            // Beyond Python: an earlier summary is read whole.
            let limit = if holds_summary(message) {
                TRANSCRIPT_MESSAGE_CHARS + SUMMARY_ALLOWANCE_CHARS
            } else {
                TRANSCRIPT_MESSAGE_CHARS
            };
            lines.push(format!("{label}: {}", clip(&message.content, limit)));
        }
        for call in &message.tool_calls {
            let arguments = python_json_dumps(&Value::Object(call.arguments.clone()), false);
            lines.push(format!(
                "{label} called {}({})",
                call.name,
                clip(&arguments, 600)
            ));
        }
    }
    lines.join("\n")
}

/// The compacted history: the summary, then everything kept, as Python's
/// `apply_summary`. The summary rides on a user message, merged into the first
/// kept message when that is also a user turn, because two user messages in a
/// row is a shape some providers reject.
pub fn apply_summary(summary: &str, rest: Vec<Message>) -> Vec<Message> {
    let content = format!("{SUMMARY_HEADER}\n{}", python_strip(summary));
    let mut rest = rest;
    if let Some(head) = rest.first_mut() {
        if head.role == Role::User && head.tool_calls.is_empty() {
            head.content = format!("{content}\n\n{}", head.content);
            return rest;
        }
    }
    let mut compacted = Vec::with_capacity(rest.len() + 1);
    compacted.push(Message {
        role: Role::User,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    });
    compacted.extend(rest);
    compacted
}

/// Python's `str.strip()`: Unicode whitespace plus the separators `\x1c`-`\x1f`.
pub fn python_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// The messages a compaction replaced, as the JSON Python archives:
/// `json.dumps([m.model_dump(mode="json") ...], ensure_ascii=False)`, every field
/// written, defaults included.
pub fn archive_payload(messages: &[Message]) -> String {
    let dumped: Vec<Value> = messages
        .iter()
        .map(|message| {
            json!({
                "role": message.role,
                "content": message.content,
                "tool_calls": message
                    .tool_calls
                    .iter()
                    .map(|call| json!({"id": call.id, "name": call.name, "arguments": call.arguments}))
                    .collect::<Vec<_>>(),
                "tool_call_id": message.tool_call_id,
                "name": message.name,
            })
        })
        .collect();
    python_json_dumps(&Value::Array(dumped), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    fn call(name: &str, arguments: Value) -> Message {
        Message {
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: name.into(),
                arguments: arguments.as_object().unwrap().clone(),
            }],
            ..msg(Role::Assistant, "")
        }
    }

    fn result(name: &str, content: &str) -> Message {
        Message {
            tool_call_id: Some("c1".into()),
            name: Some(name.into()),
            ..msg(Role::Tool, content)
        }
    }

    #[test]
    fn tokens_count_content_names_and_arguments_in_code_points() {
        // 8 + 0 = 8 chars -> 2
        assert_eq!(count_tokens(&[msg(Role::User, "abcdefgh")]), 2);
        // "é" is one code point: 7 chars -> 1
        assert_eq!(count_tokens(&[msg(Role::User, "\u{e9}abcdef")]), 1);
        // name "read_file" (9) + `{"path": "a"}` (13) = 22 -> 5
        assert_eq!(count_tokens(&[call("read_file", json!({"path": "a"}))]), 5);
        // content 3 + name 9 = 12 -> 3; summed across messages before dividing
        assert_eq!(count_tokens(&[result("read_file", "abc")]), 3);
        assert_eq!(
            count_tokens(&[msg(Role::User, "abc"), msg(Role::User, "abcde")]),
            2
        );
        assert_eq!(count_tokens(&[]), 0);
    }

    #[test]
    fn the_budget_subtracts_overhead_and_a_clamped_reserve() {
        // 200k window, 1k overhead, 4096 reserve + 8192 output.
        assert_eq!(
            conversation_budget(200_000, 1_000, 4_096, 8_192, 0),
            186_712
        );
        // An 8k window: the reserve is clamped to half the window.
        assert_eq!(conversation_budget(8_192, 1_000, 4_096, 8_192, 0), 3_096);
        // Floored at the minimum, then capped by the user's cap.
        assert_eq!(conversation_budget(4_000, 3_900, 4_096, 8_192, 0), 1_024);
        assert_eq!(conversation_budget(4_000, 3_900, 4_096, 8_192, 500), 500);
        assert_eq!(conversation_budget(200_000, 0, 0, 0, 50_000), 50_000);
        // `int(window * 0.5)` truncates an odd window.
        assert_eq!(conversation_budget(8_193, 0, 9_000, 0, 0), 4_097);
    }

    #[test]
    fn fit_keeps_the_newest_suffix_without_a_leading_tool_result() {
        let long = "x".repeat(400); // 100 tokens each
        let messages = vec![
            msg(Role::User, &long),
            call("read_file", json!({})),
            result("read_file", &long),
            msg(Role::Assistant, &long),
        ];
        assert_eq!(fit(&messages, 10_000).len(), 4);
        // The call, its result, and the answer come to exactly 205 tokens.
        let kept = fit(&messages, 205);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].role, Role::Assistant);
        // At 203 the result and the answer fit (202) but the call does not, so the
        // result would lead without its call and is dropped too.
        let kept = fit(&messages, 203);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].role, Role::Assistant);
        assert_eq!(kept[0].content, long);
        // The newest message is kept even alone over budget.
        assert_eq!(fit(&messages, 1).len(), 1);
    }

    #[test]
    fn the_cut_spares_the_recent_tail_the_current_turn_and_tool_blocks() {
        let long = "y".repeat(400);
        let mut messages: Vec<Message> = (0..10).map(|_| msg(Role::User, &long)).collect();
        assert_eq!(compaction_cut(&messages, 100_000, KEEP_RECENT, None), 0);
        assert_eq!(compaction_cut(&messages, 100, KEEP_RECENT, None), 4);
        assert_eq!(compaction_cut(&messages, 100, KEEP_RECENT, Some(2)), 2);
        messages[4] = result("read_file", &long);
        messages[3] = result("read_file", &long);
        messages[2] = call("read_file", json!({}));
        assert_eq!(compaction_cut(&messages, 100, KEEP_RECENT, None), 2);
        assert_eq!(compaction_cut(&messages, 100, 20, None), 0);
    }

    /// A turn: its message, then `steps` tool round trips of `size` characters each.
    fn long_turn(goal: &str, steps: usize, size: usize) -> Vec<Message> {
        let mut messages = vec![msg(Role::User, goal)];
        for _ in 0..steps {
            messages.push(call("read_file", json!({"path": "big.txt"})));
            messages.push(result("read_file", &"a".repeat(size)));
        }
        messages
    }

    #[test]
    fn a_pinned_fit_keeps_the_goal_ahead_of_the_newest_messages() {
        let messages = long_turn("the goal", 4, 400);
        // Unpinned, the goal is the first thing to go.
        assert_eq!(fit(&messages, 250)[0].role, Role::Assistant);
        let fitted = fit_pinned(&messages, 250, Some(0));
        assert!(fitted.pinned);
        assert_eq!(fitted.messages[0].content, "the goal");
        assert_eq!(fitted.messages[1].role, Role::Assistant);
        // Two round trips (218 tokens) fit beside the goal; the two before do not.
        assert_eq!(fitted.messages.len(), 5);
        assert_eq!(fitted.dropped, 4);
        // When everything fits, or the goal is already in the suffix, it is `fit`.
        let all = fit_pinned(&messages, 100_000, Some(0));
        assert_eq!((all.messages.len(), all.dropped, all.pinned), (9, 0, false));
        let later = fit_pinned(&messages, 250, Some(7));
        assert_eq!(later.messages.as_slice(), fit(&messages, 250));
        assert!(!later.pinned);
        // No pin is Python's `fit`.
        assert_eq!(
            fit_pinned(&messages, 250, None).messages.as_slice(),
            fit(&messages, 250)
        );
    }

    #[test]
    fn a_pinned_fit_never_leaves_a_result_without_its_call() {
        // The newest result alone is over budget: `fit` keeps it alone, so the pin
        // brings its call along rather than send an orphan result.
        let messages = long_turn("goal", 2, 4_000);
        let fitted = fit_pinned(&messages, 500, Some(0));
        let roles: Vec<Role> = fitted.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool]);
        assert_eq!(fitted.messages[1].tool_calls[0].name, "read_file");
    }

    #[test]
    fn compaction_reaches_inside_a_long_turn_and_spares_its_goal() {
        // One turn of eight round trips: Python's cut never fires.
        let messages = long_turn("goal", 8, 400);
        assert_eq!(compaction_cut(&messages, 500, KEEP_RECENT, Some(0)), 0);
        let cut = compaction_plan(&messages, 500, KEEP_RECENT, 0, None).unwrap();
        assert_eq!(cut, Cut::WithinTurn { goal: 0, cut: 11 });
        assert_eq!((cut.cut(), cut.replaced()), (11, 10));
        // A continuation in progress is never reached.
        assert_eq!(
            compaction_plan(&messages, 500, KEEP_RECENT, 0, Some(5)),
            Some(Cut::WithinTurn { goal: 0, cut: 5 })
        );
        // Nothing of the turn before the cut but its own message: nothing to do.
        assert_eq!(
            compaction_plan(&messages, 500, KEEP_RECENT, 0, Some(1)),
            None
        );
        assert_eq!(
            compaction_plan(&messages, 100_000, KEEP_RECENT, 0, None),
            None
        );
    }

    #[test]
    fn earlier_turns_are_still_compacted_first_when_that_makes_room() {
        let long = "y".repeat(4_000);
        let mut messages = vec![msg(Role::User, &long), msg(Role::Assistant, &long)];
        messages.extend(long_turn("goal", 4, 40));
        // The turn itself is small: Python's cut before it is enough.
        assert_eq!(
            compaction_plan(&messages, 1_900, KEEP_RECENT, 2, None),
            Some(Cut::Earlier(2))
        );
        // A large turn as well: the cut reaches inside it.
        let mut messages = vec![msg(Role::User, &long), msg(Role::Assistant, &long)];
        messages.extend(long_turn("goal", 4, 2_000));
        assert_eq!(
            compaction_plan(&messages, 1_900, KEEP_RECENT, 2, None),
            Some(Cut::WithinTurn { goal: 2, cut: 5 })
        );
    }

    #[test]
    fn progress_follows_the_goal_verbatim() {
        let compacted = apply_progress("do it", "  FACTS: x \n", vec![msg(Role::Assistant, "ok")]);
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0].role, Role::User);
        assert_eq!(
            compacted[0].content,
            format!("do it\n\n{PROGRESS_HEADER}\nFACTS: x")
        );
        assert!(holds_summary(&compacted[0]));
        assert!(!holds_summary(&compacted[1]));
    }

    #[test]
    fn an_earlier_summary_reaches_the_summarizer_whole() {
        let earlier = format!("{SUMMARY_HEADER}\n{}OPEN: finish", "f".repeat(4_000));
        let text = summary_request_text(&[msg(Role::User, &earlier)], 100_000);
        assert!(text.starts_with(STACKING_NOTE), "{text}");
        assert!(text.ends_with("OPEN: finish"), "{text}");
        // Without one, the request is Python's transcript.
        let plain = [msg(Role::User, "hello")];
        assert_eq!(
            summary_request_text(&plain, 100_000),
            render_transcript(&plain)
        );
        // Other messages keep Python's clip.
        let long = render_transcript(&[msg(Role::User, &"g".repeat(3_000))]);
        assert!(long.ends_with("... [1000 more chars]"), "{long}");
    }

    #[test]
    fn transcripts_are_cut_to_the_summarizers_room_keeping_both_ends() {
        let text = format!("{}{}", "h".repeat(600), "t".repeat(600));
        assert_eq!(fit_transcript(&text, 2_000), text);
        let cut = fit_transcript(&text, 300);
        assert!(cut.starts_with(&"h".repeat(100)), "{cut}");
        assert!(cut.ends_with(&"t".repeat(200)), "{cut}");
        assert!(cut.contains("[900 chars of transcript left out]"), "{cut}");
    }

    #[test]
    fn transcripts_render_roles_calls_and_clipped_results() {
        let big = "z".repeat(2_005);
        let mut nameless = result("x", "out");
        nameless.name = None;
        let text = render_transcript(&[
            msg(Role::User, "find the config"),
            Message {
                content: "looking".into(),
                ..call("read_file", json!({"path": "caf\u{e9}.toml"}))
            },
            result("read_file", &big),
            nameless,
        ]);
        let expected = format!(
            "USER: find the config\nASSISTANT: looking\nASSISTANT called read_file({{\"path\": \"caf\u{e9}.toml\"}})\nTOOL result of read_file: {} ... [5 more chars]\nTOOL result of ?: out",
            "z".repeat(2_000)
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn summaries_merge_into_a_leading_user_turn() {
        let merged = apply_summary("  GOAL: x \n", vec![msg(Role::User, "next")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].content,
            format!("{SUMMARY_HEADER}\nGOAL: x\n\nnext")
        );
        let separate = apply_summary("GOAL: x", vec![msg(Role::Assistant, "ok")]);
        assert_eq!(separate.len(), 2);
        assert_eq!(separate[0].role, Role::User);
        assert_eq!(separate[0].content, format!("{SUMMARY_HEADER}\nGOAL: x"));
        assert_eq!(apply_summary("s", Vec::new()).len(), 1);
    }

    #[test]
    fn archives_dump_every_field_as_pydantic_does() {
        let payload = archive_payload(&[msg(Role::User, "h\u{e9}"), call("t", json!({"a": 1}))]);
        assert_eq!(
            payload,
            "[{\"role\": \"user\", \"content\": \"h\u{e9}\", \"tool_calls\": [], \"tool_call_id\": null, \"name\": null}, \
             {\"role\": \"assistant\", \"content\": \"\", \"tool_calls\": [{\"id\": \"c1\", \"name\": \"t\", \"arguments\": {\"a\": 1}}], \"tool_call_id\": null, \"name\": null}]"
        );
    }
}
