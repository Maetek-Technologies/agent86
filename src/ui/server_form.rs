//! The "add a server" form of `/config model` (beyond Python): a vLLM or LM
//! Studio server anywhere, entered by kind, name, address, and an optional key,
//! so a remote server needs no hand edit of the config file.
//!
//! The form checks what it can on its own: the name (a config section and a
//! keyring account), the address (`http` or `https` with a host), a section of
//! another kind under the same name, and a stored key that would follow a
//! changed address to a new host, which must be confirmed or replaced. A key
//! that would cross the network over plain HTTP is warned about, not refused,
//! since LAN servers without TLS are common. The key is never drawn.

use super::panels::framed;
use super::text::{sanitize, wrap_words, Line};
use crate::provider::ServerKind;
use crate::repl::Style;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fmt;

/// Longest section name the form takes; it is also a keyring account.
const MAX_NAME: usize = 64;

/// What the form knows about one configured provider section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionInfo {
    pub name: String,
    pub kind: Option<ServerKind>,
    pub base_url: Option<String>,
    /// A key resolves for it now (the environment or the keyring).
    pub has_key: bool,
}

/// A completed form. `Debug` hides the key.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerDraft {
    pub kind: ServerKind,
    pub name: String,
    pub url: String,
    pub key: Option<String>,
    /// A key would cross the network unencrypted; the save panel repeats it.
    pub plain_http_key: bool,
}

impl fmt::Debug for ServerDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerDraft")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("url", &self.url)
            .field("key", &self.key.as_ref().map(|_| "***"))
            .field("plain_http_key", &self.plain_http_key)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Kind,
    Name,
    Url,
    Key,
    Confirm,
    Buttons,
}

/// The warning for a key sent over `http://` to another machine.
pub const PLAIN_HTTP_WARNING: &str = "the key would cross the network unencrypted \
     (http:// to a host that is not this machine); use https:// if the server offers it";

pub struct ServerForm {
    title: &'static str,
    kind: ServerKind,
    name: String,
    /// The name was typed, so switching the kind leaves it alone.
    name_touched: bool,
    url: String,
    key: String,
    /// The stored key may follow the changed address.
    confirm_key: bool,
    sections: Vec<SectionInfo>,
    focus: Field,
    /// 0 is Continue, 1 is Cancel.
    button: usize,
    current: Result<ServerDraft, String>,
}

impl ServerForm {
    /// An empty form for a new server, starting as vLLM.
    pub fn add(sections: Vec<SectionInfo>) -> Self {
        let mut form = Self {
            title: "Add a vLLM or LM Studio server",
            kind: ServerKind::Vllm,
            name: ServerKind::Vllm.name().to_owned(),
            name_touched: false,
            url: String::new(),
            key: String::new(),
            confirm_key: false,
            sections,
            focus: Field::Url,
            button: 0,
            current: Err(String::new()),
        };
        form.revalidate();
        form
    }

    /// The form for an existing server section, its address filled in to change.
    pub fn edit(section: &SectionInfo, kind: ServerKind, sections: Vec<SectionInfo>) -> Self {
        let mut form = Self::add(sections);
        form.title = "Change a server";
        form.kind = kind;
        form.name = section.name.clone();
        form.name_touched = true;
        form.url = section
            .base_url
            .clone()
            .unwrap_or_else(|| kind.default_base_url().to_owned());
        form.revalidate();
        form
    }

