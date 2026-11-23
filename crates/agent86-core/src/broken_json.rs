//! Tool-call arguments that are not valid JSON (beyond Python; ADR 0006,
//! "`render_ui`, hardened alongside"): where they broke, and the one repair
//! agent86 makes.
//!
//! Python's observation for such a call quotes the arguments' first 200
//! characters. A model writing a large payload usually breaks it thousands of
//! characters in, in its deepest nesting, so that quote shows nothing wrong
//! and the model guesses. [`describe`] says where the JSON broke, which
//! containers were open there, and what was expected, so the model can fix the
//! right place.
//!
//! [`drop_unmatched_closers`] is the repair: a `}` or `]` that cannot close the
//! container it meets (a `}` inside an array, or any closer with nothing left
//! open) is dropped. Models writing deep JSON add one more closer than they
//! need far more often than they leave one out (ADR 0006's session: ten of
//! twelve calls), and a closer that matches nothing has only one reading.
//! Nothing else is ever changed.

use serde_json::{Map, Value};

/// Where `raw` breaks, for the model: a few lines to follow the invalid-JSON
/// observation. `None` when `raw` parses.
pub fn describe(raw: &str) -> Option<String> {
    let error = serde_json::from_str::<Value>(raw).err()?;
    // At the end, serde_json points at the last character; the text is whole
    // up to there, so the break is after it.
    let at = if error.is_eof() {
        raw.len()
    } else {
        byte_offset(raw, error.line(), error.column())
    };
    let before = scan(&raw[..at]);
    let found = raw[at..].chars().next();
    let total = raw.chars().count();
    let position = raw[..at].chars().count() + 1;
    let expected = without_position(&error.to_string());

    let mut lines = Vec::new();
    let place = before.place();
    match found {
        Some(ch) => lines.push(format!(
            "The JSON breaks at character {position} of {total}, {place}: {expected}, found `{ch}`."
        )),
        None => lines.push(format!(
            "The JSON ends at character {total} {place}, before it is complete: {expected}."
        )),
    }
    if let Some(ch @ ('}' | ']')) = found {
        lines.push(match before.stack.last() {
            Some(open) if open.closer() != ch => format!(
                "That `{ch}` closes nothing: the innermost open container is {}, which closes \
                 with `{}`. Remove the extra `{ch}`, or add what is missing before it.",
                open.kind(),
                open.closer()
            ),
            None => format!(
                "That `{ch}` closes nothing: every container was already closed. Remove it."
            ),
            _ => String::new(),
        });
    }
    let start = raw[..at]
        .char_indices()
        .rev()
        .nth(EXCERPT_CHARS - 1)
        .map_or(0, |(index, _)| index);
    let end = found.map_or(at, |ch| at + ch.len_utf8());
    let prefix = if start > 0 { "..." } else { "" };
    lines.push(format!(
        "The text up to the break: {}",
        crate::types::python_repr(&format!("{prefix}{}", &raw[start..end]))
    ));
    let whole = scan(raw);
    lines.push(format!(
        "Outside strings the arguments hold {} `{{` and {} `}}`, {} `[` and {} `]`; every \
         container must close, innermost first.",
        whole.counts[0], whole.counts[1], whole.counts[2], whole.counts[3]
    ));
    lines.retain(|line| !line.is_empty());
    Some(lines.join("\n"))
}

/// `raw` with every closer that matches nothing dropped, when that and only
/// that makes it a JSON object: the object and the (1-based) character
/// positions dropped. `None` when `raw` already parses, when nothing would be
/// dropped, or when the result still does not parse as an object.
pub fn drop_unmatched_closers(raw: &str) -> Option<(Map<String, Value>, Vec<usize>)> {
    if serde_json::from_str::<Value>(raw).is_ok() {
        return None;
    }
    let mut kept = String::with_capacity(raw.len());
    let mut dropped = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    let mut strings = Strings::default();
    for (position, ch) in raw.chars().enumerate() {
        if strings.step(ch) {
            kept.push(ch);
            continue;
        }
        match ch {
            '{' => stack.push(Open::Object(None)),
            '[' => stack.push(Open::Array(0)),
            '}' | ']' => {
                if stack.last().is_some_and(|open| open.closer() == ch) {
                    stack.pop();
                } else {
                    dropped.push(position + 1);
                    continue;
                }
            }
            _ => {}
        }
        kept.push(ch);
    }
    if dropped.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(&kept) {
        Ok(Value::Object(arguments)) => Some((arguments, dropped)),
        _ => None,
    }
}

/// How much of the text before the break the description quotes.
const EXCERPT_CHARS: usize = 80;

