//! The interface's startup header: a block-letter `AGENT 86` wordmark in a
//! cyan-to-violet gradient over one line naming the version and the working
//! directory.
//!
//! Beyond Python, whose full-screen app opens with a compact Rich panel (version
//! and model). The plain REPL keeps Python's banner (`repl::banner`). A terminal too
//! narrow for the wordmark gets a one-line title instead, so nothing wraps.

use crate::repl::{Style, Text};
use unicode_width::UnicodeWidthStr;

/// The wordmark's rows, one glyph per letter (ANSI Shadow), each glyph a fixed width.
const GLYPHS: [[&str; 6]; 8] = [
    [
        " █████╗ ",
        "██╔══██╗",
        "███████║",
        "██╔══██║",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
    [
        " ██████╗ ",
        "██╔════╝ ",
        "██║  ███╗",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝ ",
    ],
    [
        "███████╗",
        "██╔════╝",
        "█████╗  ",
        "██╔══╝  ",
        "███████╗",
        "╚══════╝",
    ],
    [
        "███╗   ██╗",
        "████╗  ██║",
        "██╔██╗ ██║",
        "██║╚██╗██║",
        "██║ ╚████║",
        "╚═╝  ╚═══╝",
    ],
    [
        "████████╗",
        "╚══██╔══╝",
        "   ██║   ",
        "   ██║   ",
        "   ██║   ",
        "   ╚═╝   ",
    ],
    ["  ", "  ", "  ", "  ", "  ", "  "],
    [
        " █████╗ ",
        "██╔══██╗",
        "╚█████╔╝",
        "██╔══██╗",
        "╚█████╔╝",
        " ╚════╝ ",
    ],
    [
        " ██████╗ ",
        "██╔════╝ ",
        "███████╗ ",
        "██╔═══██╗",
        "╚██████╔╝",
        " ╚═════╝ ",
    ],
];

/// One 256-colour index per row, top to bottom: cyan through blue to violet.
const GRADIENT: [u8; 6] = [87, 81, 75, 69, 63, 99];

/// The wordmark's rows as plain text.
fn wordmark() -> Vec<String> {
    (0..6)
        .map(|row| GLYPHS.iter().map(|glyph| glyph[row]).collect())
        .collect()
}

/// The header for a terminal `width` cells wide. `cwd` is the directory as the
/// status line shows it; empty leaves it out. The model is left to the status
/// line, which follows `/model`; a header in scrollback cannot.
pub fn header(version: &str, cwd: &str, width: usize) -> Text {
    let rows = wordmark();
    let mark_width = rows[0].width() + 1;
    let mut info = format!("v{version}");
    if !cwd.is_empty() {
        info.push_str(&format!(" · {cwd}"));
    }
    let mut text = Text::default();
    if width >= mark_width {
        for (row, color) in rows.into_iter().zip(GRADIENT) {
            text.push(Style::BoldColor(color), format!(" {}", row.trim_end()));
            text.push(Style::Plain, "\n");
        }
        text.push(Style::Dim, format!("  {info}"));
    } else {
        text.push(Style::BoldColor(GRADIENT[0]), "agent86");
        text.push(Style::Dim, format!(" {info}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_row_has_its_glyphs_width() {
        for glyph in GLYPHS {
            let width = glyph[0].width();
            assert!(glyph.iter().all(|row| row.width() == width), "{glyph:?}");
        }
    }

    #[test]
    fn a_wide_terminal_gets_the_wordmark_in_a_gradient() {
        let text = header("1.0.0", "~/work", 100);
        let plain = text.plain_text();
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines.len(), 7);
        assert!(lines[0].starts_with("  █████╗"), "{plain}");
        assert_eq!(lines[6], "  v1.0.0 · ~/work");
        assert!(lines.iter().all(|line| line.width() < 100));
        let colors: Vec<Style> = text
            .spans()
            .iter()
            .filter(|(style, _)| matches!(style, Style::BoldColor(_)))
            .map(|(style, _)| *style)
            .collect();
        assert_eq!(colors, GRADIENT.map(Style::BoldColor).to_vec());
    }

    #[test]
    fn a_narrow_terminal_gets_one_line() {
        let plain = header("1.0.0", "~/work", 40).plain_text();
        assert_eq!(plain, "agent86 v1.0.0 · ~/work");
    }
}
