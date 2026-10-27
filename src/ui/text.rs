//! Styled lines and the text shaping the interface needs: wrapping to a width in
//! terminal cells, truncation, and making untrusted text safe to draw.

use crate::repl::{Style, Text};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One row of styled text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<(Style, String)>,
}

impl Line {
    pub fn plain(text: impl Into<String>) -> Self {
        Self::styled(Style::Plain, text)
    }

    pub fn styled(style: Style, text: impl Into<String>) -> Self {
        Self {
            spans: vec![(style, text.into())],
        }
    }

    pub fn push(mut self, style: Style, text: impl Into<String>) -> Self {
        self.spans.push((style, text.into()));
        self
    }

    /// Width in terminal cells.
    pub fn width(&self) -> usize {
        self.spans.iter().map(|(_, text)| text.width()).sum()
    }

    #[cfg(test)]
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|(_, text)| text.as_str()).collect()
    }

    /// The row with ANSI styling when `color` is on.
    pub fn render(&self, color: bool) -> String {
        let mut out = String::new();
        for (style, text) in &self.spans {
            if color && *style != Style::Plain && !text.is_empty() {
                out.push_str(style.ansi());
                out.push_str(text);
                out.push_str("\x1b[0m");
            } else {
                out.push_str(text);
            }
        }
        out
    }

    /// The row split into rows of at most `width` cells, breaking anywhere.
    pub fn hard_wrap(&self, width: usize) -> Vec<Line> {
        let width = width.max(1);
        let mut rows = vec![Line::default()];
        let mut used = 0;
        for (style, text) in &self.spans {
            let mut piece = String::new();
            for ch in text.chars() {
                let cells = ch.width().unwrap_or(0);
                if used + cells > width && used > 0 {
                    if !piece.is_empty() {
                        rows.last_mut()
                            .expect("rows is never empty")
                            .spans
                            .push((*style, std::mem::take(&mut piece)));
                    }
                    rows.push(Line::default());
                    used = 0;
                }
                piece.push(ch);
                used += cells;
            }
            if !piece.is_empty() {
                rows.last_mut()
                    .expect("rows is never empty")
                    .spans
                    .push((*style, piece));
            }
        }
        rows
    }

    /// The row cut to `width` cells, ending in `…` when anything was cut.
    pub fn truncate(&self, width: usize) -> Line {
        if self.width() <= width {
            return self.clone();
        }
        let mut out = Line::default();
        let mut used = 0;
        let budget = width.saturating_sub(1);
        'spans: for (style, text) in &self.spans {
            let mut piece = String::new();
            for ch in text.chars() {
                let cells = ch.width().unwrap_or(0);
                if used + cells > budget {
                    if !piece.is_empty() {
                        out.spans.push((*style, piece));
                    }
                    break 'spans;
                }
                piece.push(ch);
                used += cells;
            }
            out.spans.push((*style, piece));
        }
        if width > 0 {
            let style = out.spans.last().map_or(Style::Plain, |(style, _)| *style);
            out.spans.push((style, "…".into()));
        }
        out
    }
}

/// Command output (styled spans with embedded newlines) as lines.
pub fn text_lines(text: &Text) -> Vec<Line> {
    let mut lines = vec![Line::default()];
    for (style, piece) in text.spans() {
        let mut parts = piece.split('\n');
        if let Some(first) = parts.next() {
            if !first.is_empty() {
                let line = lines.last_mut().expect("lines is never empty");
                line.spans.push((*style, sanitize(first)));
            }
        }
        for part in parts {
            let mut line = Line::default();
            if !part.is_empty() {
                line.spans.push((*style, sanitize(part)));
            }
            lines.push(line);
        }
    }
    lines
}

/// Text from a model, a tool, a server, or a file, made safe to draw: tabs become
/// spaces and other control characters (escape sequences included) are dropped,
/// so nothing it contains can move the cursor or restyle the terminal. Newlines stay.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// One line of text word-wrapped into rows of at most `width` cells. Words longer
/// than a row are broken.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for word in split_keeping_spaces(text) {
        let cells = word.width();
        if used + cells <= width {
            row.push_str(word);
            used += cells;
            continue;
        }
        if word.trim().is_empty() {
            // A space at the end of a row is dropped rather than starting the next one.
            rows.push(std::mem::take(&mut row));
            used = 0;
            continue;
        }
        if used > 0 {
            rows.push(std::mem::take(&mut row).trim_end().to_owned());
            used = 0;
        }
        for ch in word.chars() {
            let cells = ch.width().unwrap_or(0);
            if used + cells > width && used > 0 {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += cells;
        }
    }
    rows.push(row);
    rows
}