    fn fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Kind, Field::Name, Field::Url, Field::Key];
        if self.key_would_move() {
            fields.push(Field::Confirm);
        }
        fields.push(Field::Buttons);
        fields
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Field::Name => Some(&mut self.name),
            Field::Url => Some(&mut self.url),
            Field::Key => Some(&mut self.key),
            Field::Kind | Field::Confirm | Field::Buttons => None,
        }
    }

    fn move_focus(&mut self, step: isize) {
        let fields = self.fields();
        let at = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0) as isize;
        self.focus = fields[(at + step).rem_euclid(fields.len() as isize) as usize];
    }

    fn switch_kind(&mut self) {
        self.kind = match self.kind {
            ServerKind::Vllm => ServerKind::LmStudio,
            ServerKind::LmStudio => ServerKind::Vllm,
        };
        if !self.name_touched {
            self.name = self.kind.name().to_owned();
        }
    }

    /// The configured section under the typed name.
    fn section(&self) -> Option<&SectionInfo> {
        let name = self.name.trim();
        self.sections.iter().find(|section| section.name == name)
    }

    /// The section has a key, and the typed address is not the one it goes to now.
    fn key_would_move(&self) -> bool {
        let Some(section) = self.section() else {
            return false;
        };
        let current = section
            .base_url
            .clone()
            .or_else(|| section.kind.map(|kind| kind.default_base_url().to_owned()))
            .unwrap_or_default();
        section.has_key
            && self.key.trim().is_empty()
            && normalized(&current) != normalized(&self.url)
    }

    /// A key would go out: the typed one, or the section's own.
    fn sends_key(&self) -> bool {
        !self.key.trim().is_empty() || self.section().is_some_and(|section| section.has_key)
    }

    fn revalidate(&mut self) {
        self.current = self.build();
    }

    fn build(&self) -> Result<ServerDraft, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Enter a name for this server.".to_owned());
        }
        if name.chars().count() > MAX_NAME
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(format!(
                "A name is letters, digits, '-' and '_', at most {MAX_NAME} characters."
            ));
        }
        if let Some(section) = self.section() {
            if section.kind != Some(self.kind) {
                return Err(format!(
                    "A provider named '{name}' already exists and is not {} server. Pick \
                     another name.",
                    article(self.kind)
                ));
            }
        }
        let url = self.url.trim();
        let parsed = url::Url::parse(url)
            .ok()
            .filter(|parsed| matches!(parsed.scheme(), "http" | "https"))
            .filter(|parsed| parsed.host_str().is_some_and(|host| !host.is_empty()));
        let Some(parsed) = parsed else {
            return Err(format!(
                "Enter the server's address, such as {}.",
                example(self.kind)
            ));
        };
        if self.key_would_move() && !self.confirm_key {
            return Err(format!(
                "[providers.{name}] has a key, which would now go to this address. Type \
                 the key for this server, or confirm below that the key may follow."
            ));
        }
        let plain_http_key = parsed.scheme() == "http" && !is_loopback(&parsed) && self.sends_key();
        Ok(ServerDraft {
            kind: self.kind,
            name: name.to_owned(),
            url: url.trim_end_matches('/').to_owned(),
            key: Some(self.key.trim().to_owned()).filter(|key| !key.is_empty()),
            plain_http_key,
        })
    }

    /// `None` while open; then the completed server, or `None` for a cancel.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Option<ServerDraft>> {
        if key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
        {
            return Some(None);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (self.focus, key.code) {
            (_, KeyCode::Tab | KeyCode::Down) => self.move_focus(1),
            (_, KeyCode::BackTab | KeyCode::Up) => self.move_focus(-1),
            (Field::Kind, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) => {
                self.switch_kind();
            }
            (Field::Confirm, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) => {
                self.confirm_key = !self.confirm_key;
            }
            (Field::Buttons, KeyCode::Left | KeyCode::Right) => self.button = 1 - self.button,
            (Field::Buttons, KeyCode::Enter) if self.button == 1 => return Some(None),
            (_, KeyCode::Enter) => {
                if let Ok(draft) = &self.current {
                    return Some(Some(draft.clone()));
                }
            }
            (_, KeyCode::Backspace) => {
                if self.focus == Field::Name {
                    self.name_touched = true;
                }
                if let Some(text) = self.text_mut() {
                    text.pop();
                }
            }
            (_, KeyCode::Char('u')) if ctrl => {
                if let Some(text) = self.text_mut() {
                    text.clear();
                }
            }
            (_, KeyCode::Char(ch)) if !ctrl => {
                if self.focus == Field::Name {
                    self.name_touched = true;
                }
                if let Some(text) = self.text_mut() {
                    text.push(ch);
                }
            }
            _ => {}
        }
        // The confirmation goes away once nothing needs confirming.
        if !self.fields().contains(&self.focus) {
            self.focus = Field::Key;
        }
        if !self.key_would_move() {
            self.confirm_key = false;
        }
        self.revalidate();
        None
    }

    /// A paste goes into the focused field, on one line; control characters never
    /// reach the key.
    pub fn on_paste(&mut self, text: &str) {
        if self.focus == Field::Name {
            self.name_touched = true;
        }
        let key = self.focus == Field::Key;
        if let Some(field) = self.text_mut() {
            if key {
                field.extend(text.chars().filter(|ch| !ch.is_control()));
            } else {
                field.push_str(text.trim().replace(['\r', '\n'], " ").as_str());
            }
        }
        self.revalidate();
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let inner = width.saturating_sub(4).max(1);
        let mut rows = vec![Line::styled(Style::Bold, self.title), Line::default()];
        let lead =
            |field: Field| Line::styled(Style::Cyan, if self.focus == field { "❯ " } else { "  " });
        let (vllm, lmstudio) = match self.kind {
            ServerKind::Vllm => ("(•)", "( )"),
            ServerKind::LmStudio => ("( )", "(•)"),
        };
        rows.push(
            lead(Field::Kind)
                .push(Style::Dim, format!("{:<8} ", "kind"))
                .push(Style::Plain, format!("{vllm} vLLM  {lmstudio} LM Studio")),
        );
        let text = |field: Field, label: &str, value: &str, placeholder: &str| {
            let line = lead(field).push(Style::Dim, format!("{label:<8} "));
            if value.is_empty() {
                line.push(Style::Dim, placeholder.to_owned())
            } else {
                line.push(Style::Plain, sanitize(value))
            }
        };
        rows.push(text(Field::Name, "name", &self.name, "section name"));
        rows.push(text(Field::Url, "url", &self.url, example(self.kind)));
        let dots = "•".repeat(self.key.chars().count());
        rows.push(text(
            Field::Key,
            "key",
            &dots,
            "optional: only if the server requires one",
        ));
        if self.key_would_move() {
            let mark = if self.confirm_key { "[x]" } else { "[ ]" };
            rows.push(
                lead(Field::Confirm).push(Style::Plain, format!("{mark} send the stored key here")),
            );
        }
        rows.push(Line::default());
        match &self.current {
            Err(error) if !error.is_empty() => {
                rows.extend(
                    wrap_words(&sanitize(error), inner)
                        .into_iter()
                        .map(|row| Line::styled(Style::Red, row)),
                );
            }
            Ok(draft) if draft.plain_http_key => {
                rows.extend(
                    wrap_words(&format!("warning: {PLAIN_HTTP_WARNING}"), inner)
                        .into_iter()
                        .map(|row| Line::styled(Style::Yellow, row)),
                );
            }
            _ => rows.push(Line::default()),
        }
        let on_buttons = self.focus == Field::Buttons;
        let button = |index: usize, label: &str, enabled: bool| {
            let style = if !enabled {
                Style::Dim
            } else if on_buttons && self.button == index {
                Style::BoldCyan
            } else {
                Style::Plain
            };
            (style, format!("[ {label} ]"))
        };
        let (continue_style, continue_label) = button(0, "Continue", self.current.is_ok());
        let (cancel_style, cancel_label) = button(1, "Cancel", true);
        rows.push(
            lead(Field::Buttons)
                .push(continue_style, continue_label)
                .push(Style::Plain, "  ")
                .push(cancel_style, cancel_label),
        );
        framed(
            rows,
            width,
            "tab or ↑↓ between fields · space to choose · enter to continue · esc to cancel",
        )
    }
}

