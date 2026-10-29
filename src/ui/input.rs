//! The prompt editor: a multi-line draft with a cursor, and Python's
//! `PromptInput` history rules.
//!
//! Up and Down walk the shared prompt history only from the first and last line,
//! so inside a multi-line draft they still move the cursor. The draft being typed
//! is kept while history is browsed and handed back past the newest entry. Any
//! edit ends browsing: what is on screen is then the user's draft.

use super::status::last_component;
use super::text::{sanitize, Line};
use crate::repl::Style;
use unicode_width::UnicodeWidthChar;

/// Tallest the input box grows before it scrolls, as Python's `MAX_ROWS`.
pub const MAX_ROWS: usize = 8;

#[derive(Debug, Default)]
pub struct Editor {
    /// The draft, as characters, so the cursor is a plain index.
    text: Vec<char>,
    cursor: usize,
    /// Index into the history while browsing it.
    browsing: Option<usize>,
    /// The draft that was on screen when browsing started.
    draft: Vec<char>,
    /// First visual row shown, when the draft is taller than [`MAX_ROWS`].
    scroll: usize,
}

impl Editor {
    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Replace the draft, with the cursor at the end, and stop browsing history.
    pub fn set_text(&mut self, text: &str) {
        self.set(text.chars().collect());
        self.browsing = None;
        self.draft.clear();
    }

    /// The cursor, as a character index into the draft.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace characters `start..end` with `text` and put the cursor after it.
    pub fn replace(&mut self, start: usize, end: usize, text: &str) {
        let end = end.min(self.text.len());
        let start = start.min(end);
        let inserted: Vec<char> = text.chars().collect();
        self.cursor = start + inserted.len();
        self.text.splice(start..end, inserted);
        self.browsing = None;
    }

    pub fn insert(&mut self, text: &str) {
        self.browsing = None;
        for ch in text.chars() {
            match ch {
                '\t' => (0..4).for_each(|_| self.put(' ')),
                '\n' => self.put('\n'),
                // Escape sequences and carriage returns from a paste never reach the draft.
                c if c.is_control() => {}
                c => self.put(c),
            }
        }
    }

