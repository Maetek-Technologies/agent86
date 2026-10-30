//! Cleaning up streamed reasoning, beyond Python (which drops reasoning).
//!
//! Some hosts serve a thinking model with a reasoning parser that does not match
//! its template. The model then writes its own closing tag (`</think>`,
//! `</thinking>`, or something stranger such as `</trans>`) inside the reasoning
//! field. After that it often degenerates into repeating one short piece of text
//! until the output cap, which costs thousands of tokens and minutes of waiting.
//! [`ReasoningFilter`] drops such a tag and notices such a loop, whichever host
//! is serving the model, so the adapter can stop reading.

/// The stop reason a reply ends with when its reasoning went into a loop. It is
/// not one of the provider stop reasons, so nothing mistakes it for a cut-off.
pub const REASONING_LOOP: &str = "reasoning_loop";

/// A line is only held back while it could still be a closing tag, and no tag
/// name the filter recognizes is longer than this.
const MAX_HELD: usize = 64;
const MAX_TAG_NAME: usize = 32;

/// The reasoning counts as looping when its last `LOOP_WINDOW` bytes are one
/// unit of at most `MAX_LOOP_PERIOD` bytes repeated, so at least eight copies.
/// Real reasoning never repeats itself exactly for that long.
const LOOP_WINDOW: usize = 2048;
const MAX_LOOP_PERIOD: usize = 256;
/// The loop check runs after this much new reasoning, not on every token.
const CHECK_EVERY: usize = 128;
/// How much of the raw reasoning is kept for the loop check, at most.
const RAW_KEPT: usize = 4 * LOOP_WINDOW;

/// The reasoning of one attempt, as it streams.
#[derive(Debug, Default)]
pub struct ReasoningFilter {
    /// Everything passed on so far.
    shown: String,
    /// The start of the current line, held while it could be a closing tag.
    held: String,
    /// Part of the current line was already passed on, so it is no tag line.
    mid_line: bool,
    /// The end of the reasoning as received, tags and all: a loop made only of
    /// stray tags is never shown, but it still has to be noticed.
    raw: String,
    unchecked: usize,
    looping: bool,
}

impl ReasoningFilter {
    /// Take the next piece of reasoning and return what can be shown now.
    pub fn push(&mut self, piece: &str) -> String {
        let start = self.shown.len();
        let mut rest = std::mem::take(&mut self.held);
        rest.push_str(piece);
        let mut rest = rest.as_str();
        while let Some(end) = rest.find('\n') {
            let (line, tail) = rest.split_at(end + 1);
            rest = tail;
            if self.mid_line {
                self.mid_line = false;
                self.shown.push_str(line);
            } else if !self.is_stray_tag(line) {
                self.shown.push_str(line);
            }
        }
        if !rest.is_empty() {
            if !self.mid_line && rest.len() <= MAX_HELD && could_be_tag(rest) {
                self.held = rest.to_owned();
            } else {
                self.mid_line = true;
                self.shown.push_str(rest);
            }
        }
        self.raw.push_str(piece);
        self.unchecked += piece.len();
        if self.unchecked >= CHECK_EVERY {
            self.unchecked = 0;
            self.looping = self.looping || loop_start(&self.raw).is_some();
            if self.raw.len() > RAW_KEPT {
                let mut cut = self.raw.len() - LOOP_WINDOW;
                while !self.raw.is_char_boundary(cut) {
                    cut -= 1;
                }
                self.raw.drain(..cut);
            }
        }
        self.shown[start..].to_owned()
    }

    /// The reasoning is over (the answer started, or the reply ended): settle the
    /// line still held and return what of it can be shown.
    pub fn flush(&mut self) -> String {
        let held = std::mem::take(&mut self.held);
        if held.is_empty() || self.is_stray_tag(&held) {
            return String::new();
        }
        self.mid_line = true;
        self.shown.push_str(&held);
        held
    }

    /// Whether the reasoning has turned into one piece repeated over and over.
    pub fn looping(&self) -> bool {
        self.looping
    }

    /// The reasoning to keep: everything shown, without the repeated run once it
    /// looped (if any of it was shown), since that may later stand in for the answer.
    pub fn into_text(self) -> String {
        let mut text = self.shown;
        if self.looping {
            if let Some(start) = loop_start(&text) {
                text.truncate(start);
                text.truncate(text.trim_end().len());
            }
        }
        text
    }

    /// A line holding only a closing tag that nothing earlier opened.
    fn is_stray_tag(&self, line: &str) -> bool {
        let Some(name) = closing_tag(line.trim()) else {
            return false;
        };
        let opener = format!("<{name}");
        !self.shown.match_indices(&opener).any(|(at, _)| {
            matches!(
                self.shown[at + opener.len()..].chars().next(),
                Some('>' | ' ' | '\t' | '\n' | '/') | None
            )
        })
    }
}

/// `</name>` with a plain tag name, and nothing else.
fn closing_tag(text: &str) -> Option<&str> {
    let name = text.strip_prefix("</")?.strip_suffix('>')?;
    is_tag_name(name).then_some(name)
}

fn is_tag_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
        && name.len() <= MAX_TAG_NAME
}