fn article(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::Vllm => "a vLLM",
        ServerKind::LmStudio => "an LM Studio",
    }
}

fn example(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::Vllm => "http://gpu-box.lan:8000/v1",
        ServerKind::LmStudio => "http://studio.lan:1234/v1",
    }
}

fn normalized(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

/// `localhost`, or an address on the loopback interface.
fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(form: &mut ServerForm, text: &str) {
        for ch in text.chars() {
            assert!(form.on_key(key(KeyCode::Char(ch))).is_none());
        }
    }

    fn clear(form: &mut ServerForm) {
        form.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    }

    /// Move focus to `field` with Tab.
    fn focus(form: &mut ServerForm, field: Field) {
        for _ in 0..8 {
            if form.focus == field {
                return;
            }
            form.on_key(key(KeyCode::Tab));
        }
        panic!("{field:?} not reachable");
    }

    fn sections() -> Vec<SectionInfo> {
        vec![
            SectionInfo {
                name: "openai".into(),
                kind: None,
                base_url: Some("https://api.openai.com/v1".into()),
                has_key: true,
            },
            SectionInfo {
                name: "vllm".into(),
                kind: Some(ServerKind::Vllm),
                base_url: Some("http://localhost:8000/v1".into()),
                has_key: false,
            },
            SectionInfo {
                name: "gpu-box".into(),
                kind: Some(ServerKind::Vllm),
                base_url: Some("http://gpu-box.lan:8000/v1".into()),
                has_key: true,
            },
        ]
    }

    fn text(form: &ServerForm) -> String {
        form.render(100)
            .iter()
            .map(Line::plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_new_server_needs_an_address_and_a_free_or_matching_name() {
        let mut form = ServerForm::add(sections());
        assert!(form.current.is_err(), "no address yet");
        typed(&mut form, "http://10.0.0.5:8000/v1/");
        let draft = form.current.clone().unwrap();
        assert_eq!(
            (draft.kind, draft.name.as_str(), draft.url.as_str()),
            (ServerKind::Vllm, "vllm", "http://10.0.0.5:8000/v1")
        );
        assert_eq!(draft.key, None);
        assert!(!draft.plain_http_key, "no key goes out");

        // The kind switches the untouched name with it.
        focus(&mut form, Field::Kind);
        form.on_key(key(KeyCode::Char(' ')));
        assert_eq!(form.current.clone().unwrap().name, "lmstudio");

        // A section of another kind under the name is refused; bad names too.
        focus(&mut form, Field::Name);
        clear(&mut form);
        typed(&mut form, "openai");
        assert!(form
            .current
            .clone()
            .unwrap_err()
            .contains("not an LM Studio server"));
        clear(&mut form);
        typed(&mut form, "gpu box");
        assert!(form
            .current
            .clone()
            .unwrap_err()
            .contains("letters, digits"));

        // Not a web address.
        clear(&mut form);
        typed(&mut form, "studio");
        focus(&mut form, Field::Url);
        clear(&mut form);
        typed(&mut form, "ftp://host/v1");
        assert!(form.current.clone().unwrap_err().contains("such as"));
    }

    #[test]
    fn the_key_is_never_drawn_and_never_in_debug() {
        let mut form = ServerForm::add(sections());
        typed(&mut form, "https://studio.example/v1");
        focus(&mut form, Field::Key);
        typed(&mut form, "sk-secret-value");
        let drawn = text(&form);
        assert!(!drawn.contains("sk-secret-value"));
        assert!(drawn.contains(&"•".repeat(15)));
        let draft = form.current.clone().unwrap();
        assert_eq!(draft.key.as_deref(), Some("sk-secret-value"));
        assert!(!format!("{draft:?}").contains("sk-secret-value"));
    }

    #[test]
    fn a_key_over_plain_http_to_another_machine_is_warned_about() {
        let mut form = ServerForm::add(sections());
        typed(&mut form, "http://10.0.0.5:8000/v1");
        focus(&mut form, Field::Key);
        typed(&mut form, "k");
        assert!(form.current.clone().unwrap().plain_http_key);
        assert!(text(&form).contains("unencrypted"));

        // Loopback and https are fine.
        for url in [
            "http://localhost:8000/v1",
            "http://127.0.0.1:8000/v1",
            "https://10.0.0.5/v1",
        ] {
            focus(&mut form, Field::Url);
            clear(&mut form);
            typed(&mut form, url);
            assert!(!form.current.clone().unwrap().plain_http_key, "{url}");
        }
    }

    #[test]
    fn a_stored_key_follows_a_new_address_only_when_confirmed_or_replaced() {
        let gpu = sections()[2].clone();
        let mut form = ServerForm::edit(&gpu, ServerKind::Vllm, sections());
        // Unchanged: fine, and the stored key going over http to the LAN is warned.
        assert!(form.current.clone().unwrap().plain_http_key);
        focus(&mut form, Field::Url);
        clear(&mut form);
        typed(&mut form, "https://new-host.example/v1");
        let error = form.current.clone().unwrap_err();
        assert!(error.contains("[providers.gpu-box] has a key"), "{error}");
        assert!(text(&form).contains("[ ] send the stored key here"));

        focus(&mut form, Field::Confirm);
        form.on_key(key(KeyCode::Char(' ')));
        assert_eq!(
            form.current.clone().unwrap().url,
            "https://new-host.example/v1"
        );

        // Or a new key, which needs no confirmation.
        let mut form = ServerForm::edit(&gpu, ServerKind::Vllm, sections());
        focus(&mut form, Field::Url);
        clear(&mut form);
        typed(&mut form, "https://new-host.example/v1");
        focus(&mut form, Field::Key);
        typed(&mut form, "fresh-key");
        assert_eq!(
            form.current.clone().unwrap().key.as_deref(),
            Some("fresh-key")
        );
    }

    #[test]
    fn escape_cancels_and_enter_submits_only_a_valid_form() {
        let mut form = ServerForm::add(sections());
        assert_eq!(
            form.on_key(key(KeyCode::Enter)),
            None,
            "nothing to submit yet"
        );
        typed(&mut form, "http://localhost:8000");
        let Some(Some(draft)) = form.on_key(key(KeyCode::Enter)) else {
            panic!("a valid form submits");
        };
        assert_eq!(draft.url, "http://localhost:8000");
        let mut form = ServerForm::add(sections());
        assert_eq!(form.on_key(key(KeyCode::Esc)), Some(None));
    }
}
