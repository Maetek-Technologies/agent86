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

/// The detective from the logo, 16 by 16 pixels; two pixel rows make one terminal
/// row (half blocks), so it is 16 cells wide and 8 tall. `.` is transparent.
const MASCOT: [&str; 16] = [
    "................",
    ".....hhhhhh.....",
    "....hhhhhhhh....",
    "....hhhhhhhh....",
    "...bbbbbbbbbb...",
    ".HHHHHHHHHHHHHH.",
    "....ssssssss....",
    "....seessees....",
    "....ssssssss....",
    ".....ssSSss.....",
    "...wwccssccww...",
    "..cccccwwccccc..",
    ".SSccccccccccSS.",
    ".SScccCCCCcccSS.",
    ".SScccCCCCcccSS.",
    "..ScccCCCCcccS..",
];

/// The mascot's width in cells, and the gap between it and the wordmark.
const MASCOT_WIDTH: usize = 16;
const MASCOT_GAP: usize = 2;

/// A mascot pixel's 256-colour index: hat grey and band orange, the robot's
/// silver face with cyan eyes, a cream collar, and a blue coat.
fn pixel_color(pixel: u8) -> Option<u8> {
    match pixel {
        b'h' => Some(245),
        b'H' => Some(240),
        b'b' => Some(208),
        b's' => Some(252),
        b'S' => Some(246),
        b'e' => Some(87),
        b'w' => Some(223),
        b'c' => Some(67),
        b'C' => Some(24),
        _ => None,
    }
}

/// One terminal row of the mascot, from pixel rows `2 * row` and `2 * row + 1`.
fn mascot_row(row: usize, text: &mut Text) {
    let (top, bottom) = (MASCOT[2 * row].as_bytes(), MASCOT[2 * row + 1].as_bytes());
    for (&top, &bottom) in top.iter().zip(bottom) {
        let (top, bottom) = (pixel_color(top), pixel_color(bottom));
        let (glyph, fg, bg) = match (top, bottom) {
            (None, None) => {
                text.push(Style::Plain, " ");
                continue;
            }
            (Some(top), None) => ('▀', top, None),
            (None, Some(bottom)) => ('▄', bottom, None),
            (Some(top), Some(bottom)) if top == bottom => ('█', top, None),
            (Some(top), Some(bottom)) => ('▀', top, Some(bottom)),
        };
        text.push(Style::Pixels { fg, bg }, glyph);
    }
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
    if width >= 1 + MASCOT_WIDTH + MASCOT_GAP + mark_width {
        // The mascot beside the wordmark: the wordmark's six rows sit on the
        // mascot's rows 1 to 6, the version under them, the copyright below.
        let gap = " ".repeat(MASCOT_GAP);
        for line in 0..8 {
            text.push(Style::Plain, " ");
            mascot_row(line, &mut text);
            text.push(Style::Plain, &gap);
            if (1..=6).contains(&line) {
                let color = GRADIENT[line - 1];
                text.push(Style::BoldColor(color), rows[line - 1].trim_end());
            } else if line == 7 {
                text.push(Style::Dim, &info);
            }
            text.push(Style::Plain, "\n");
        }
        text.push(
            Style::Dim,
            format!("{}{COPYRIGHT}", " ".repeat(1 + MASCOT_WIDTH + MASCOT_GAP)),
        );
    } else if width >= mark_width {
        for (row, color) in rows.into_iter().zip(GRADIENT) {
            text.push(Style::BoldColor(color), format!(" {}", row.trim_end()));
            text.push(Style::Plain, "\n");
        }
        text.push(Style::Dim, format!("  {info}"));
        text.push(Style::Plain, "\n");
        text.push(Style::Dim, format!("  {COPYRIGHT}"));
    } else {
        text.push(Style::BoldColor(GRADIENT[0]), "agent86");
        text.push(Style::Dim, format!(" {info}"));
        text.push(Style::Plain, "\n");
        text.push(Style::Dim, COPYRIGHT);
    }
    text
}

/// The line under the version, matching the LICENSE.
const COPYRIGHT: &str = "Copyright © 2026 Maetek Technologies";

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
    fn every_mascot_row_is_sixteen_pixels_wide() {
        assert!(MASCOT.iter().all(|row| row.len() == MASCOT_WIDTH));
    }

    #[test]
    fn a_very_wide_terminal_gets_the_mascot_beside_the_wordmark() {
        let text = header("1.0.0", "~/work", 100);
        let plain = text.plain_text();
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines.len(), 9, "{plain}");
        assert!(lines.iter().all(|line| line.width() < 100), "{plain}");
        // The wordmark's first row sits on the mascot's second row.
        let indent = 1 + MASCOT_WIDTH + MASCOT_GAP;
        let beside = |line: &str| line.chars().skip(indent).collect::<String>();
        assert_eq!(beside(lines[0]), "", "{plain}");
        for (line, row) in lines[1..7].iter().zip(wordmark()) {
            assert_eq!(beside(line), row.trim_end(), "{plain}");
        }
        assert_eq!(beside(lines[7]), "v1.0.0 · ~/work", "{plain}");
        assert_eq!(
            lines[8],
            format!("{}Copyright © 2026 Maetek Technologies", " ".repeat(indent))
        );
    }

    #[test]
    fn a_wide_terminal_gets_the_wordmark_in_a_gradient() {
        let text = header("1.0.0", "~/work", 80);
        let plain = text.plain_text();
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines.len(), 8);
        assert!(lines[0].starts_with("  █████╗"), "{plain}");
        assert_eq!(lines[6], "  v1.0.0 · ~/work");
        // The copyright sits under the version; the session line follows the header.
        assert_eq!(lines[7], "  Copyright © 2026 Maetek Technologies");
        assert!(lines.iter().all(|line| line.width() < 80));
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
        assert_eq!(
            plain,
            "agent86 v1.0.0 · ~/work\nCopyright © 2026 Maetek Technologies"
        );
    }
}
