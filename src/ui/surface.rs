//! agent86UI in the interface (beyond Python; ADR 0005, slice 5): the surfaces
//! the model drew with `render_ui`, kept on the interface's own headless
//! renderer and drawn as text.
//!
//! The interface is a renderer like an embedder's: it applies each validated
//! `TurnEvent::Ui` message to its own [`Runtime`], so what the user types and
//! presses never touches the agent's copy (`UiSession`). It draws the basic
//! catalog's interactive subset: Text, Row, Column, Card, List, Divider,
//! Button, TextField, CheckBox, ChoicePicker, Slider (as a number),
//! DateTimeInput (as text), and media as a link. Tabs and Modal get one line
//! saying they are not drawn here yet.
//!
//! A surface is a question to the user, so the display levels never hide it,
//! and it is drawn in a frame of its own (magenta, heavy lines, naming the
//! agent), never in the style of an approval or one of agent86's panels.

use super::text::{sanitize, wrap_words, Line};
use crate::repl::Style;
use crate::sdk::agent86_ui::runtime::{Node, Runtime};
use crate::sdk::{AgentMessage, AgentMessageKind, Catalog};
use serde_json::Value;

/// What the frame's title says, before the surface id.
pub const TITLE: &str = "agent86UI · drawn by the agent";

/// The components a user can act on, which take the focus in turn.
#[allow(dead_code)] // The focus arrives with slice 5c.
const FOCUSABLE: &[&str] = &[
    "Button",
    "TextField",
    "CheckBox",
    "ChoicePicker",
    "Slider",
    "DateTimeInput",
];

/// Narrowest cell a Row gives a child before it stacks them instead.
const MIN_COLUMN: usize = 12;

/// Where the focus is: a component instance (a template gives one per item,
/// told apart by its data path) and, in a ChoicePicker, the option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub component_id: String,
    pub data_path: String,
    pub option: usize,
}

impl Focus {
    #[allow(dead_code)] // The focus arrives with slice 5c.
    pub fn on(node: &Node) -> Self {
        Self {
            component_id: node.id.clone(),
            data_path: node.data_path.clone(),
            option: 0,
        }
    }

    fn is(&self, node: &Node) -> bool {
        self.component_id == node.id && self.data_path == node.data_path
    }
}

/// A surface drawn: its rows, and the row the focus is on, so a view cut to
/// fit the screen can keep it in sight.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Drawn {
    pub lines: Vec<Line>,
    pub focus_row: Option<usize>,
}

/// The interface's surfaces.
pub struct Surfaces {
    runtime: Runtime,
    /// The surfaces up, oldest first; the last is the one drawn live.
    order: Vec<String>,
}

impl Default for Surfaces {
    fn default() -> Self {
        Self::new()
    }
}

impl Surfaces {
    /// A renderer for the basic catalog, under the rules `render_ui` checks
    /// batches with, so whatever the agent accepted applies here too.
    pub fn new() -> Self {
        let mut runtime =
            Runtime::new([Catalog::basic().clone()]).expect("the basic catalog compiles");
        runtime.set_rules(crate::agent86ui::RULES);
        Self {
            runtime,
            order: Vec::new(),
        }
    }

    /// The surfaces up, oldest first.
    pub fn ids(&self) -> &[String] {
        &self.order
    }

    /// The surface drawn live: the newest one up.
    pub fn current(&self) -> Option<&str> {
        self.order.last().map(String::as_str)
    }

