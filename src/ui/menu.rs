//! The completion menu under the input box, ported from Python's command palette
//! (`_sync_palette`) and `@file` completion (`_sync_mention_palette`).
//!
//! `@path` completion wins when the cursor is inside a mention: a line can hold
//! both a mention and prose, and only a line that is a command can be one. The
//! command menu follows Python's rule: the draft starts with `/` and holds no space.

use super::text::{ellipsize, sanitize, Line};
use crate::mentions::complete_mentions;
use crate::repl::{Style, COMMANDS};
use std::path::Path;

/// Most menu rows shown at once; the rest scroll.
pub const MENU_ROWS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuKind {
    /// Slash commands; the value is the command's name.
    Commands,
    /// Workspace paths for the `@` mention between these character indices of the
    /// draft; choosing one replaces that span.
    Paths { start: usize, end: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub value: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub kind: MenuKind,
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl Menu {
    /// The menu for this draft and cursor, or `None` when nothing applies.
    pub fn for_draft(text: &str, cursor: usize, workspace: Option<&Path>) -> Option<Self> {
        if let Some((prefix, start, end)) = mention_token(text, cursor) {
            let workspace = workspace?;
            let items: Vec<MenuItem> = complete_mentions(&prefix, workspace)
                .into_iter()
                .map(|value| MenuItem {
                    value,
                    description: String::new(),
                })
                .collect();
            return (!items.is_empty()).then_some(Self {
                kind: MenuKind::Paths { start, end },
                items,
                selected: 0,
            });
        }
        if !text.starts_with('/') || text.contains(' ') {
            return None;
        }
        let items: Vec<MenuItem> = COMMANDS
            .iter()
            .filter(|entry| entry.name.starts_with(text))
            .map(|entry| MenuItem {
                value: entry.name.to_owned(),
                description: entry.description.to_owned(),
            })
            .collect();
        // A command typed in full is the one highlighted, so `/mode` and Enter runs
        // `/mode`, not `/models`, which comes first. Python highlights the first row.
        let selected = items
            .iter()
            .position(|item| item.value == text)
            .unwrap_or(0);
        (!items.is_empty()).then_some(Self {
            kind: MenuKind::Commands,
            items,
            selected,
        })
    }

    pub fn up(&mut self) {
        self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
    }

    pub fn down(&mut self) {
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn chosen(&self) -> &str {
        &self.items[self.selected].value
    }

    /// The menu's rows, `width` cells wide at most.
    pub fn render(&self, width: usize) -> Vec<Line> {
        let first = self.selected.saturating_sub(MENU_ROWS - 1);
        let shown = &self.items[first..self.items.len().min(first + MENU_ROWS)];
        let label = |item: &MenuItem| match self.kind {
            MenuKind::Commands => item.value.clone(),
            // File names come off the user's disk: drawn plain, never interpreted.
            MenuKind::Paths { .. } => format!("@{}", sanitize(&item.value)),
        };
        let column = shown
            .iter()
            .map(|item| label(item).chars().count())
            .max()
            .unwrap_or(0);
        shown
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let name = label(item);
                let selected = first + index == self.selected;
                let mut line = if selected {
                    Line::styled(Style::BoldCyan, format!("  ❯ {name}"))
                } else {
                    Line::plain(format!("    {name}"))
                };
                if !item.description.is_empty() {
                    let pad = column - name.chars().count() + 2;
                    let room = width.saturating_sub(column + 6);
                    line = line.push(
                        Style::Dim,
                        format!("{}{}", " ".repeat(pad), ellipsize(&item.description, room)),
                    );
                }
                line.truncate(width)
            })
            .collect()
    }
}

/// The `@…` token the cursor sits in: the text after the `@`, and the token's
/// start and end as character indices. It runs from the whitespace before it to
/// the cursor, so completing mid-line never eats the text that follows.
pub fn mention_token(text: &str, cursor: usize) -> Option<(String, usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let token: String = chars[start..cursor].iter().collect();
    let prefix = token.strip_prefix('@')?;
    Some((prefix.to_owned(), start, cursor))
}

/// What a chosen path is written as: quoted when it has a space, because only a
/// quoted path with a space survives the mention pattern.
pub fn mention_text(path: &str) -> String {
    if path.contains(' ') {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(menu: &Menu) -> Vec<&str> {
        menu.items.iter().map(|item| item.value.as_str()).collect()
    }

    #[test]
    fn the_command_menu_follows_pythons_rule() {
        let menu = Menu::for_draft("/mo", 3, None).unwrap();
        assert_eq!(menu.kind, MenuKind::Commands);
        assert_eq!(values(&menu), ["/models", "/model", "/mode"]);
        assert_eq!(menu.chosen(), "/models");
        let typed_in_full = Menu::for_draft("/mode", 5, None).unwrap();
        assert_eq!(values(&typed_in_full), ["/models", "/model", "/mode"]);
        assert_eq!(typed_in_full.chosen(), "/mode");
        let config = Menu::for_draft("/config", 7, None).unwrap();
        assert_eq!(values(&config), ["/config", "/config model", "/config mcp"]);
        assert!(
            Menu::for_draft("/model x", 8, None).is_none(),
            "a space closes it"
        );
        assert!(Menu::for_draft("hello", 5, None).is_none());
        assert!(Menu::for_draft("/zzz", 4, None).is_none());
    }

    #[test]
    fn the_menu_wraps_and_scrolls() {
        let mut menu = Menu::for_draft("/", 1, None).unwrap();
        menu.up();
        assert_eq!(menu.chosen(), "/exit");
        menu.down();
        assert_eq!(menu.chosen(), "/help");
        let rows = menu.render(60);
        assert_eq!(rows.len(), MENU_ROWS);
        assert!(rows[0].plain_text().starts_with("  ❯ /help"));
        assert!(rows[0].plain_text().contains("Show this help"));
        for _ in 0..10 {
            menu.down();
        }
        let rows = menu.render(60);
        assert!(rows
            .last()
            .is_some_and(|row| row.plain_text().starts_with("  ❯ ")));
        assert!(rows.iter().all(|row| row.width() <= 60));
    }

    #[test]
    fn the_mention_token_ends_at_the_cursor() {
        assert_eq!(
            mention_token("look at @src/ma please", 15),
            Some(("src/ma".to_owned(), 8, 15))
        );
        assert_eq!(mention_token("@", 1), Some((String::new(), 0, 1)));
        assert_eq!(mention_token("mail a@b", 8), None);
        assert_eq!(mention_token("x @a b", 6), None);
        assert_eq!(mention_text("my file.txt"), "@\"my file.txt\"");
        assert_eq!(mention_text("src/"), "@src/");
    }
}
