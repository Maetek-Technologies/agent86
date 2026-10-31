//! The transcript as entries, ported from Python's `tui/widgets/transcript.py` and
//! `tool_block.py`, kept beside the scrollback for the Ctrl+O transcript view.
//!
//! What is printed into scrollback can't change afterwards, so a tool call is
//! printed collapsed (its name, arguments, and the first line of its result) and
//! the view shows it expanded, as Python's `ToolBlockEntry.body_lines` does: the
//! full arguments as JSON and the result, capped at [`RESULT_LINE_LIMIT`] lines.

use super::text::{ellipsize, sanitize, Line};
use crate::repl::Style;

/// How much of the argument JSON fits on a tool block's first line.
pub const ARG_PREVIEW_LIMIT: usize = 72;
/// How much of the result summary fits on a tool block's second line.
pub const SUMMARY_LIMIT: usize = 100;
/// How many lines of a result an expanded block shows, as Python's.
pub const RESULT_LINE_LIMIT: usize = 200;
/// The summary of a call the turn ended without seeing answered.
pub const PENDING_SUMMARY: &str = "(no result)";

pub enum Entry {
    /// Lines exactly as they were printed: prompts, replies, notes, command output.
    Lines(Vec<Line>),
    Tool(ToolEntry),
}

/// One tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolEntry {
    pub name: String,
    /// The arguments as one-line JSON, for the collapsed form.
    pub preview: String,
    /// The arguments as indented JSON, for the expanded form.
    pub args: String,
    /// `None` until the result lands.
    pub summary: Option<String>,
    /// The full result the model saw.
    pub result: String,
    pub ok: bool,
}

impl ToolEntry {
    /// `⏺ name(args)` and `⎿ summary`, cut to `width`, after a blank line.
    pub fn collapsed(&self, width: usize) -> Vec<Line> {
        let marker = if self.ok { Style::Green } else { Style::Red };
        let head = Line::styled(marker, "⏺ ")
            .push(Style::Bold, sanitize(&self.name))
            .push(
                Style::Plain,
                format!(
                    "({})",
                    ellipsize(&sanitize(&self.preview), ARG_PREVIEW_LIMIT)
                ),
            );
        let summary = self.summary.as_deref().unwrap_or(PENDING_SUMMARY);
        let body = Line::styled(Style::Dim, "  ⎿  ").push(
            if self.ok { Style::Dim } else { Style::DimRed },
            ellipsize(&sanitize(summary), SUMMARY_LIMIT),
        );
        vec![Line::default(), head.truncate(width), body.truncate(width)]
    }

    /// Python's `body_lines`: the full arguments, then the (capped) result.
    pub fn body_lines(&self) -> Vec<String> {
        self.body_lines_capped(Some(RESULT_LINE_LIMIT))
    }

    /// [`ToolEntry::body_lines`] with the result cut to `cap` lines, or whole for `None`.
    pub fn body_lines_capped(&self, cap: Option<usize>) -> Vec<String> {
        let cap = cap.unwrap_or(usize::MAX);
        let mut lines = vec!["  args:".to_owned()];
        let args = if self.args.is_empty() {
            "{}"
        } else {
            self.args.as_str()
        };
        lines.extend(args.lines().map(|line| format!("    {line}")));
        lines.push("  result:".to_owned());
        let body = if self.result.is_empty() {
            self.summary.as_deref().unwrap_or_default()
        } else {
            self.result.as_str()
        }
        .trim_end();
        if body.is_empty() {
            lines.push("    (no output)".to_owned());
        } else {
            let all: Vec<&str> = body.lines().collect();
            lines.extend(all.iter().take(cap).map(|line| format!("    {line}")));
            if all.len() > cap {
                lines.push(format!("    … truncated ({} more lines)", all.len() - cap));
            }
        }
        lines
    }

    /// The block as `[ui] tool_use = "full"` prints it into scrollback: the call,
    /// then the arguments and the whole result, which the transcript view caps.
    pub fn full(&self, width: usize) -> Vec<Line> {
        let mut lines = self.collapsed(width);
        for row in self.body_lines_capped(None) {
            lines.extend(Line::styled(Style::Dim, sanitize(&row)).hard_wrap(width));
        }
        lines
    }

    /// The block as the transcript view draws it.
    pub fn lines(&self, width: usize, expanded: bool) -> Vec<Line> {
        let mut lines = self.collapsed(width);
        if expanded {
            for row in self.body_lines() {
                lines.extend(Line::styled(Style::Dim, sanitize(&row)).hard_wrap(width));
            }
        }
        lines
    }
}

/// Every entry as the transcript view draws it at `width`.
pub fn view_lines(entries: &[Entry], width: usize, expanded: bool) -> Vec<Line> {
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            Entry::Lines(lines) => {
                out.extend(lines.iter().flat_map(|line| line.hard_wrap(width)));
            }
            Entry::Tool(tool) => out.extend(tool.lines(width, expanded)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ToolEntry {
        ToolEntry {
            name: "write_file".into(),
            preview: r#"{"path": "out.txt", "content": "hello"}"#.into(),
            args: "{\n  \"path\": \"out.txt\",\n  \"content\": \"hello\"\n}".into(),
            summary: Some("Wrote 5 bytes to out.txt.".into()),
            result: "Wrote 5 bytes to out.txt.".into(),
            ok: true,
        }
    }

    #[test]
    fn expanded_blocks_show_python_s_body_lines() {
        assert_eq!(
            tool().body_lines(),
            [
                "  args:",
                "    {",
                "      \"path\": \"out.txt\",",
                "      \"content\": \"hello\"",
                "    }",
                "  result:",
                "    Wrote 5 bytes to out.txt.",
            ]
        );
        let empty = ToolEntry {
            name: "list_dir".into(),
            ..ToolEntry::default()
        };
        assert_eq!(
            empty.body_lines(),
            ["  args:", "    {}", "  result:", "    (no output)"]
        );
    }

    #[test]
    fn long_results_are_capped_at_python_s_limit() {
        let mut long = tool();
        long.result = (0..250).map(|n| format!("line {n}\n")).collect();
        let lines = long.body_lines();
        assert_eq!(lines.last().unwrap(), "    … truncated (50 more lines)");
        // `args:` and four JSON lines, `result:`, the kept lines, and the note.
        assert_eq!(lines.len(), 5 + 1 + RESULT_LINE_LIMIT + 1);
    }

    #[test]
    fn the_view_expands_tools_and_keeps_other_lines() {
        let entries = vec![Entry::Lines(vec![Line::plain("> hi")]), Entry::Tool(tool())];
        let collapsed: Vec<String> = view_lines(&entries, 60, false)
            .iter()
            .map(Line::plain_text)
            .collect();
        assert_eq!(collapsed.len(), 4);
        assert_eq!(collapsed[3], "  ⎿  Wrote 5 bytes to out.txt.");
        let expanded = view_lines(&entries, 60, true);
        assert_eq!(expanded.len(), 4 + 7);
        assert!(expanded.iter().all(|line| line.width() <= 60));
    }
}