    /// Apply one validated message. Returns the surface that leaves the live
    /// slot because of it (deleted, or another surface became the newest),
    /// drawn as it last was, for its static copy in scrollback.
    pub fn apply(&mut self, message: &AgentMessage, width: usize) -> Option<Drawn> {
        let Ok(value) = serde_json::to_value(message) else {
            return None;
        };
        let before = self.current().map(String::from);
        let target = surface_of(message);
        // Drawn now: once deleted, there is nothing left to draw.
        let leaving_copy = before.as_deref().map(|id| self.draw(id, width, None));
        if !self.runtime.apply(&[value]).is_ok() {
            return None;
        }
        if let Some(target) = target {
            self.order.retain(|id| id != &target);
            if matches!(message.kind, AgentMessageKind::DeleteSurface(_)) {
                // Nothing to promote: the next newest is drawn live from now on.
            } else {
                self.order.push(target);
            }
        }
        // Keep only what the runtime still holds.
        let up: Vec<String> = self.runtime.surface_ids().map(String::from).collect();
        self.order.retain(|id| up.contains(id));
        let after = self.current().map(String::from);
        match (before, leaving_copy) {
            (Some(before), Some(copy)) if after.as_deref() != Some(before.as_str()) => Some(copy),
            _ => None,
        }
    }

    /// The surface `id`, drawn `width` cells wide in its frame, with `focus`
    /// marked.
    pub fn draw(&self, id: &str, width: usize, focus: Option<&Focus>) -> Drawn {
        let inner = width.saturating_sub(4).max(1);
        let mut body = Body::default();
        match self.runtime.tree(id) {
            Some(root) => draw_node(&root, inner, focus, &mut body),
            None => body.push(Line::styled(Style::Dim, "(nothing to show)"), false),
        }
        frame(id, body, width)
    }

