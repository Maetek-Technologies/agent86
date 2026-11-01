//! Untrusted text made visible, not obeyed, at a terminal (beyond Python).
//!
//! The approval prompt and the project-trust question print text a model or a
//! repository wrote, and the user decides on what they see. A carriage return,
//! an escape sequence, or a bidirectional override could make that differ from
//! what runs (`echo hi\r` drawn over `rm -rf ~`), so each such character is shown
//! as a symbol instead of being sent to the terminal. Dropping them, as the
//! interface's `sanitize` does for drawing, would hide part of a command from the
//! person approving it.

/// `text` with every control or invisible formatting character shown as a visible
/// symbol: C0 controls as their Control Pictures (`␍`, `␛`), DEL as `␡`, and C1
/// controls, bidirectional controls, and zero-width characters as `<U+XXXX>`.
/// Newlines and tabs are kept.
pub fn show_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            '\u{0}'..='\u{1f}' => {
                out.push(char::from_u32(0x2400 + ch as u32).expect("Control Pictures"))
            }
            '\u{7f}' => out.push('\u{2421}'),
            c if hidden(c) => out.push_str(&format!("<U+{:04X}>", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// C1 controls, and the formatting characters that reorder or hide text.
fn hidden(ch: char) -> bool {
    matches!(
        ch,
        '\u{80}'..='\u{9f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_carriage_return_cannot_draw_one_command_over_another() {
        assert_eq!(show_controls("rm -rf x\recho hi"), "rm -rf x␍echo hi");
    }

    #[test]
    fn escape_sequences_and_c1_controls_are_shown_not_sent() {
        assert_eq!(show_controls("a\x1b[2Kb"), "a␛[2Kb");
        assert_eq!(show_controls("a\u{9b}2Kb"), "a<U+009B>2Kb");
        assert_eq!(show_controls("\u{7f}\u{0}"), "␡␀");
    }

    #[test]
    fn bidirectional_and_zero_width_characters_are_named() {
        assert_eq!(
            show_controls("ok\u{202e}txt.exe\u{200b}"),
            "ok<U+202E>txt.exe<U+200B>"
        );
    }

    #[test]
    fn newlines_tabs_and_ordinary_text_are_kept() {
        let text = "line one\n\tcafé ✓ 日本\n";
        assert_eq!(show_controls(text), text);
    }
}