/// Styled text word-wrapped into rows of at most `width` cells; a `\n` in a span
/// starts a new row. Words longer than a row are broken.
pub fn wrap_spans(spans: &[(Style, String)], width: usize) -> Vec<Line> {
    let width = width.max(1);
    let mut rows = vec![Line::default()];
    let mut used = 0;
    for (style, text) in spans {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                rows.push(Line::default());
                used = 0;
            }
            for word in split_keeping_spaces(part) {
                let cells = word.width();
                if used + cells <= width {
                    append(rows.last_mut(), *style, word);
                    used += cells;
                    continue;
                }
                if word.trim().is_empty() {
                    // A space at the end of a row is dropped rather than starting the next one.
                    rows.push(Line::default());
                    used = 0;
                    continue;
                }
                if used > 0 {
                    trim_end(rows.last_mut());
                    rows.push(Line::default());
                    used = 0;
                }
                for ch in word.chars() {
                    let cells = ch.width().unwrap_or(0);
                    if used + cells > width && used > 0 {
                        rows.push(Line::default());
                        used = 0;
                    }
                    append(rows.last_mut(), *style, ch.encode_utf8(&mut [0; 4]));
                    used += cells;
                }
            }
        }
    }
    rows
}

/// Add `text` to the row, joining the last span when the style is the same.
fn append(row: Option<&mut Line>, style: Style, text: &str) {
    let Some(row) = row else { return };
    match row.spans.last_mut() {
        Some((last, piece)) if *last == style => piece.push_str(text),
        _ => row.spans.push((style, text.to_owned())),
    }
}

fn trim_end(row: Option<&mut Line>) {
    let Some(row) = row else { return };
    while let Some((_, piece)) = row.spans.last_mut() {
        let trimmed = piece.trim_end_matches(' ').len();
        piece.truncate(trimmed);
        if !piece.is_empty() {
            break;
        }
        row.spans.pop();
    }
}

/// `text` split into runs of spaces and runs of everything else.
fn split_keeping_spaces(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut in_space = None;
    for (index, ch) in text.char_indices() {
        let space = ch == ' ';
        if in_space.is_some_and(|previous| previous != space) {
            pieces.push(&text[start..index]);
            start = index;
        }
        in_space = Some(space);
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// One-line preview of `text`, whitespace collapsed and cut at `limit` characters,
/// as Python's `ellipsize`.
pub fn ellipsize(text: &str, limit: usize) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= limit {
        return single;
    }
    let head: String = single.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_wrap_at_spaces_and_long_words_break() {
        assert_eq!(
            wrap_words("the quick brown fox", 9),
            ["the quick", "brown fox"]
        );
        assert_eq!(wrap_words("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap_words("", 4), [""]);
        assert_eq!(wrap_words("ab  cd", 3), ["ab", "cd"]);
    }

    #[test]
    fn styled_text_wraps_at_words_and_keeps_its_styles() {
        let spans = vec![
            (Style::Plain, "run ".to_owned()),
            (Style::BoldCyan, "cargo test".to_owned()),
            (Style::Plain, " then\nship it".to_owned()),
        ];
        let rows = wrap_spans(&spans, 10);
        let text: Vec<String> = rows.iter().map(Line::plain_text).collect();
        assert_eq!(text, ["run cargo", "test then", "ship it"]);
        assert_eq!(
            rows[0].spans,
            vec![
                (Style::Plain, "run ".to_owned()),
                (Style::BoldCyan, "cargo".to_owned())
            ]
        );
        assert_eq!(rows[1].spans[0], (Style::BoldCyan, "test".to_owned()));
    }

    #[test]
    fn wide_characters_count_as_two_cells() {
        assert_eq!(
            wrap_words("日本語のテキスト", 6),
            ["日本語", "のテキ", "スト"]
        );
        let line = Line::plain("日本語");
        assert_eq!(line.width(), 6);
        assert_eq!(line.truncate(5).plain_text(), "日本…");
    }

    #[test]
    fn untrusted_text_cannot_carry_escape_sequences() {
        assert_eq!(sanitize("a\x1b[31mred\tb\r\nc"), "a[31mred    b\nc");
    }

    #[test]
    fn hard_wrapping_keeps_styles() {
        let line = Line::styled(Style::Bold, "abc").push(Style::Dim, "defg");
        let rows = line.hard_wrap(3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].spans, vec![(Style::Dim, "def".to_owned())]);
    }

    #[test]
    fn previews_collapse_whitespace_like_python() {
        assert_eq!(ellipsize("a\n  b c", 10), "a b c");
        assert_eq!(ellipsize("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn command_output_splits_into_lines() {
        let mut text = Text::styled(Style::Bold, "title");
        text.push(Style::Plain, "\nrow 1\nrow 2");
        let lines = text_lines(&text);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans, vec![(Style::Bold, "title".to_owned())]);
        assert_eq!(lines[2].plain_text(), "row 2");
    }
}