    /// The component instances that take the focus, in reading order.
    #[allow(dead_code)] // The focus arrives with slice 5c.
    pub fn focusable(&self, id: &str) -> Vec<Focus> {
        self.runtime
            .tree(id)
            .map(|root| {
                root.walk()
                    .into_iter()
                    .filter(|node| FOCUSABLE.contains(&node.component.as_str()))
                    .map(Focus::on)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The surface a message is about.
fn surface_of(message: &AgentMessage) -> Option<String> {
    let value = serde_json::to_value(&message.kind).ok()?;
    value
        .as_object()?
        .values()
        .next()?
        .get("surfaceId")?
        .as_str()
        .map(String::from)
}

/// The rows inside a frame, and which of them holds the focus.
#[derive(Default)]
struct Body {
    lines: Vec<Line>,
    focus_row: Option<usize>,
}

impl Body {
    fn push(&mut self, line: Line, focused: bool) {
        if focused && self.focus_row.is_none() {
            self.focus_row = Some(self.lines.len());
        }
        self.lines.push(line);
    }
}

/// The surface's frame: heavy magenta lines, the title naming the agent and
/// the surface on the top border.
fn frame(id: &str, body: Body, width: usize) -> Drawn {
    let border = Style::Magenta;
    let width = width.max(8);
    let inner = width - 4;
    let id = sanitize(id).replace('\n', " ");
    let room = width.saturating_sub(4);
    // A narrow frame drops "drawn by the agent" before it cuts the surface id.
    let mut title = Line::styled(Style::BoldMagenta, format!(" {TITLE} · {id} "));
    if title.width() > room {
        title = Line::styled(Style::BoldMagenta, format!(" agent86UI · {id} "));
    }
    let title = title.truncate(room);
    let fill = width.saturating_sub(3 + title.width());
    let mut top = Line::styled(border, "┏━");
    top.spans.extend(title.spans);
    let mut lines = vec![top.push(border, format!("{}┓", "━".repeat(fill)))];
    for row in body.lines {
        let row = row.truncate(inner);
        let pad = inner.saturating_sub(row.width());
        let mut line = Line::styled(border, "┃ ");
        line.spans.extend(row.spans);
        lines.push(line.push(Style::Plain, " ".repeat(pad)).push(border, " ┃"));
    }
    lines.push(Line::styled(
        border,
        format!("┗{}┛", "━".repeat(width.saturating_sub(2))),
    ));
    Drawn {
        lines,
        focus_row: body.focus_row.map(|row| row + 1),
    }
}

/// A property as text, sanitized: a string as it is, a number or a boolean as
/// JSON writes it, anything else (or nothing) empty.
fn text_of(node: &Node, key: &str) -> String {
    match node.properties.get(key) {
        Some(Value::String(text)) => sanitize(text),
        Some(Value::Null) | None => String::new(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(other) => sanitize(&other.to_string()),
    }
}

/// Text wrapped to `width`, each row in `style`.
fn wrapped(text: &str, width: usize, style: Style) -> Vec<Line> {
    text.split('\n')
        .flat_map(|paragraph| wrap_words(paragraph, width))
        .map(|row| Line::styled(style, row))
        .collect()
}

/// The children a node holds under `key`.
fn children<'a>(node: &'a Node, key: &str) -> &'a [Node] {
    node.children
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, nodes)| nodes.as_slice())
        .unwrap_or_default()
}

/// The text a Button shows: its child's text, else its id.
fn button_label(node: &Node) -> String {
    let label: Vec<String> = children(node, "child")
        .iter()
        .flat_map(|child| child.walk())
        .filter(|n| n.component == "Text")
        .map(|n| text_of(n, "text"))
        .collect();
    let label = label.join(" ").replace('\n', " ");
    if label.trim().is_empty() {
        sanitize(&node.id)
    } else {
        label
    }
}

/// The style of a field's box: magenta when focused.
fn field_style(focused: bool) -> Style {
    if focused {
        Style::BoldMagenta
    } else {
        Style::Plain
    }
}

/// The marker in front of the focused input.
fn lead(focused: bool) -> Line {
    if focused {
        Line::styled(Style::BoldMagenta, "› ")
    } else {
        Line::plain("  ")
    }
}

fn draw_node(node: &Node, width: usize, focus: Option<&Focus>, out: &mut Body) {
    let focused = focus.is_some_and(|focus| focus.is(node));
    if node.placeholder {
        out.push(Line::styled(Style::Dim, "…"), false);
        return;
    }
    match node.component.as_str() {
        "Text" => {
            let style = match node.properties.get("variant").and_then(Value::as_str) {
                Some("caption") => Style::Dim,
                _ => Style::Plain,
            };
            for line in wrapped(&text_of(node, "text"), width, style) {
                out.push(line, false);
            }
        }
        "Column" => {
            for child in children(node, "children") {
                draw_node(child, width, focus, out);
            }
        }
        "List" => {
            let horizontal =
                node.properties.get("direction").and_then(Value::as_str) == Some("horizontal");
            if horizontal {
                draw_row(children(node, "children"), width, focus, out);
            } else {
                for child in children(node, "children") {
                    draw_node(child, width, focus, out);
                }
            }
        }
        "Row" => draw_row(children(node, "children"), width, focus, out),
        "Card" => {
            let mut inner = Body::default();
            for child in children(node, "child") {
                draw_node(child, width.saturating_sub(4).max(1), focus, &mut inner);
            }
            let border = Style::Dim;
            out.push(
                Line::styled(border, format!("╭{}╮", "─".repeat(width.saturating_sub(2)))),
                false,
            );
            let offset = out.lines.len();
            if out.focus_row.is_none() {
                out.focus_row = inner.focus_row.map(|row| row + offset);
            }
            let room = width.saturating_sub(4).max(1);
            for row in inner.lines {
                let row = row.truncate(room);
                let pad = room.saturating_sub(row.width());
                let mut line = Line::styled(border, "│ ");
                line.spans.extend(row.spans);
                out.lines
                    .push(line.push(Style::Plain, " ".repeat(pad)).push(border, " │"));
            }
            out.push(
                Line::styled(border, format!("╰{}╯", "─".repeat(width.saturating_sub(2)))),
                false,
            );
        }
        "Divider" => {
            let vertical = node.properties.get("axis").and_then(Value::as_str) == Some("vertical");
            let line = if vertical {
                "│".to_owned()
            } else {
                "─".repeat(width)
            };
            out.push(Line::styled(Style::Dim, line), false);
        }
        "Button" => {
            let label = button_label(node);
            let style = if !node.is_valid() {
                Style::Dim
            } else if focused {
                Style::BoldMagenta
            } else if node.properties.get("variant").and_then(Value::as_str) == Some("primary") {
                Style::Bold
            } else {
                Style::Plain
            };
            let text =
                if node.properties.get("variant").and_then(Value::as_str) == Some("borderless") {
                    label
                } else {
                    format!("[ {label} ]")
                };
            out.push(lead(focused).push(style, text), focused);
        }
        "TextField" | "DateTimeInput" => {
            let label = text_of(node, "label");
            if !label.is_empty() {
                for line in wrapped(&label, width, Style::Dim) {
                    out.push(line, false);
                }
            }
            let value = text_of(node, "value");
            let shown = if value.is_empty() && !focused {
                Line::styled(Style::Dim, text_of(node, "placeholder"))
            } else {
                Line::styled(field_style(focused), value.replace('\n', " ⏎ "))
            };
            let room = width.saturating_sub(6).max(1);
            let caret = if focused { "▏" } else { "" };
            let shown = shown.push(Style::BoldMagenta, caret).truncate(room);
            let pad = room.saturating_sub(shown.width());
            let mut line = lead(focused).push(field_style(focused), "[ ");
            line.spans.extend(shown.spans);
            out.push(
                line.push(Style::Plain, " ".repeat(pad))
                    .push(field_style(focused), " ]"),
                focused,
            );
            failures(node, width, out);
        }
        "CheckBox" => {
            let checked = node
                .properties
                .get("value")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mark = if checked { "[x] " } else { "[ ] " };
            out.push(
                lead(focused)
                    .push(field_style(focused), mark)
                    .push(Style::Plain, text_of(node, "label")),
                focused,
            );
            failures(node, width, out);
        }
        "ChoicePicker" => {
            let label = text_of(node, "label");
            if !label.is_empty() {
                for line in wrapped(&label, width, Style::Dim) {
                    out.push(line, false);
                }
            }
            let single =
                node.properties.get("variant").and_then(Value::as_str) != Some("multipleSelection");
            let chosen: Vec<String> = match node.properties.get("value") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect(),
                Some(Value::String(one)) => vec![one.clone()],
                _ => Vec::new(),
            };
            let options = node
                .properties
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (index, option) in options.iter().enumerate() {
                let value = option.get("value").and_then(Value::as_str).unwrap_or("");
                let on = chosen.iter().any(|c| c == value);
                let mark = match (single, on) {
                    (true, true) => "(•) ",
                    (true, false) => "( ) ",
                    (false, true) => "[x] ",
                    (false, false) => "[ ] ",
                };
                let here = focused && focus.is_some_and(|f| f.option == index);
                let text = option
                    .get("label")
                    .map(|label| match label {
                        Value::String(text) => sanitize(text),
                        other => sanitize(&other.to_string()),
                    })
                    .unwrap_or_else(|| sanitize(value));
                out.push(
                    lead(here)
                        .push(field_style(here), mark)
                        .push(Style::Plain, text),
                    here,
                );
            }
            failures(node, width, out);
        }
        "Slider" => {
            let number = |key: &str, default: f64| {
                node.properties
                    .get(key)
                    .and_then(Value::as_f64)
                    .unwrap_or(default)
            };
            let (min, max) = (number("min", 0.0), number("max", 100.0));
            let value = number("value", min);
            let label = text_of(node, "label");
            let mut line = lead(focused);
            if !label.is_empty() {
                line = line.push(Style::Plain, format!("{label}  "));
            }
            out.push(
                line.push(field_style(focused), format!("◀ {} ▶", show_number(value)))
                    .push(
                        Style::Dim,
                        format!("  ({} to {})", show_number(min), show_number(max)),
                    ),
                focused,
            );
            failures(node, width, out);
        }
        kind @ ("Image" | "Video" | "AudioPlayer") => {
            let what = match kind {
                "Image" => "image",
                "Video" => "video",
                _ => "audio",
            };
            let description = text_of(node, "description");
            let url = text_of(node, "url");
            let mut text = format!("[{what}]");
            if !description.is_empty() {
                text.push(' ');
                text.push_str(&description);
            }
            for line in wrapped(&text, width, Style::Dim) {
                out.push(line, false);
            }
            if !url.is_empty() && !url.starts_with("data:") {
                for line in wrapped(&url, width, Style::Cyan) {
                    out.push(line, false);
                }
            }
        }
        "Icon" => {
            let name = text_of(node, "name");
            out.push(Line::styled(Style::Dim, format!("[{name}]")), false);
        }
        other => {
            let name = if other.is_empty() { "?" } else { other };
            out.push(
                Line::styled(
                    Style::Dim,
                    format!("({} is not drawn in the terminal yet)", sanitize(name)),
                ),
                false,
            );
        }
    }
    for error in &node.errors {
        out.push(
            Line::styled(Style::Red, format!("! {}", sanitize(error))),
            false,
        );
    }
}

