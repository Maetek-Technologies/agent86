//! The inline renderer: a live region at the bottom of the terminal, redrawn in
//! place, with finished lines printed above it into the terminal's own scrollback.
//!
//! This is how Ink, which Claude Code is built on, draws: it remembers how many rows
//! the live region took, moves the cursor back to its top, clears down, prints any
//! newly finished lines (which scroll up into history like ordinary output), and
//! prints the live region again. Every line handed to [`Screen::frame`] for the live
//! region must already fit the width, so the row count is exact.
//!
//! The control sequences are written directly (VT100, which Windows 10 and later
//! support once virtual terminal processing is on), so a frame is one write.

use super::text::Line;
use std::io::{self, Write};

/// Where the terminal cursor goes after a frame: the input caret, or hidden.
pub type Caret = Option<(usize, usize)>;

pub struct Screen<W: Write> {
    out: W,
    color: bool,
    /// Widths of the live rows drawn last, so they can be found again after a resize.
    drawn: Vec<usize>,
    /// The row (within the live region) and column the cursor was left at.
    cursor_row: usize,
    cursor_col: usize,
    width: usize,
    /// The alternate screen (the transcript view) is showing.
    alternate: bool,
}

impl<W: Write> Screen<W> {
    pub fn new(out: W, color: bool, width: usize) -> Self {
        Self {
            out,
            color,
            drawn: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            width: width.max(1),
            alternate: false,
        }
    }

    /// The terminal is now `width` cells wide. Rows already drawn may have been
    /// reflowed by the terminal: a row wider than the new width now takes several.
    pub fn resize(&mut self, width: usize) {
        let width = width.max(1);
        if width < self.width {
            let rows = |cells: usize| cells.div_ceil(width).max(1);
            let before: usize = self.drawn[..self.cursor_row.min(self.drawn.len())]
                .iter()
                .map(|&cells| rows(cells))
                .sum();
            self.cursor_row = before + self.cursor_col / width;
            self.cursor_col %= width;
            self.drawn = self
                .drawn
                .iter()
                .flat_map(|&cells| {
                    let count = rows(cells);
                    (0..count).map(move |index| {
                        if index + 1 < count {
                            width
                        } else {
                            cells - width * (count - 1)
                        }
                    })
                })
                .collect();
        }
        self.width = width;
    }

    /// Print `committed` into scrollback, then draw `live` at the bottom and put the
    /// cursor at `caret` (a row and column within `live`), or hide it.
    pub fn frame(&mut self, committed: &[Line], live: &[Line], caret: Caret) -> io::Result<()> {
        write!(self.out, "\x1b[?2026h\x1b[?25l")?;
        self.erase()?;
        for line in committed {
            write!(self.out, "{}\r\n", line.render(self.color))?;
        }
        for (index, line) in live.iter().enumerate() {
            if index > 0 {
                write!(self.out, "\r\n")?;
            }
            write!(self.out, "{}", line.render(self.color))?;
        }
        self.drawn = live.iter().map(Line::width).collect();
        let last = live.len().saturating_sub(1);
        self.cursor_row = last;
        self.cursor_col = live.last().map_or(0, Line::width);
        if let Some((row, col)) = caret {
            let row = row.min(last);
            if row < last {
                write!(self.out, "\x1b[{}A", last - row)?;
            }
            write!(self.out, "\x1b[{}G\x1b[?25h", col + 1)?;
            self.cursor_row = row;
            self.cursor_col = col;
        }
        write!(self.out, "\x1b[?2026l")?;
        self.out.flush()
    }

    /// Remove the live region, leaving the cursor where it began, as before exiting.
    pub fn clear_live(&mut self) -> io::Result<()> {
        self.erase()?;
        write!(self.out, "\x1b[?25h")?;
        self.drawn.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.out.flush()
    }

    /// Clear the whole screen and start again at the top, as `/clear` does.
    pub fn clear_all(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[2J\x1b[H")?;
        self.drawn.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.out.flush()
    }

    /// Switch to the alternate screen for the full-screen transcript view. The
    /// scrollback and the live region stay as they are and come back on leaving.
    pub fn enter_alternate(&mut self) -> io::Result<()> {
        if self.alternate {
            return Ok(());
        }
        self.alternate = true;
        write!(self.out, "\x1b[?1049h\x1b[?25l")?;
        self.out.flush()
    }

    /// Return from the alternate screen, with the cursor where it was.
    pub fn leave_alternate(&mut self) -> io::Result<()> {
        if !self.alternate {
            return Ok(());
        }
        self.alternate = false;
        write!(self.out, "\x1b[?1049l")?;
        self.out.flush()
    }

    /// Draw a whole page on the alternate screen; each line must fit the width.
    pub fn page(&mut self, lines: &[Line]) -> io::Result<()> {
        write!(self.out, "\x1b[?2026h\x1b[H\x1b[2J")?;
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                write!(self.out, "\r\n")?;
            }
            write!(self.out, "{}", line.render(self.color))?;
        }
        write!(self.out, "\x1b[?2026l")?;
        self.out.flush()
    }

    /// Move to the top of the live region and clear everything below.
    fn erase(&mut self) -> io::Result<()> {
        write!(self.out, "\r")?;
        if self.cursor_row > 0 {
            write!(self.out, "\x1b[{}A", self.cursor_row)?;
        }
        write!(self.out, "\x1b[J")?;
        Ok(())
    }

    #[cfg(test)]
    pub fn output(&self) -> &W {
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(screen: &Screen<Vec<u8>>) -> String {
        String::from_utf8_lossy(screen.output()).into_owned()
    }

    #[test]
    fn a_frame_returns_to_the_top_of_the_last_one_before_drawing() {
        let mut screen = Screen::new(Vec::new(), false, 40);
        let live = [Line::plain("one"), Line::plain("two"), Line::plain("three")];
        screen.frame(&[], &live, None).unwrap();
        assert!(written(&screen).contains("one\r\ntwo\r\nthree"));
        assert_eq!(screen.cursor_row, 2);

        screen
            .frame(&[Line::plain("done")], &live[..1], None)
            .unwrap();
        let out = written(&screen);
        // Up two rows (CSI 2 A), clear down (CSI J), then the committed line first.
        let second = &out[out.rfind("\r\x1b[2A").expect("moved up")..];
        assert!(second.contains("\x1b[J"), "{second:?}");
        assert!(second.contains("done\r\none"), "{second:?}");
    }

    #[test]
    fn the_caret_is_placed_inside_the_live_region() {
        let mut screen = Screen::new(Vec::new(), false, 40);
        let live = [Line::plain("input"), Line::plain("status")];
        screen.frame(&[], &live, Some((0, 4))).unwrap();
        assert_eq!((screen.cursor_row, screen.cursor_col), (0, 4));
        // The next frame starts from the caret's row, not the last row.
        screen.frame(&[], &live, None).unwrap();
        assert_eq!(screen.cursor_row, 1);
    }

    #[test]
    fn narrowing_the_terminal_counts_reflowed_rows() {
        let mut screen = Screen::new(Vec::new(), false, 40);
        let live = [Line::plain("x".repeat(30)), Line::plain("status")];
        screen.frame(&[], &live, None).unwrap();
        screen.resize(20);
        // The 30-cell row now takes two rows, so the last row is the third.
        assert_eq!(screen.drawn, vec![20, 10, 6]);
        assert_eq!(screen.cursor_row, 2);
    }
}
