//! Secrets the user hands the agent without the model ever seeing them, beyond
//! Python.
//!
//! A developer often has to give an agent a key, a token, or a password. Typed into
//! the chat, it would go to the provider and into the scrollback, the transcript
//! view, `state_json`, episodes (recalled into later tasks), and the trace. Instead
//! the user enters it in a masked prompt (`request_secret`, or `/secret NAME`), and
//! the harness keeps it here. The model only ever sees its name: the value reaches
//! `run_command` and `python_exec` as an environment variable, and wherever it shows
//! up in text the model or the trace would get (tool results, the user's own input,
//! the model's reply), it is replaced by [`placeholder`].
//!
//! This keeps secrets out of transcripts, provider logs, traces, and memory. It is
//! no defence against a hostile model, which could still get a transformed copy
//! (base64, part of the value) past literal redaction.

use serde_json::Value;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

/// What the user is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequest {
    /// The environment variable name the value will be set as.
    pub name: String,
    /// Why the agent wants it, as the model wrote it.
    pub reason: String,
    /// Whether a value is already held under `name` (entering one replaces it).
    pub replacing: bool,
}

/// Asks the user for one secret: the value, or `None` when they declined.
pub type SecretPrompt = Box<dyn Fn(&SecretRequest) -> Option<String> + Send + Sync>;

/// The session's secrets, by name, and the prompt that asks for one. Clones share
/// both, so the tool context, its sub-agent forks, and the recorder see one vault.
#[derive(Clone, Default)]
pub struct SecretVault {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// In the order they were entered.
    values: RwLock<Vec<(String, String)>>,
    prompt: Mutex<Option<Arc<SecretPrompt>>>,
}

impl std::fmt::Debug for SecretVault {
    /// Names only: a vault printed in a panic or a log must not leak a value.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretVault")
            .field("names", &self.names())
            .finish()
    }
}

/// What a value is replaced by in text the model or the trace sees.
pub fn placeholder(name: &str) -> String {
    format!("${{secret:{name}}}")
}

/// The pattern a secret's name must match: an environment variable name every
/// shell accepts.
pub const NAME_PATTERN: &str = "^[A-Za-z_][A-Za-z0-9_]*$";

/// Whether `name` matches [`NAME_PATTERN`].
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

impl SecretVault {
    fn values(&self) -> std::sync::RwLockReadGuard<'_, Vec<(String, String)>> {
        self.inner
            .values
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn values_mut(&self) -> std::sync::RwLockWriteGuard<'_, Vec<(String, String)>> {
        self.inner
            .values
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Hold `value` under `name`, replacing any value already there.
    pub fn set(&self, name: &str, value: String) {
        let mut values = self.values_mut();
        match values.iter_mut().find(|(known, _)| known == name) {
            Some(entry) => entry.1 = value,
            None => values.push((name.to_owned(), value)),
        }
    }

    /// Drop the value held under `name`; false if there was none.
    pub fn remove(&self, name: &str) -> bool {
        let mut values = self.values_mut();
        let before = values.len();
        values.retain(|(known, _)| known != name);
        values.len() != before
    }

    pub fn contains(&self, name: &str) -> bool {
        self.values().iter().any(|(known, _)| known == name)
    }

    pub fn is_empty(&self) -> bool {
        self.values().is_empty()
    }

    /// The names held, in the order they were entered.
    pub fn names(&self) -> Vec<String> {
        self.values().iter().map(|(name, _)| name.clone()).collect()
    }

    /// Every `(name, value)`, for a tool process's environment.
    pub fn entries(&self) -> Vec<(String, String)> {
        self.values().clone()
    }

    /// Install (or, with `None`, remove) the prompt that asks the user for a secret.
    pub fn set_prompt(&self, prompt: Option<SecretPrompt>) {
        *self
            .inner
            .prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = prompt.map(Arc::new);
    }

    /// Whether anyone can be asked: false for piped and unattended runs.
    pub fn can_ask(&self) -> bool {
        self.prompt().is_some()
    }