/// A number as a person writes it: no `.0` on a whole one.
fn show_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The messages of a node's failing checks, under it, in red.
fn failures(node: &Node, width: usize, out: &mut Body) {
    for message in node.validation_errors() {
        for row in wrap_words(&sanitize(&message), width.saturating_sub(2).max(1)) {
            out.push(Line::plain("  ").push(Style::Red, row), false);
        }
    }
}

/// Children side by side, each in a share of the width by its `weight`; when a
/// share would be too narrow, they stack instead.
fn draw_row(nodes: &[Node], width: usize, focus: Option<&Focus>, out: &mut Body) {
    if nodes.is_empty() {
        return;
    }
    let gaps = nodes.len() - 1;
    let room = width.saturating_sub(gaps * 2);
    if room / nodes.len() < MIN_COLUMN {
        for node in nodes {
            draw_node(node, width, focus, out);
        }
        return;
    }
    let weights: Vec<f64> = nodes
        .iter()
        .map(|node| {
            node.properties
                .get("weight")
                .and_then(Value::as_f64)
                .filter(|w| *w > 0.0)
                .unwrap_or(1.0)
        })
        .collect();
    let total: f64 = weights.iter().sum();
    let mut widths: Vec<usize> = weights
        .iter()
        .map(|w| ((room as f64) * w / total).floor() as usize)
        .collect();
    let used: usize = widths.iter().sum();
    if let Some(last) = widths.last_mut() {
        *last += room.saturating_sub(used);
    }
    let columns: Vec<Body> = nodes
        .iter()
        .zip(&widths)
        .map(|(node, width)| {
            let mut body = Body::default();
            draw_node(node, *width, focus, &mut body);
            body
        })
        .collect();
    let height = columns.iter().map(|c| c.lines.len()).max().unwrap_or(0);
    let offset = out.lines.len();
    if out.focus_row.is_none() {
        out.focus_row = columns
            .iter()
            .find_map(|c| c.focus_row)
            .map(|row| row + offset);
    }
    let mut merged = Body::default();
    for row in 0..height {
        let mut line = Line::default();
        for (index, (column, width)) in columns.iter().zip(&widths).enumerate() {
            if index > 0 {
                line = line.push(Style::Plain, "  ");
            }
            let cell = column
                .lines
                .get(row)
                .map(|l| l.truncate(*width))
                .unwrap_or_default();
            let pad = width.saturating_sub(cell.width());
            line.spans.extend(cell.spans);
            line = line.push(Style::Plain, " ".repeat(pad));
        }
        merged.lines.push(line);
    }
    out.lines.extend(merged.lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(value: Value) -> AgentMessage {
        serde_json::from_value(value).unwrap()
    }

    fn create(id: &str, components: Value, data: Value) -> AgentMessage {
        message(serde_json::json!({"version": "1.0", "createSurface": {
            "surfaceId": id,
            "catalogId": Catalog::basic().id(),
            "components": components,
            "dataModel": data,
        }}))
    }

    fn text(drawn: &Drawn) -> Vec<String> {
        drawn
            .lines
            .iter()
            .map(|line| line.plain_text().trim_end().to_owned())
            .collect()
    }

    fn form() -> AgentMessage {
        create(
            "plan",
            serde_json::json!([
                {"id": "root", "component": "Column",
                 "children": ["title", "name", "plan", "weekly", "rating", "buttons"]},
                {"id": "title", "component": "Text", "text": "Pick a plan"},
                {"id": "name", "component": "TextField", "label": "Name",
                 "value": {"@path": "/name"}, "placeholder": "Your name"},
                {"id": "plan", "component": "ChoicePicker", "label": "Plan",
                 "variant": "mutuallyExclusive", "value": {"@path": "/plan"},
                 "options": [{"label": "Basic", "value": "basic"},
                             {"label": "Pro", "value": "pro"}]},
                {"id": "weekly", "component": "CheckBox", "label": "Repeats weekly",
                 "value": {"@path": "/weekly"}},
                {"id": "rating", "component": "Slider", "label": "Rating",
                 "min": 1, "max": 5, "value": {"@path": "/rating"}},
                {"id": "buttons", "component": "Row", "children": ["ok", "cancel"]},
                {"id": "ok", "component": "Button", "child": "ok_label", "variant": "primary",
                 "action": {"event": {"name": "submit"}}},
                {"id": "ok_label", "component": "Text", "text": "Submit"},
                {"id": "cancel", "component": "Button", "child": "cancel_label",
                 "action": {"event": {"name": "cancel"}}},
                {"id": "cancel_label", "component": "Text", "text": "Cancel"}
            ]),
            serde_json::json!({"name": "", "plan": ["pro"], "weekly": true, "rating": 3}),
        )
    }

    #[test]
    fn a_form_is_drawn_in_its_own_frame() {
        let mut surfaces = Surfaces::new();
        assert_eq!(surfaces.apply(&form(), 60), None);
        assert_eq!(surfaces.current(), Some("plan"));
        let drawn = surfaces.draw("plan", 60, None);
        let rows = text(&drawn);
        assert!(
            rows[0].starts_with("┏━ agent86UI · drawn by the agent · plan ━"),
            "{rows:#?}"
        );
        assert!(rows[0].ends_with('┓'));
        assert!(rows.last().unwrap().starts_with("┗━"));
        let body: Vec<String> = rows[1..rows.len() - 1]
            .iter()
            .map(|row| {
                row.trim_start_matches("┃ ")
                    .trim_end_matches('┃')
                    .trim_end()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            body,
            [
                "Pick a plan",
                "Name",
                "  [ Your name                                          ]",
                "Plan",
                "  ( ) Basic",
                "  (•) Pro",
                "  [x] Repeats weekly",
                "  Rating  ◀ 3 ▶  (1 to 5)",
                "  [ Submit ]                   [ Cancel ]",
            ]
        );
        assert!(drawn.lines.iter().all(|line| line.width() == 60));
        // The frame is never the approval panel's (cyan) or bold red.
        assert_eq!(drawn.lines[0].spans[0].0, Style::Magenta);
    }

    #[test]
    fn the_focus_is_marked_and_its_row_reported() {
        let mut surfaces = Surfaces::new();
        surfaces.apply(&form(), 60);
        let focusable = surfaces.focusable("plan");
        let ids: Vec<&str> = focusable.iter().map(|f| f.component_id.as_str()).collect();
        assert_eq!(ids, ["name", "plan", "weekly", "rating", "ok", "cancel"]);

        let mut focus = focusable[1].clone();
        focus.option = 1;
        let drawn = surfaces.draw("plan", 60, Some(&focus));
        let row = drawn.focus_row.unwrap();
        assert!(
            text(&drawn)[row].contains("› (•) Pro"),
            "{:#?}",
            text(&drawn)
        );

        let drawn = surfaces.draw("plan", 60, Some(&focusable[0]));
        let row = drawn.focus_row.unwrap();
        assert!(text(&drawn)[row].contains("› [ ▏"), "{:#?}", text(&drawn));
    }

    #[test]
    fn a_narrow_frame_keeps_the_surface_id() {
        let mut surfaces = Surfaces::new();
        surfaces.apply(&form(), 30);
        let rows = text(&surfaces.draw("plan", 30, None));
        assert!(rows[0].starts_with("┏━ agent86UI · plan ━"), "{rows:#?}");
    }

    #[test]
    fn a_narrow_row_stacks_and_every_row_fits() {
        let mut surfaces = Surfaces::new();
        surfaces.apply(&form(), 24);
        let drawn = surfaces.draw("plan", 24, None);
        assert!(drawn.lines.iter().all(|line| line.width() == 24));
        let rows = text(&drawn);
        assert!(rows.iter().any(|row| row.contains("[ Submit ]")));
        assert!(rows.iter().any(|row| row.contains("[ Cancel ]")));
    }

    #[test]
    fn a_surface_leaving_the_live_slot_gives_its_last_state() {
        let mut surfaces = Surfaces::new();
        surfaces.apply(&form(), 60);
        let second = create(
            "note",
            serde_json::json!([{"id": "root", "component": "Text", "text": "Done"}]),
            serde_json::json!({}),
        );
        let copy = surfaces.apply(&second, 60).expect("the form's copy");
        assert!(text(&copy).iter().any(|row| row.contains("Pick a plan")));
        assert_eq!(surfaces.ids(), ["plan", "note"]);
        assert_eq!(surfaces.current(), Some("note"));

        // Updating the live surface keeps it live: no copy.
        let update = message(serde_json::json!({"version": "1.0", "updateComponents": {
            "surfaceId": "note",
            "components": [{"id": "root", "component": "Text", "text": "Done, really"}]
        }}));
        assert_eq!(surfaces.apply(&update, 60), None);

        let delete = message(serde_json::json!({"version": "1.0",
            "deleteSurface": {"surfaceId": "note"}}));
        let copy = surfaces.apply(&delete, 60).expect("the note's copy");
        assert!(text(&copy).iter().any(|row| row.contains("Done, really")));
        assert_eq!(surfaces.current(), Some("plan"));
    }

    #[test]
    fn media_is_a_link_and_tabs_say_they_are_not_drawn() {
        let mut surfaces = Surfaces::new();
        surfaces.apply(
            &create(
                "m",
                serde_json::json!([
                    {"id": "root", "component": "Column", "children": ["pic", "tabs"]},
                    {"id": "pic", "component": "Image", "url": "docs/logo.png",
                     "description": "The logo"},
                    {"id": "tabs", "component": "Tabs",
                     "tabs": [{"title": "One", "child": "one"}]},
                    {"id": "one", "component": "Text", "text": "first"}
                ]),
                serde_json::json!({}),
            ),
            50,
        );
        let rows = text(&surfaces.draw("m", 50, None));
        assert!(rows.iter().any(|row| row.contains("[image] The logo")));
        assert!(rows.iter().any(|row| row.contains("docs/logo.png")));
        assert!(rows
            .iter()
            .any(|row| row.contains("(Tabs is not drawn in the terminal yet)")));
    }

    #[test]
    fn untrusted_text_cannot_reach_the_terminal() {
        let mut surfaces = Surfaces::new();
        surfaces.apply(
            &create(
                "x\u{1b}[2J",
                serde_json::json!([{"id": "root", "component": "Text",
                                    "text": "hi\u{1b}]0;owned\u{7}there"}]),
                serde_json::json!({}),
            ),
            40,
        );
        let id = surfaces.current().unwrap().to_owned();
        let drawn = surfaces.draw(&id, 40, None);
        for line in &drawn.lines {
            for (_, span) in &line.spans {
                assert!(!span.chars().any(|c| c.is_control()), "{span:?}");
            }
        }
    }
}
