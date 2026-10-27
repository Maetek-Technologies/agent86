//! Colouring for an approval's detail. Python highlights it with Pygments through
//! Rich's `Syntax` (`ansi_dark`) using the tool's lexer; a unified diff is coloured
//! here the same way (additions green, removals red, hunk headers cyan), and other
//! lexers are shown plain, as there is no syntax highlighter in this port.

use super::text::{sanitize, Line};
use crate::repl::Style;

/// One row of an approval's detail, coloured for its lexer.
pub fn detail_line(row: &str, lexer: Option<&str>) -> Line {
    let row = sanitize(row);
    let style = match lexer {
        Some("diff") if row.starts_with("+++") || row.starts_with("---") => Style::Bold,
        Some("diff") if row.starts_with('+') => Style::Green,
        Some("diff") if row.starts_with('-') => Style::Red,
        Some("diff") if row.starts_with("@@") => Style::Cyan,
        Some(_) => Style::Plain,
        None => Style::Dim,
    };
    Line::styled(style, row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_are_coloured_by_line_kind() {
        let style = |row: &str| detail_line(row, Some("diff")).spans[0].0;
        assert_eq!(style("--- a.txt"), Style::Bold);
        assert_eq!(style("+++ a.txt"), Style::Bold);
        assert_eq!(style("@@ -1 +1 @@"), Style::Cyan);
        assert_eq!(style("+hello"), Style::Green);
        assert_eq!(style("-bye"), Style::Red);
        assert_eq!(style(" same"), Style::Plain);
        assert_eq!(detail_line("ls -la", Some("bash")).spans[0].0, Style::Plain);
        assert_eq!(detail_line("{}", None).spans[0].0, Style::Dim);
    }
}