    fn prompt(&self) -> Option<Arc<SecretPrompt>> {
        self.inner
            .prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ask the user for `name` and hold what they enter. False when they declined,
    /// entered nothing, or nobody can be asked. The lock is not held while the user
    /// types, so redaction elsewhere never waits on them.
    pub fn ask(&self, name: &str, reason: &str) -> bool {
        let Some(prompt) = self.prompt() else {
            return false;
        };
        let request = SecretRequest {
            name: name.to_owned(),
            reason: reason.to_owned(),
            replacing: self.contains(name),
        };
        match prompt(&request).filter(|value| !value.is_empty()) {
            Some(value) => {
                self.set(name, value);
                true
            }
            None => false,
        }
    }

    /// `text` with every held value replaced by its [`placeholder`]. Longer values
    /// go first, so one secret that contains another is replaced whole.
    pub fn redact(&self, text: &str) -> String {
        let values = self.values();
        if values.is_empty() {
            return text.to_owned();
        }
        let mut ordered: Vec<&(String, String)> = values.iter().collect();
        ordered.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
        let mut text = text.to_owned();
        for (name, value) in ordered {
            if !value.is_empty() && text.contains(value.as_str()) {
                text = text.replace(value.as_str(), &placeholder(name));
            }
        }
        text
    }

    /// [`SecretVault::redact`] on every string in `value`, keys included.
    pub fn redact_value(&self, value: &mut Value) {
        if self.is_empty() {
            return;
        }
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_value(item)),
            Value::Object(map) => {
                let entries = std::mem::take(map);
                for (key, mut item) in entries {
                    self.redact_value(&mut item);
                    map.insert(self.redact(&key), item);
                }
            }
            _ => {}
        }
    }

    /// `text` with every [`placeholder`] replaced by the value it names: the reverse
    /// of [`SecretVault::redact`], for the arguments of a tool that writes a secret
    /// where the user asked (a file). A placeholder naming a secret that is not held
    /// is an error naming it, so the model can ask for it rather than write the
    /// placeholder literally. Text that only looks like one (`${secret:}`, a name
    /// that is not a variable name, no closing brace) is left alone.
    pub fn expand(&self, text: &str) -> Result<String, String> {
        const OPEN: &str = "${secret:";
        if !text.contains(OPEN) {
            return Ok(text.to_owned());
        }
        let values = self.values();
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(OPEN) {
            out.push_str(&rest[..start]);
            let after = &rest[start + OPEN.len()..];
            let name = after.find('}').map(|end| &after[..end]);
            match name.filter(|name| valid_name(name)) {
                Some(name) => {
                    let value = values
                        .iter()
                        .find(|(known, _)| known == name)
                        .map(|(_, value)| value)
                        .ok_or_else(|| unknown_secret(name, &values))?;
                    out.push_str(value);
                    rest = &after[name.len() + 1..];
                }
                None => {
                    out.push_str(OPEN);
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        Ok(out)
    }

    /// [`SecretVault::expand`] on every string in `value` (not keys).
    pub fn expand_value(&self, value: &Value) -> Result<Value, String> {
        Ok(match value {
            Value::String(text) => Value::String(self.expand(text)?),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.expand_value(item))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| Ok((key.clone(), self.expand_value(item)?)))
                    .collect::<Result<_, String>>()?,
            ),
            other => other.clone(),
        })
    }

    /// The longest tail of `text` that could be the start of a held value, so a
    /// stream can hold it back until it knows. Always on a character boundary.
    fn partial_tail(&self, text: &str) -> usize {
        let values = self.values();
        text.char_indices()
            .map(|(index, _)| index)
            .find(|&index| {
                let tail = &text[index..];
                values
                    .iter()
                    .any(|(_, value)| value.len() > tail.len() && value.starts_with(tail))
            })
            .map_or(0, |index| text.len() - index)
    }

    /// The system message's note naming the secrets held, so the model knows what it
    /// may use (one entered with `/secret` was never announced by a tool result).
    pub fn prompt_section(&self) -> Option<String> {
        let names = self.names();
        if names.is_empty() {
            return None;
        }
        let mut lines = vec![SECTION_HEADER.to_owned()];
        lines.extend(names.iter().map(|name| format!("- {name}")));
        Some(lines.join("\n"))
    }
}

/// The error for a placeholder naming a secret that is not held.
fn unknown_secret(name: &str, values: &[(String, String)]) -> String {
    let held = if values.is_empty() {
        "No secrets are held".to_owned()
    } else {
        let names: Vec<&str> = values.iter().map(|(name, _)| name.as_str()).collect();
        format!("The secrets held are {}", names.join(", "))
    };
    format!(
        "Unknown secret {name} in {}. {held}; ask the user for it with request_secret.",
        placeholder(name)
    )
}

