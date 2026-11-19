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

/// Per-message cap when rendering the transcript for the summarizer, so one huge
/// tool observation cannot be the whole summarization request.
const TRANSCRIPT_MESSAGE_CHARS: usize = 2_000;

/// The user-visible line for a successful compaction, as Python's
/// `compacted_notice`.
pub fn compacted_notice(count: usize) -> String {
    format!("\n[compacted {count} messages into a summary]\n")
}

/// The line when summarizing failed and the oldest messages were dropped.
pub fn compaction_failed_notice(count: usize) -> String {
    format!("\n[compaction failed; dropped {count} messages]\n")
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
            lines.push(format!(
                "{label}: {}",
                clip(&message.content, TRANSCRIPT_MESSAGE_CHARS)
            ));
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