    fn put(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.browsing = None;
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.browsing = None;
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.len());
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    pub fn word_left(&mut self) {
        while self.cursor > 0 && self.text[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.text[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    pub fn word_right(&mut self) {
        let len = self.text.len();
        while self.cursor < len && self.text[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < len && !self.text[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    /// Ctrl+W: delete the word before the cursor.
    pub fn delete_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.drain(self.cursor..end);
        self.browsing = None;
    }

    /// Ctrl+U: delete back to the start of the line.
    pub fn delete_to_line_start(&mut self) {
        let start = self.line_start(self.cursor);
        self.text.drain(start..self.cursor);
        self.cursor = start;
        self.browsing = None;
    }

    /// Ctrl+K: delete to the end of the line.
    pub fn delete_to_line_end(&mut self) {
        let end = self.line_end(self.cursor);
        self.text.drain(self.cursor..end);
        self.browsing = None;
    }

    /// Up: the line above, or an older history entry from the first line.
    pub fn up(&mut self, history: &[String]) {
        let start = self.line_start(self.cursor);
        if start > 0 {
            let column = self.cursor - start;
            let above = self.line_start(start - 1);
            self.cursor = (above + column).min(start - 1);
            return;
        }
        if history.is_empty() {
            return;
        }
        let index = match self.browsing {
            None => {
                self.draft = self.text.clone();
                history.len() - 1
            }
            Some(0) => return,
            Some(index) => index - 1,
        };
        self.browsing = Some(index);
        self.set(history[index].chars().collect());
    }

    /// Down: the line below, a newer history entry from the last line, then the draft.
    pub fn down(&mut self, history: &[String]) {
        let end = self.line_end(self.cursor);
        if end < self.text.len() {
            let column = self.cursor - self.line_start(self.cursor);
            let below = end + 1;
            self.cursor = (below + column).min(self.line_end(below));
            return;
        }
        match self.browsing {
            None => {}
            Some(index) if index + 1 < history.len() => {
                self.browsing = Some(index + 1);
                self.set(history[index + 1].chars().collect());
            }
            Some(_) => {
                self.browsing = None;
                let draft = std::mem::take(&mut self.draft);
                self.set(draft);
            }
        }
    }

    /// Replace the text, with the cursor at the end where typing continues.
    fn set(&mut self, text: Vec<char>) {
        self.cursor = text.len();
        self.text = text;
        self.scroll = 0;
    }

    fn line_start(&self, index: usize) -> usize {
        self.text[..index]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |at| at + 1)
    }

    fn line_end(&self, index: usize) -> usize {
        self.text[index..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.text.len(), |at| index + at)
    }

    /// The draft wrapped to `width` cells: the visual rows, and the caret's row
    /// and column among them. Rows break at the width, not at words, so the caret
    /// is always where the next character goes.
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(2);
        let mut rows = vec![String::new()];
        let mut used = 0;
        let mut caret = (0, 0);
        for (index, &ch) in self.text.iter().enumerate() {
            if index == self.cursor {
                caret = (rows.len() - 1, used);
            }
            if ch == '\n' {
                rows.push(String::new());
                used = 0;
                continue;
            }
            let cells = ch.width().unwrap_or(0);
            if used + cells > width {
                rows.push(String::new());
                used = 0;
                if index == self.cursor {
                    caret = (rows.len() - 1, 0);
                }
            }
            rows.last_mut().expect("rows is never empty").push(ch);
            used += cells;
        }
        if self.cursor == self.text.len() {
            caret = if used >= width {
                rows.push(String::new());
                (rows.len() - 1, 0)
            } else {
                (rows.len() - 1, used)
            };
        }
        (rows, caret)
    }

    /// The bordered input box, `width` cells wide, and the caret within it.
    /// `active` is false while a turn runs: the draft is kept but dimmed. `title`
    /// (the working directory, beyond Python) sits right-aligned in the top border.
    pub fn render(
        &mut self,
        width: usize,
        active: bool,
        title: &str,
    ) -> (Vec<Line>, (usize, usize)) {
        let width = width.max(8);
        // "│ > " before the text and " │" after it.
        let inner = width - 6;
        let (rows, (caret_row, caret_col)) = self.layout(inner);
        if caret_row < self.scroll {
            self.scroll = caret_row;
        } else if caret_row >= self.scroll + MAX_ROWS {
            self.scroll = caret_row + 1 - MAX_ROWS;
        }
        let shown = &rows[self.scroll..rows.len().min(self.scroll + MAX_ROWS)];
        let border = Style::Dim;
        let text_style = if active { Style::Plain } else { Style::Dim };
        let mut lines = vec![top_border(width, title)];
        for (index, row) in shown.iter().enumerate() {
            let lead = if self.scroll + index == 0 { "> " } else { "  " };
            let pad = inner.saturating_sub(unicode_width::UnicodeWidthStr::width(row.as_str()));
            lines.push(
                Line::styled(border, "│ ")
                    .push(Style::Bold, lead)
                    .push(text_style, row.clone())
                    .push(Style::Plain, " ".repeat(pad))
                    .push(border, " │"),
            );
        }
        lines.push(Line::styled(border, format!("╰{}╯", "─".repeat(width - 2))));
        (lines, (1 + caret_row - self.scroll, 4 + caret_col))
    }
}

/// The box's top border with `title` set into it near the right corner. A title
/// too long for the width falls back to its last component, then to none, so the
/// border keeps at least a few cells of rule on the left.
fn top_border(width: usize, title: &str) -> Line {
    let title = sanitize(title);
    let fits = |text: &str| {
        let cells = unicode_width::UnicodeWidthStr::width(text);
        (!text.is_empty() && cells + 8 <= width).then_some(cells)
    };
    let short = last_component(&title).to_owned();
    let Some((title, cells)) = fits(&title)
        .map(|cells| (title.clone(), cells))
        .or_else(|| fits(&short).map(|cells| (short, cells)))
    else {
        return Line::styled(Style::Dim, format!("╭{}╮", "─".repeat(width - 2)));
    };
    // "╭" + rule + " title " + "─╮"
    let rule = width - cells - 5;
    Line::styled(Style::Dim, format!("╭{} ", "─".repeat(rule)))
        .push(Style::Plain, title)
        .push(Style::Dim, " ─╮")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Editor {
        let mut editor = Editor::default();
        editor.insert(text);
        editor
    }

    #[test]
    fn editing_moves_the_cursor_and_keeps_newlines() {
        let mut editor = typed("hello\nworld");
        editor.home();
        editor.insert(">");
        assert_eq!(editor.text(), "hello\n>world");
        editor.delete_word();
        assert_eq!(editor.text(), "hello\nworld");
        editor.end();
        editor.backspace();
        assert_eq!(editor.text(), "hello\nworl");
        editor.delete_to_line_start();
        assert_eq!(editor.text(), "hello\n");
        editor.insert("a\tb\x1b");
        assert_eq!(editor.text(), "hello\na    b");
    }

    #[test]
    fn history_is_browsed_only_from_the_edge_lines() {
        let history = vec!["first".to_owned(), "second".to_owned()];
        let mut editor = typed("draft");
        editor.up(&history);
        assert_eq!(editor.text(), "second");
        editor.up(&history);
        assert_eq!(editor.text(), "first");
        editor.up(&history);
        assert_eq!(editor.text(), "first", "the oldest entry stays put");
        editor.down(&history);
        editor.down(&history);
        assert_eq!(
            editor.text(),
            "draft",
            "past the newest, the draft comes back"
        );

        let mut multi = typed("one\ntwo");
        multi.up(&history);
        assert_eq!(
            multi.text(),
            "one\ntwo",
            "Up moved the cursor, not the history"
        );
        multi.up(&history);
        assert_eq!(multi.text(), "second");
    }

    #[test]
    fn typing_ends_history_browsing() {
        let history = vec!["first".to_owned()];
        let mut editor = typed("draft");
        editor.up(&history);
        editor.insert("!");
        editor.down(&history);
        assert_eq!(editor.text(), "first!");
    }

    #[test]
    fn a_span_can_be_replaced_in_place() {
        let mut editor = typed("see @sr now");
        editor.replace(4, 7, "@src/");
        assert_eq!(editor.text(), "see @src/ now");
        assert_eq!(editor.cursor(), 9);
        editor.set_text("/model ");
        assert_eq!((editor.text().as_str(), editor.cursor()), ("/model ", 7));
    }

    #[test]
    fn the_layout_wraps_and_tracks_the_caret() {
        let editor = typed("abcdef");
        assert_eq!(editor.layout(4), (vec!["abcd".into(), "ef".into()], (1, 2)));
        let full = typed("abcd");
        assert_eq!(full.layout(4), (vec!["abcd".into(), String::new()], (1, 0)));
        let mut middle = typed("ab\ncd");
        middle.up(&[]);
        assert_eq!(middle.layout(10).1, (0, 2));
    }

    #[test]
    fn the_box_is_bordered_and_scrolls_past_eight_rows() {
        let mut editor = typed("hi");
        let (lines, caret) = editor.render(20, true, "");
        assert_eq!(lines[0].plain_text(), format!("╭{}╮", "─".repeat(18)));
        assert_eq!(lines[1].plain_text(), format!("│ > hi{} │", " ".repeat(12)));
        assert_eq!(caret, (1, 6));
        let mut tall = typed(&"x\n".repeat(12));
        let (lines, caret) = tall.render(20, true, "");
        assert_eq!(lines.len(), MAX_ROWS + 2);
        assert_eq!(caret.0, MAX_ROWS);
    }

    #[test]
    fn the_working_directory_sits_in_the_top_border() {
        let row = |width, title| top_border(width, title).plain_text();
        assert_eq!(
            row(30, "~/src/app"),
            format!("╭{} ~/src/app ─╮", "─".repeat(16))
        );
        // Too long for the width: the last component, then a bare border.
        assert_eq!(row(14, "~/src/app"), format!("╭{} app ─╮", "─".repeat(6)));
        assert_eq!(row(10, "~/src/app"), format!("╭{}╮", "─".repeat(8)));
        for width in [10, 14, 30] {
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(row(width, "~/src/app").as_str()),
                width
            );
        }
        // Untrusted text from the filesystem cannot carry escape sequences.
        assert!(!row(40, "~/a\x1b[31mb").contains('\x1b'));
    }
}