const SECTION_HEADER: &str = "## Secrets\n\
The user has provided these secrets. Their values are hidden from you: each is set as \
an environment variable of the same name in run_command and python_exec, write_file and \
edit_file replace ${secret:NAME} in their arguments with the value, and tool output \
shows ${secret:NAME} where a value would appear. Never ask the user to type a secret \
into the chat; call request_secret instead.";

/// Redacts streamed text as it arrives. A value split across two deltas would slip
/// past a per-delta replacement, so the tail that could still become a value is held
/// back until the next delta settles it, or until [`StreamRedactor::finish`].
#[derive(Default)]
pub struct StreamRedactor {
    pending: String,
}

impl StreamRedactor {
    /// Take `delta`; return what is safe to show now (possibly empty).
    pub fn push(&mut self, vault: &SecretVault, delta: &str) -> String {
        if vault.is_empty() && self.pending.is_empty() {
            return delta.to_owned();
        }
        self.pending.push_str(delta);
        let redacted = vault.redact(&self.pending);
        let held = vault.partial_tail(&redacted);
        let (ready, rest) = redacted.split_at(redacted.len() - held);
        self.pending = rest.to_owned();
        ready.to_owned()
    }

    /// Whatever was held back, redacted, now that the stream has ended.
    pub fn finish(&mut self, vault: &SecretVault) -> String {
        vault.redact(&std::mem::take(&mut self.pending))
    }
}

