//! Replies rendered as Markdown, block by block as they stream.
//!
//! Python's full-screen app shows a reply as plain text while it streams and
//! re-renders the finished reply as one Rich `Markdown` document. Text in the
//! terminal's scrollback can't be re-rendered, so here a block is rendered once it
//! is complete: [`commit_point`] says how much of the stream is made of complete
//! blocks, and [`render`] draws them. Block boundaries are blank lines outside a
//! code fence, so rendering a reply in pieces gives the same result as rendering it
//! whole; a list is held until it ends, so a loose list is not split either.
//!
//! Model text is never interpreted as anything but Markdown: everything drawn goes
//! through `sanitize`, and links are shown as text with their address.

use super::text::{sanitize, wrap_spans, Line};
use crate::repl::Style;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

/// How much of `text` is complete blocks that can be rendered now: a byte index
/// at a line start, or `None` when nothing is complete yet.
///
/// A block ends at a blank line outside a code fence, after a closing fence, or
/// after a heading, once the next line has begun (the next line decides whether a
/// list or an indented block carries on past the blank line).
pub fn commit_point(text: &str) -> Option<usize> {
    let mut point = None;
    let mut fence: Option<(char, usize)> = None;
    // The block since the last boundary holds list items, so an indented line or
    // another item after a blank line carries it on.
    let mut in_list = false;
    // Blank lines have been seen since the last text.
    let mut after_blank = false;
    // A block (a closing fence or a heading) ended here; it is a boundary once the
    // next line begins.
    let mut ended: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let complete = line.ends_with('\n');
        let body = line.trim_end_matches(['\n', '\r']);
        if let Some(end) = ended.take() {
            point = Some(end);
            in_list = false;
        }

        if let Some((ch, len)) = fence {
            if complete && is_fence_close(body, ch, len) {
                fence = None;
                ended = Some(offset);
            }
            continue;
        }
        if body.trim().is_empty() {
            after_blank |= complete;
            continue;
        }
        if std::mem::take(&mut after_blank) {
            let indented = body.starts_with([' ', '\t']);
            let continues = is_indented(body) || (in_list && (indented || is_list_item(body)));
            if !continues {
                point = Some(start);
                in_list = false;
            }
        }
        if !complete {
            break;
        }
        if let Some(open) = fence_open(body) {
            fence = Some(open);
        } else if is_list_item(body) {
            in_list = true;
        } else if is_heading(body) {
            ended = Some(offset);
        }
    }
    point
}

/// The fence character and length that open a fenced code block, if `line` does.
fn fence_open(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let ch = trimmed
        .chars()
        .next()
        .filter(|&ch| ch == '`' || ch == '~')?;
    let len = trimmed.chars().take_while(|&c| c == ch).count();
    // A backtick fence's info string may not contain a backtick.
    (len >= 3 && !(ch == '`' && trimmed[len..].contains('`'))).then_some((ch, len))
}

fn is_fence_close(line: &str, ch: char, len: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let run = trimmed.chars().take_while(|&c| c == ch).count();
    line.len() - trimmed.len() <= 3 && run >= len && trimmed[run..].trim().is_empty()
}

fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    line.len() - trimmed.len() <= 3
        && (1..=6).contains(&hashes)
        && trimmed[hashes..].chars().next().is_none_or(|c| c == ' ')
}

fn is_indented(line: &str) -> bool {
    line.starts_with("    ") || line.starts_with('\t')
}