/// The byte offset of serde_json's (1-based) line and column, which count
/// bytes; clamped to the text and to a character boundary.
fn byte_offset(raw: &str, line: usize, column: usize) -> usize {
    let line_start = raw
        .match_indices('\n')
        .nth(line.saturating_sub(2))
        .filter(|_| line > 1)
        .map_or(0, |(index, _)| index + 1);
    let mut at = (line_start + column.saturating_sub(1)).min(raw.len());
    while !raw.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// serde_json's message without its " at line L column C".
fn without_position(message: &str) -> String {
    match message.rfind(" at line ") {
        Some(index) => message[..index].to_owned(),
        None => message.to_owned(),
    }
}

/// An open container, and where the scan is inside it.
#[derive(Debug, Clone)]
enum Open {
    /// An object, with the key whose value is being written, once one is.
    Object(Option<String>),
    /// An array, with the index of the item being written.
    Array(usize),
}

impl Open {
    fn closer(&self) -> char {
        match self {
            Open::Object(_) => '}',
            Open::Array(_) => ']',
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Open::Object(_) => "an object",
            Open::Array(_) => "an array",
        }
    }
}

/// Whether the scan is inside a string, and whether the last character escaped
/// the next.
#[derive(Default)]
struct Strings {
    inside: bool,
    escaped: bool,
}

impl Strings {
    /// Feed `ch`; true when it belongs to a string (its quotes included).
    fn step(&mut self, ch: char) -> bool {
        if self.inside {
            if self.escaped {
                self.escaped = false;
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == '"' {
                self.inside = false;
            }
            return true;
        }
        if ch == '"' {
            self.inside = true;
            return true;
        }
        false
    }
}

/// What a scan of a prefix found: the containers still open, and the counts of
/// `{`, `}`, `[`, `]` outside strings.
struct Scan {
    stack: Vec<Open>,
    counts: [usize; 4],
}

impl Scan {
    /// Where the scan is, in words: inside the innermost open container, named
    /// by its path (`messages[0].createSurface`), or at the top level, outside
    /// every container.
    fn place(&self) -> String {
        let Some((innermost, outer)) = self.stack.split_last() else {
            return "at the top level".to_owned();
        };
        if outer.is_empty() {
            let noun = match innermost {
                Open::Object(_) => "object",
                Open::Array(_) => "array",
            };
            return format!("inside the top-level {noun}");
        }
        let mut path = String::new();
        for open in outer {
            match open {
                Open::Object(Some(key)) if is_identifier(key) => {
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(key);
                }
                Open::Object(Some(key)) => path.push_str(&format!("[{key:?}]")),
                Open::Object(None) => path.push_str(".?"),
                Open::Array(index) => path.push_str(&format!("[{index}]")),
            }
        }
        format!("inside `{path}`")
    }
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == '@')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Scan `text` for its open containers, the keys and indices being written in
/// them, and its bracket counts. A closer that matches nothing is counted but
/// does not pop, so the stack stays what the writer meant at that point.
fn scan(text: &str) -> Scan {
    let mut stack: Vec<Open> = Vec::new();
    let mut counts = [0; 4];
    let mut strings = Strings::default();
    // The string being read, when it may be an object's key.
    let mut key: Option<String> = None;
    // Whether the object on top is waiting for a key (after `{` or `,`).
    let mut wants_key = false;
    for ch in text.chars() {
        let was_inside = strings.inside;
        if strings.step(ch) {
            if wants_key {
                match (was_inside, strings.inside) {
                    (false, true) => key = Some(String::new()),
                    (true, true) => {
                        if let Some(key) = key.as_mut() {
                            key.push(ch);
                        }
                    }
                    (true, false) => {
                        if let (Some(Open::Object(slot)), Some(key)) =
                            (stack.last_mut(), key.take())
                        {
                            // Escapes are kept as written; a path only names.
                            *slot = Some(key);
                        }
                        wants_key = false;
                    }
                    (false, false) => {}
                }
            }
            continue;
        }
        match ch {
            '{' => {
                counts[0] += 1;
                stack.push(Open::Object(None));
                wants_key = true;
            }
            '}' => {
                counts[1] += 1;
                if matches!(stack.last(), Some(Open::Object(_))) {
                    stack.pop();
                }
                wants_key = false;
            }
            '[' => {
                counts[2] += 1;
                stack.push(Open::Array(0));
                wants_key = false;
            }
            ']' => {
                counts[3] += 1;
                if matches!(stack.last(), Some(Open::Array(_))) {
                    stack.pop();
                }
                wants_key = false;
            }
            ',' => match stack.last_mut() {
                Some(Open::Array(index)) => *index += 1,
                Some(Open::Object(slot)) => {
                    *slot = None;
                    wants_key = true;
                }
                None => {}
            },
            _ => {}
        }
    }
    Scan { stack, counts }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_json_needs_no_description_or_repair() {
        assert_eq!(describe(r#"{"path": "a"}"#), None);
        assert_eq!(drop_unmatched_closers(r#"{"path": "a"}"#), None);
    }

    #[test]
    fn an_extra_closer_in_an_array_is_described_where_it_is() {
        // The shape of ADR 0006's session: one `}` too many after the deepest
        // object, inside the components array.
        let raw = r#"{"messages": [{"createSurface": {"components": [{"id": "a", "action": {"event": {"name": "go"}}}}], "dataModel": {}}}]}"#;
        let text = describe(raw).unwrap();
        let first = text.lines().next().unwrap();
        let extra = raw.find("}}}}]").unwrap() + 3;
        assert!(
            first.starts_with(&format!(
                "The JSON breaks at character {} of {}, ",
                extra + 1,
                raw.len()
            )),
            "{first}"
        );
        assert!(
            first.contains("inside `messages[0].createSurface.components`"),
            "{first}"
        );
        assert!(
            first.ends_with("expected `,` or `]`, found `}`."),
            "{first}"
        );
        assert!(
            text.contains("That `}` closes nothing: the innermost open container is an array, which closes with `]`."),
            "{text}"
        );
        assert!(
            text.contains(r#"The text up to the break: '...createSurface": "#),
            "{text}"
        );
        assert!(text.contains(r#"{"name": "go"}}}}'"#), "{text}");
        assert!(
            text.contains("hold 7 `{` and 8 `}`, 2 `[` and 2 `]`"),
            "{text}"
        );
    }

    #[test]
    fn the_repair_drops_only_the_closer_that_matches_nothing() {
        let raw = r#"{"messages": [{"createSurface": {"components": [{"id": "a", "action": {"event": {"name": "go"}}}}], "dataModel": {}}}]}"#;
        let (arguments, dropped) = drop_unmatched_closers(raw).unwrap();
        assert_eq!(dropped, vec![raw.find("}}}}]").unwrap() + 3 + 1]);
        assert_eq!(
            Value::Object(arguments),
            json!({"messages": [{"createSurface": {
                "components": [{"id": "a", "action": {"event": {"name": "go"}}}],
                "dataModel": {}
            }}]})
        );
    }

    #[test]
    fn closers_inside_strings_are_left_alone() {
        let raw = r#"{"text": "a } and a ] and a \" quote"}}"#;
        let (arguments, dropped) = drop_unmatched_closers(raw).unwrap();
        assert_eq!(arguments["text"], json!("a } and a ] and a \" quote"));
        assert_eq!(dropped, vec![raw.chars().count()]);
        let text = describe(raw).unwrap();
        assert!(
            text.contains("That `}` closes nothing: every container was already closed."),
            "{text}"
        );
        assert!(
            text.contains("hold 1 `{` and 2 `}`, 0 `[` and 0 `]`"),
            "{text}"
        );
    }

    #[test]
    fn a_closer_that_closes_the_wrong_level_is_not_guessed_at() {
        // One `}` too many closes the top-level object early: every closer up
        // to there matches something, so there is nothing safe to drop.
        let raw = r#"{"messages": [{"a": 1}], "x": {"b": 2}}, "wait": true}"#;
        assert_eq!(drop_unmatched_closers(raw), None);
        let text = describe(raw).unwrap();
        assert!(
            text.contains("at the top level: trailing characters, found `,`."),
            "{text}"
        );
    }

    #[test]
    fn missing_structure_is_not_repaired() {
        assert_eq!(drop_unmatched_closers(r#"{"path": "#), None);
        assert_eq!(drop_unmatched_closers(r#"{"a": 1 "b": 2}"#), None);
        // A closer dropped, yet what is left is not an object.
        assert_eq!(drop_unmatched_closers("[1]]"), None);
    }

    #[test]
    fn truncated_json_says_where_it_ended() {
        let text = describe(r#"{"messages": [{"version": "1.0""#).unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("The JSON ends at character 31 inside `messages[0]`, before it is complete: EOF while parsing an object"),
            "{first}"
        );
    }

    #[test]
    fn a_long_payload_is_quoted_from_just_before_the_break() {
        let filler = "x".repeat(500);
        let raw = format!(r#"{{"a": "{filler}", "b": [1, 2}}]}}"#);
        let text = describe(&raw).unwrap();
        let quote = text
            .lines()
            .find(|line| line.starts_with("The text up to the break: "))
            .unwrap();
        assert!(quote.contains("'...xxx"), "{quote}");
        assert!(quote.ends_with(r#"", "b": [1, 2}'"#), "{quote}");
        assert!(quote.chars().count() < 140, "{quote}");
    }

    #[test]
    fn keys_that_are_not_identifiers_are_quoted_in_the_path() {
        let text = describe(r#"{"my key": [{"a": 1}}]}"#).unwrap();
        assert!(text.contains(r#"inside `["my key"]`"#), "{text}");
    }

    #[test]
    fn multibyte_text_before_the_break_is_counted_in_characters() {
        let raw = r#"{"t": "héllo — ✓", "b": [1}]}"#;
        let text = describe(raw).unwrap();
        let expected = raw
            .find("}]")
            .map(|at| raw[..at].chars().count() + 1)
            .unwrap();
        assert!(
            text.contains(&format!("breaks at character {expected} of ")),
            "{text}"
        );
    }
}