/// Ask at the terminal, input hidden, for `agent86 run` and the plain REPL: the
/// reason first (on stderr, so `run --json` keeps stdout to one object), then the
/// masked prompt. An empty entry or a failed read declines.
pub fn terminal_prompt(request: &SecretRequest) -> Option<String> {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = writeln!(err);
    let _ = writeln!(
        err,
        "The agent asks for the secret {}{}.",
        request.name,
        if request.replacing {
            " (replacing the one entered before)"
        } else {
            ""
        }
    );
    let reason = request.reason.trim();
    if !reason.is_empty() {
        let shown: String = reason.chars().filter(|ch| !ch.is_control()).collect();
        let _ = writeln!(err, "  {shown}");
    }
    let _ = writeln!(
        err,
        "The value stays out of the chat; the agent only sees its name. Press Enter alone to decline."
    );
    let _ = err.flush();
    let value =
        crate::keys::prompt_hidden(&format!("{} (input is hidden): ", request.name)).ok()?;
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vault(entries: &[(&str, &str)]) -> SecretVault {
        let vault = SecretVault::default();
        for (name, value) in entries {
            vault.set(name, (*value).to_owned());
        }
        vault
    }

    #[test]
    fn names_must_be_environment_variable_names() {
        for name in ["A", "_x", "STRIPE_API_KEY", "a1"] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "1A", "A-B", "A B", "A=B", "$A", "Ä"] {
            assert!(!valid_name(name), "{name}");
        }
    }

    #[test]
    fn values_are_replaced_by_their_placeholder_longest_first() {
        let vault = vault(&[("SHORT", "abc"), ("LONG", "abcdef")]);
        assert_eq!(
            vault.redact("x abcdef y abc z"),
            "x ${secret:LONG} y ${secret:SHORT} z"
        );
        assert_eq!(SecretVault::default().redact("abc"), "abc");
    }

    #[test]
    fn json_strings_and_keys_are_redacted() {
        let vault = vault(&[("K", "s3cr3t")]);
        let mut value = json!({"a": ["s3cr3t", 1], "s3cr3t": {"b": "xs3cr3tx"}});
        vault.redact_value(&mut value);
        assert_eq!(
            value,
            json!({"a": ["${secret:K}", 1], "${secret:K}": {"b": "x${secret:K}x"}})
        );
    }

    #[test]
    fn setting_again_replaces_and_forgetting_removes() {
        let vault = vault(&[("A", "one"), ("B", "two")]);
        vault.set("A", "three".to_owned());
        assert_eq!(
            vault.entries(),
            vec![
                ("A".to_owned(), "three".to_owned()),
                ("B".to_owned(), "two".to_owned())
            ]
        );
        assert!(vault.remove("A"));
        assert!(!vault.remove("A"));
        assert_eq!(vault.names(), ["B"]);
        // Clones share the values.
        let clone = vault.clone();
        clone.set("C", "x".to_owned());
        assert!(vault.contains("C"));
    }

    #[test]
    fn debug_output_names_but_never_shows_values() {
        let vault = vault(&[("TOKEN", "hunter2")]);
        let printed = format!("{vault:?}");
        assert!(printed.contains("TOKEN"));
        assert!(!printed.contains("hunter2"));
    }

    #[test]
    fn a_value_split_across_deltas_is_still_redacted() {
        let vault = vault(&[("K", "s3cr3t")]);
        let mut stream = StreamRedactor::default();
        let mut shown = String::new();
        for delta in ["the key is s3", "cr", "3t, and s", "o on s3"] {
            shown.push_str(&stream.push(&vault, delta));
        }
        // "s3" at the end could still become the value, so it waits for the end.
        assert_eq!(shown, "the key is ${secret:K}, and so on ");
        shown.push_str(&stream.finish(&vault));
        assert_eq!(shown, "the key is ${secret:K}, and so on s3");
    }

    #[test]
    fn an_empty_vault_streams_text_through_untouched() {
        let vault = SecretVault::default();
        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push(&vault, "hello"), "hello");
        assert_eq!(stream.finish(&vault), "");
    }

    #[test]
    fn multibyte_text_is_held_back_on_character_boundaries() {
        let vault = vault(&[("K", "héllo")]);
        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push(&vault, "say h"), "say ");
        assert_eq!(stream.push(&vault, "é"), "");
        assert_eq!(stream.push(&vault, "llo!"), "${secret:K}!");
    }

    #[test]
    fn asking_holds_what_the_user_entered() {
        let vault = SecretVault::default();
        assert!(!vault.can_ask());
        assert!(!vault.ask("A", "why"));
        vault.set_prompt(Some(Box::new(|request| {
            assert_eq!(request.reason, "deploy");
            (request.name == "A").then(|| "value".to_owned())
        })));
        assert!(vault.ask("A", "deploy"));
        assert!(!vault.ask("B", "deploy"));
        assert_eq!(vault.names(), ["A"]);
    }

    #[test]
    fn placeholders_expand_to_the_values_they_name() {
        let vault = vault(&[("A", "one"), ("B_2", "two")]);
        assert_eq!(
            vault
                .expand("A=${secret:A}\nB=${secret:B_2}${secret:A}")
                .unwrap(),
            "A=one\nB=twoone"
        );
        // Expansion undoes redaction.
        let text = "token one and two";
        assert_eq!(vault.expand(&vault.redact(text)).unwrap(), text);
        // Only well-formed placeholders count.
        for literal in [
            "${secret:}",
            "${secret:1A}",
            "${secret:A",
            "$secret:A",
            "${secret:a b}",
        ] {
            assert_eq!(vault.expand(literal).unwrap(), literal);
        }
        assert_eq!(
            vault.expand("${secret:C}").unwrap_err(),
            "Unknown secret C in ${secret:C}. The secrets held are A, B_2; ask the user for \
             it with request_secret."
        );
        assert_eq!(
            SecretVault::default().expand("${secret:C}").unwrap_err(),
            "Unknown secret C in ${secret:C}. No secrets are held; ask the user for it with \
             request_secret."
        );
    }

    #[test]
    fn json_strings_expand_but_keys_and_other_values_stay() {
        let vault = vault(&[("K", "v")]);
        assert_eq!(
            vault
                .expand_value(&json!({"${secret:K}": ["${secret:K}", 1, null]}))
                .unwrap(),
            json!({"${secret:K}": ["v", 1, null]})
        );
        assert!(vault.expand_value(&json!(["${secret:X}"])).is_err());
    }

    #[test]
    fn the_prompt_section_lists_names_only() {
        assert_eq!(SecretVault::default().prompt_section(), None);
        let section = vault(&[("A", "q7x_a"), ("B", "q7x_b")])
            .prompt_section()
            .unwrap();
        assert!(section.starts_with("## Secrets\n"));
        assert!(section.ends_with("\n- A\n- B"));
        assert!(!section.contains("q7x"));
    }
}