fn is_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix(['-', '*', '+']) {
        return rest.is_empty() || rest.starts_with(' ');
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    (1..=9).contains(&digits)
        && trimmed[digits..]
            .strip_prefix(['.', ')'])
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// Where a reply's text has to be cut without a block boundary (the tail limit),
/// the pieces to render: the head, closed if it ends inside a code fence, and the
/// tail, reopened with the same fence so it still renders as code.
pub fn split_inside(text: &str, at: usize) -> (String, String) {
    let (head, tail) = text.split_at(at);
    let mut fence: Option<(char, usize, &str)> = None;
    for line in head.lines() {
        match fence {
            Some((ch, len, _)) if is_fence_close(line, ch, len) => fence = None,
            Some(_) => {}
            None => {
                if let Some((ch, len)) = fence_open(line) {
                    fence = Some((ch, len, line.trim_start()));
                }
            }
        }
    }
    match fence {
        None => (head.to_owned(), tail.to_owned()),
        Some((ch, len, opener)) => {
            let close: String = std::iter::repeat_n(ch, len).collect();
            let head = format!("{}\n{close}\n", head.trim_end_matches('\n'));
            (head, format!("{opener}\n{tail}"))
        }
    }
}

/// `text` rendered as Markdown into rows of at most `width` cells.
pub fn render(text: &str, width: usize) -> Vec<Line> {
    let mut renderer = Renderer::new(width.max(10));
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct ListLevel {
    /// The next number, for an ordered list.
    next: Option<u64>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    cells: Vec<String>,
    head_rows: usize,
}

struct Renderer {
    width: usize,
    out: Vec<Line>,
    /// Inline text of the block being built.
    spans: Vec<(Style, String)>,
    styles: Vec<Style>,
    lists: Vec<ListLevel>,
    /// The marker for the first row of the current list item (`• `, `1. `).
    marker: Option<String>,
    /// How far the current item's text hangs: its marker's width.
    item_indent: usize,
    quotes: usize,
    heading: Option<HeadingLevel>,
    code: Option<String>,
    link: Option<String>,
    table: Option<Table>,
    /// A blank line goes before the next top-level block.
    gap: bool,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            spans: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            marker: None,
            item_indent: 0,
            quotes: 0,
            heading: None,
            code: None,
            link: None,
            table: None,
            gap: false,
        }
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or(Style::Plain)
    }

    fn text(&mut self, text: &str) {
        let text = sanitize(text);
        if let Some(code) = &mut self.code {
            code.push_str(&text);
        } else if let Some(table) = &mut self.table {
            if let Some(cell) = table.cells.last_mut() {
                cell.push_str(&text);
            }
        } else {
            let style = self.style();
            self.spans.push((style, text));
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                if let Some(table) = &mut self.table {
                    if let Some(cell) = table.cells.last_mut() {
                        cell.push_str(&sanitize(&code));
                    }
                } else {
                    self.spans.push((Style::BoldCyan, sanitize(&code)));
                }
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Rule => {
                self.flush();
                self.block_gap();
                self.out
                    .push(Line::styled(Style::Dim, "─".repeat(self.width)));
            }
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            Event::InlineMath(math) | Event::DisplayMath(math) => self.text(&math),
            Event::FootnoteReference(name) => self.text(&format!("[{name}]")),
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block_gap(),
            Tag::Heading { level, .. } => {
                self.flush();
                self.block_gap();
                self.heading = Some(level);
                self.styles.push(match level {
                    HeadingLevel::H1 => Style::BoldUnderline,
                    _ => Style::Bold,
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.block_gap();
                self.quotes += 1;
                self.styles.push(Style::Italic);
            }
            Tag::CodeBlock(_) => {
                self.flush();
                self.block_gap();
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.block_gap();
                }
                self.lists.push(ListLevel { next: start });
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(ListLevel { next: Some(next) }) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.item_indent = marker.width();
                self.marker = Some(marker);
            }
            Tag::Emphasis => self.styles.push(Style::Italic),
            Tag::Strong => self.styles.push(Style::Bold),
            Tag::Strikethrough => self.styles.push(Style::Dim),
            Tag::Link { dest_url, .. } => {
                self.link = Some(sanitize(&dest_url));
                self.styles.push(Style::Cyan);
            }
            Tag::Image { dest_url, .. } => {
                self.link = Some(sanitize(&dest_url));
                self.styles.push(Style::Cyan);
                self.text("image: ");
            }
            Tag::Table(_) => {
                self.flush();
                self.block_gap();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.cells.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cells.push(String::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = None;
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quotes = self.quotes.saturating_sub(1);
                self.styles.pop();
            }
            TagEnd::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                let indent = self.indent_width();
                for row in code.trim_end_matches('\n').split('\n') {
                    let line = Line::styled(Style::Dim, "│ ").push(Style::Plain, row);
                    for piece in line.hard_wrap(self.width.saturating_sub(indent).max(1)) {
                        let mut full = Line::plain(" ".repeat(indent));
                        full.spans.extend(piece.spans);
                        self.out.push(full);
                    }
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.marker = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                if let Some(url) = self.link.take() {
                    let shown: String = self
                        .spans
                        .iter()
                        .rev()
                        .take(1)
                        .map(|(_, t)| t.clone())
                        .collect();
                    if !url.is_empty() && shown != url {
                        self.spans.push((Style::Dim, format!(" ({url})")));
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let cells = std::mem::take(&mut table.cells);
                    table.rows.push(cells);
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let cells = std::mem::take(&mut table.cells);
                    table.rows.push(cells);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.draw_table(&table);
                }
            }
            _ => {}
        }
    }

    /// A blank line before a new top-level block, once there is something above it.
    fn block_gap(&mut self) {
        if self.lists.is_empty() && self.quotes == 0 {
            if self.gap && !self.out.is_empty() {
                self.out.push(Line::default());
            }
            self.gap = true;
        }
    }

    /// How far text in the current list item is indented: two cells per outer
    /// list, then the item's own marker.
    fn indent_width(&self) -> usize {
        if self.lists.is_empty() {
            0
        } else {
            (self.lists.len() - 1) * 2 + self.item_indent
        }
    }

    /// Wrap the inline text built so far under the current list and quote prefixes.
    fn flush(&mut self) {
        if self.spans.iter().all(|(_, text)| text.trim().is_empty()) {
            self.spans.clear();
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let nesting = self.lists.len().saturating_sub(1) * 2;
        let hang = if self.lists.is_empty() {
            0
        } else {
            self.item_indent
        };
        let quote = "▌ ".repeat(self.quotes);
        let marker = self.marker.take();
        let lead = nesting + hang + quote.width();
        let rows = wrap_spans(&spans, self.width.saturating_sub(lead).max(1));
        for (index, row) in rows.into_iter().enumerate() {
            let mut line = Line::default();
            if nesting > 0 {
                line = line.push(Style::Plain, " ".repeat(nesting));
            }
            if !quote.is_empty() {
                line = line.push(Style::Dim, quote.clone());
            }
            match &marker {
                Some(marker) if index == 0 => line = line.push(Style::Cyan, marker.clone()),
                _ if hang > 0 => line = line.push(Style::Plain, " ".repeat(hang)),
                _ => {}
            }
            line.spans.extend(row.spans);
            self.out.push(line);
        }
    }

    fn draw_table(&mut self, table: &Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.trim().width());
            }
        }
        for (index, row) in table.rows.iter().enumerate() {
            let head = index < table.head_rows;
            let mut line = Line::default();
            for (column, width) in widths.iter().enumerate() {
                let cell = row.get(column).map_or("", |cell| cell.trim());
                if column > 0 {
                    line = line.push(Style::Dim, " │ ");
                }
                let pad = width - cell.width();
                line = line.push(
                    if head { Style::Bold } else { Style::Plain },
                    format!("{cell}{}", " ".repeat(pad)),
                );
            }
            self.out.push(line.truncate(self.width));
            if head && index + 1 == table.head_rows {
                let rule: Vec<String> = widths.iter().map(|width| "─".repeat(*width)).collect();
                self.out
                    .push(Line::styled(Style::Dim, rule.join("─┼─")).truncate(self.width));
            }
        }
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush();
        if let Some(code) = self.code.take() {
            // An unterminated fence: still shown as code.
            for row in code.trim_end_matches('\n').split('\n') {
                self.out.push(
                    Line::styled(Style::Dim, "│ ")
                        .push(Style::Plain, row)
                        .truncate(self.width),
                );
            }
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain_text).collect()
    }

    #[test]
    fn blocks_commit_once_the_next_one_begins() {
        assert_eq!(commit_point("First paragraph."), None);
        assert_eq!(
            commit_point("First.\n\n"),
            None,
            "the next block has not begun"
        );
        assert_eq!(commit_point("First.\n\nSec"), Some(8));
        assert_eq!(commit_point("One.\n\nTwo.\n\nThr"), Some(12));
        assert_eq!(
            commit_point("# Title\nBody"),
            Some(8),
            "a heading is a block"
        );
    }

    #[test]
    fn code_fences_and_lists_are_held_until_they_end() {
        let fence = "```rust\nfn a() {}\n\nfn b() {}\n";
        assert_eq!(
            commit_point(fence),
            None,
            "a blank line inside a fence is not a boundary"
        );
        assert_eq!(
            commit_point(&format!("{fence}```\nAfter")),
            Some(fence.len() + 4)
        );
        let loose = "- one\n\n- two\n\n";
        assert_eq!(commit_point(loose), None);
        assert_eq!(commit_point(&format!("{loose}Done")), Some(loose.len()));
        assert_eq!(commit_point("- one\n\n  more of one\n\nx"), Some(22));
    }

    #[test]
    fn a_forced_split_inside_a_fence_closes_and_reopens_it() {
        let (head, tail) = split_inside("```py\nprint(1)\nprint(2)\n", 15);
        assert_eq!(head, "```py\nprint(1)\n```\n");
        assert_eq!(tail, "```py\nprint(2)\n");
        let (head, tail) = split_inside("plain\nmore", 6);
        assert_eq!((head.as_str(), tail.as_str()), ("plain\n", "more"));
    }

    #[test]
    fn markdown_renders_headings_lists_code_and_links() {
        let lines = render(
            "# Plan\n\nRun **cargo test** and `fmt`.\n\n- first\n- second item\n  wraps\n\n1. a\n2. b\n\n```sh\nls -la\n```\n\nSee [docs](https://x.dev).",
            40,
        );
        assert_eq!(
            text(&lines),
            [
                "Plan",
                "",
                "Run cargo test and fmt.",
                "",
                "• first",
                "• second item wraps",
                "",
                "1. a",
                "2. b",
                "",
                "│ ls -la",
                "",
                "See docs (https://x.dev).",
            ]
        );
        assert_eq!(lines[0].spans[0].0, Style::BoldUnderline);
        assert!(lines[2]
            .spans
            .contains(&(Style::Bold, "cargo test".to_owned())));
        assert!(lines[2]
            .spans
            .contains(&(Style::BoldCyan, "fmt".to_owned())));
    }

    #[test]
    fn nested_lists_quotes_and_tables() {
        let lines = render(
            "- a\n  - b\n\n> quoted\n\n| k | v |\n|---|---|\n| x | 10 |",
            40,
        );
        let rows: Vec<String> = text(&lines)
            .into_iter()
            .map(|row| row.trim_end().to_owned())
            .collect();
        assert_eq!(
            rows,
            [
                "• a",
                "  • b",
                "",
                "▌ quoted",
                "",
                "k │ v",
                "──┼───",
                "x │ 10"
            ]
        );
    }

    #[test]
    fn long_text_wraps_and_model_text_cannot_style_the_terminal() {
        let lines = render("word ".repeat(20).trim_end(), 24);
        assert!(lines.iter().all(|line| line.width() <= 24));
        assert!(lines.len() > 3);
        let lines = render("a \x1b[31mred\x1b[0m b", 40);
        assert_eq!(text(&lines), ["a [31mred[0m b"]);
    }
}