/// Whether a line begun with `text` could still turn out to be a closing tag.
fn could_be_tag(text: &str) -> bool {
    let text = text.trim_start_matches([' ', '\t']);
    let Some(rest) = text.strip_prefix('<') else {
        return text.is_empty();
    };
    let Some(rest) = rest.strip_prefix('/') else {
        return rest.is_empty();
    };
    let (name, after) = match rest.find('>') {
        Some(end) => (&rest[..end], Some(&rest[end + 1..])),
        None => (rest, None),
    };
    let name_ok = name.is_empty() && after.is_none() || is_tag_name(name);
    name_ok && after.is_none_or(|after| after.trim().is_empty())
}

/// Where the repeated run at the end of `text` starts, if the last
/// [`LOOP_WINDOW`] bytes are one short unit repeated.
fn loop_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < LOOP_WINDOW {
        return None;
    }
    let window = len - LOOP_WINDOW;
    let period = (1..=MAX_LOOP_PERIOD).find(|&period| {
        (window..len - period)
            .rev()
            .all(|i| bytes[i] == bytes[i + period])
    })?;
    // Walk back to where the repetition begins, then to a character boundary.
    let mut start = window;
    while start > 0 && bytes[start - 1] == bytes[start - 1 + period] {
        start -= 1;
    }
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Some(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pieces: &[&str]) -> (String, ReasoningFilter) {
        let mut filter = ReasoningFilter::default();
        let mut shown: String = pieces.iter().map(|piece| filter.push(piece)).collect();
        shown.push_str(&filter.flush());
        (shown, filter)
    }

    #[test]
    fn a_stray_closing_tag_on_its_own_line_is_dropped_even_when_split() {
        let (shown, _) = run(&["No tool needed.\n<", "/tra", "ns", ">\n", "Done."]);
        assert_eq!(shown, "No tool needed.\nDone.");
        let (shown, _) = run(&["Answer: 391.\n  </thinking>  \n"]);
        assert_eq!(shown, "Answer: 391.\n");
        // At the very end, with no newline after it.
        let (shown, filter) = run(&["Answer.\n</think>"]);
        assert_eq!(shown, "Answer.\n");
        assert_eq!(filter.into_text(), "Answer.\n");
    }

    #[test]
    fn tags_that_belong_to_the_reasoning_are_kept() {
        // Opened earlier, or not alone on the line, or not a closing tag at all.
        let text = "Wrap it:\n<div class=\"x\">\nhi\n</div>\nThen close with </span>.\n</ not a tag>\n<br>\n";
        let (shown, _) = run(&[text]);
        assert_eq!(shown, text);
        // A line that merely starts like a tag is passed on once it cannot be one.
        let (shown, _) = run(&["<", "/", "b", " is bold\n"]);
        assert_eq!(shown, "</b is bold\n");
        // Ordinary text is never held back.
        let mut filter = ReasoningFilter::default();
        assert_eq!(filter.push("Let me"), "Let me");
        assert_eq!(filter.push(" think <"), " think <");
    }

    #[test]
    fn a_held_line_is_released_when_it_stops_looking_like_a_tag() {
        let mut filter = ReasoningFilter::default();
        assert_eq!(filter.push("ok\n  "), "ok\n");
        assert_eq!(filter.push("</x"), "");
        assert_eq!(filter.push("y> more\n"), "  </xy> more\n");
    }

    #[test]
    fn a_repeated_run_is_a_loop_and_is_cut_from_the_kept_text() {
        let mut filter = ReasoningFilter::default();
        filter.push("17 * 23 = 391.\n</thinking>\n");
        for _ in 0..2000 {
            filter.push("\"\n");
        }
        assert!(filter.looping());
        assert_eq!(filter.into_text(), "17 * 23 = 391.");

        // A longer unit, a sentence repeated.
        let mut filter = ReasoningFilter::default();
        filter.push("Thinking.\n");
        for _ in 0..60 {
            filter.push("Wait, let me reconsider the question once more. ");
        }
        assert!(filter.looping());
        assert_eq!(filter.into_text(), "Thinking.");
    }

    #[test]
    fn a_loop_of_stray_tags_is_noticed_though_none_of_it_is_shown() {
        let mut filter = ReasoningFilter::default();
        let mut shown = filter.push("The answer is 391.\n</parameter>\n</invoke>\n");
        for _ in 0..200 {
            shown.push_str(&filter.push("</function_results>\n"));
        }
        assert_eq!(shown, "The answer is 391.\n");
        assert!(filter.looping());
        assert_eq!(filter.into_text(), "The answer is 391.\n");
    }

    #[test]
    fn varied_reasoning_and_short_runs_are_not_loops() {
        let mut filter = ReasoningFilter::default();
        for step in 0..400 {
            filter.push(&format!("Step {step}: check the next case.\n"));
        }
        filter.push(&"-".repeat(200));
        filter.push("\n| a | b |\n|---|---|\n");
        assert!(!filter.looping());
        // Multibyte text keeps the cut on a character boundary.
        let mut filter = ReasoningFilter::default();
        filter.push("é");
        filter.push(&"×é".repeat(1000));
        assert!(filter.looping());
        assert_eq!(filter.into_text(), "");
    }
}
