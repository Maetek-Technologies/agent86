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
//!
//! A secret may also be remembered for the project: the value goes into the OS
//! keyring (service `agent86`, account `secret:<workspace hash>:<NAME>`, in the same
//! layout as every other key), and a names-only index entry (`secret-index:<hash>`)
//! lists what the workspace has remembered, since keyrings cannot be listed
//! portably. At startup the remembered names are loaded back into the vault.

use crate::secrets::{SecretStore, SharedStore, SERVICE_NAME};
use serde_json::Value;
use std::path::Path;
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
    /// Whether the value can be remembered for this project (a keyring is there).
    pub can_remember: bool,
    /// Whether `name` is remembered now, so the choice starts on "remember" and a
    /// new value does not leave the old one in the keyring.
    pub remembered: bool,
}

/// What the user entered: the value, and whether to remember it for the project.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretAnswer {
    pub value: String,
    pub remember: bool,
}

impl std::fmt::Debug for SecretAnswer {
    /// Never the value: an answer printed in a panic or a log must not leak it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretAnswer")
            .field("value", &"<hidden>")
            .field("remember", &self.remember)
            .finish()
    }
}

/// Where an answer ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// Whether the value is in the keyring for this project now.
    pub remembered: bool,
    /// Why the keyring did not do as asked, when it did not.
    pub warning: Option<String>,
}

impl Stored {
    /// The line telling the user what happened to `name`.
    pub fn describe(&self, name: &str) -> String {
        let mut line = if self.remembered {
            format!("secret {name} set and remembered for this project")
        } else {
            format!("secret {name} set for this session")
        };
        line.push_str(" · the agent sees only its name");
        if let Some(warning) = &self.warning {
            line.push_str(&format!(" · {warning}"));
        }
        line
    }
}

/// Why nothing was stored for an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotSet {
    /// The user declined, entered nothing, or nobody could be asked.
    Declined,
    /// Shorter than [`MIN_SECRET_CHARS`] (beyond Python, which has no vault).
    TooShort,
}

impl NotSet {
    /// The line telling the user why `name` was not set.
    pub fn describe(self, name: &str) -> String {
        match self {
            Self::Declined => format!("secret {name} not set"),
            Self::TooShort => format!(
                "secret {name} not set: it needs at least {MIN_SECRET_CHARS} characters, or \
                 hiding it would rewrite ordinary text wherever those characters appear"
            ),
        }
    }
}

/// The shortest value the vault holds. Every occurrence of a value is replaced in
/// what the model, the session, and the trace see, so a one- or two-character
/// secret would mangle ordinary text.
pub const MIN_SECRET_CHARS: usize = 6;

/// Names the model may not ask a secret for (beyond Python): set as environment
/// variables for `run_command` and `python_exec`, they would change how programs
/// run (what is executed, what is loaded into it) rather than hand them a value.
/// Matched ignoring case; entries ending in `_` are prefixes. `/secret NAME`, where
/// the user picks the name, is not restricted.
pub const RESERVED_NAMES: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "SHELL",
    "COMSPEC",
    "SYSTEMROOT",
    "IFS",
    "PS4",
    "ENV",
    "BASH_ENV",
    "CDPATH",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "LD_",
    "DYLD_",
    "PYTHON",
    "GIT_",
];

/// Whether `name` is one the model may not ask for ([`RESERVED_NAMES`]).
pub fn reserved_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    RESERVED_NAMES.iter().any(|reserved| {
        if reserved.ends_with('_') || *reserved == "PYTHON" {
            name.starts_with(reserved)
        } else {
            name == *reserved
        }
    })
}

/// Asks the user for one secret: the answer, or `None` when they declined.
pub type SecretPrompt = Box<dyn Fn(&SecretRequest) -> Option<SecretAnswer> + Send + Sync>;

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
    /// The keyring and the project's key in it, once attached.
    keep: Mutex<Option<(SharedStore, String)>>,
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

    /// The request the prompt shows for `name`.
    pub fn request(&self, name: &str, reason: &str) -> SecretRequest {
        SecretRequest {
            name: name.to_owned(),
            reason: reason.to_owned(),
            replacing: self.contains(name),
            can_remember: self.can_remember(),
            remembered: self.is_remembered(name),
        }
    }

    /// Ask the user for `name` and hold what they enter; `None` when they declined,
    /// entered nothing, or nobody can be asked. The lock is not held while the user
    /// types, so redaction elsewhere never waits on them.
    pub fn ask(&self, name: &str, reason: &str) -> Result<Stored, NotSet> {
        let prompt = self.prompt().ok_or(NotSet::Declined)?;
        let answer = prompt(&self.request(name, reason))
            .filter(|answer| !answer.value.is_empty())
            .ok_or(NotSet::Declined)?;
        self.accept(name, answer)
    }

    /// Hold `answer` under `name`, and keep it in the keyring (or take it out) as
    /// the user chose. A keyring that refuses leaves the secret in the session. A
    /// value shorter than [`MIN_SECRET_CHARS`] is not held.
    pub fn accept(&self, name: &str, answer: SecretAnswer) -> Result<Stored, NotSet> {
        if answer.value.chars().count() < MIN_SECRET_CHARS {
            return Err(NotSet::TooShort);
        }
        self.set(name, answer.value);
        let result = if answer.remember && self.can_remember() {
            self.remember(name).map(|()| true)
        } else if self.is_remembered(name) {
            // "Just this once": the keyring must not keep an older value.
            self.unremember(name).map(|_| false)
        } else {
            Ok(false)
        };
        Ok(match result {
            Ok(remembered) => Stored {
                remembered,
                warning: None,
            },
            Err(error) => Stored {
                remembered: self.is_remembered(name),
                warning: Some(format!("the keyring refused: {error}")),
            },
        })
    }

    // ---- remembering ------------------------------------------------------

    /// Keep secrets for the workspace in `store`, under a key derived from the
    /// workspace's path.
    pub fn attach_store(&self, store: SharedStore, workspace: &Path) {
        let path = crate::tools::plain_path(workspace);
        let project = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));
        *self
            .inner
            .keep
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((store, project));
    }

    fn keep(&self) -> Option<(SharedStore, String)> {
        self.inner
            .keep
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether a value can be remembered: a store is attached and works.
    pub fn can_remember(&self) -> bool {
        self.keep().is_some_and(|(store, _)| store.available())
    }

    /// The names remembered for this project (never values), in the order they were
    /// remembered.
    pub fn remembered_names(&self) -> Vec<String> {
        self.keep()
            .map(|(store, project)| read_index(store.as_ref(), &project))
            .unwrap_or_default()
    }

    pub fn is_remembered(&self, name: &str) -> bool {
        self.remembered_names().iter().any(|known| known == name)
    }

    /// Put the value held under `name` in the keyring and the project's index.
    pub fn remember(&self, name: &str) -> Result<(), String> {
        let (store, project) = self.keep().ok_or("no keyring to remember it in")?;
        let value = self
            .values()
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| format!("no secret named {name}"))?;
        store
            .set_password(SERVICE_NAME, &account(&project, name), &value)
            .map_err(|error| error.to_string())?;
        let mut names = read_index(store.as_ref(), &project);
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
            write_index(store.as_ref(), &project, &names)?;
        }
        Ok(())
    }

    /// Take `name` out of the keyring and the index, keeping the session's value.
    /// Whether it was remembered.
    fn unremember(&self, name: &str) -> Result<bool, String> {
        let Some((store, project)) = self.keep() else {
            return Ok(false);
        };
        let mut names = read_index(store.as_ref(), &project);
        let listed = names.iter().any(|known| known == name);
        let deleted = store
            .delete_password(SERVICE_NAME, &account(&project, name))
            .map_err(|error| error.to_string())?;
        if listed {
            names.retain(|known| known != name);
            write_index(store.as_ref(), &project, &names)?;
        }
        Ok(listed || deleted)
    }

    /// `/secret forget NAME`: drop `name` from the session and the keyring; whether
    /// either had it. A keyring that refuses is an error, after the session has
    /// already dropped the value.
    pub fn forget(&self, name: &str) -> Result<bool, String> {
        let held = self.remove(name);
        let kept = self.unremember(name)?;
        Ok(held || kept)
    }

    /// Load every secret remembered for this project into the session; the names
    /// loaded. A name whose value the keyring no longer has is skipped.
    pub fn load_remembered(&self) -> Vec<String> {
        let Some((store, project)) = self.keep() else {
            return Vec::new();
        };
        let mut loaded = Vec::new();
        for name in read_index(store.as_ref(), &project) {
            if let Some(value) = store.get_password(SERVICE_NAME, &account(&project, &name)) {
                if !value.is_empty() {
                    self.set(&name, value);
                    loaded.push(name);
                }
            }
        }
        loaded
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

/// The keyring account of the secret `name` in the project `project`.
fn account(project: &str, name: &str) -> String {
    format!("secret:{project}:{name}")
}

/// The keyring account of the project's list of remembered names.
fn index_account(project: &str) -> String {
    format!("secret-index:{project}")
}

/// The names the project's index lists; only well-formed names count.
fn read_index(store: &(dyn SecretStore + Send + Sync), project: &str) -> Vec<String> {
    store
        .get_password(SERVICE_NAME, &index_account(project))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|name| valid_name(name))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Write the index, or delete it when nothing is remembered.
fn write_index(
    store: &(dyn SecretStore + Send + Sync),
    project: &str,
    names: &[String],
) -> Result<(), String> {
    let account = index_account(project);
    if names.is_empty() {
        store
            .delete_password(SERVICE_NAME, &account)
            .map(|_| ())
            .map_err(|error| error.to_string())
    } else {
        store
            .set_password(SERVICE_NAME, &account, &names.join("\n"))
            .map_err(|error| error.to_string())
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
/// masked prompt, then whether to remember it for the project when a keyring is
/// there. An empty entry or a failed read declines.
pub fn terminal_prompt(request: &SecretRequest) -> Option<SecretAnswer> {
    use std::io::{BufRead, Write};
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
    if value.is_empty() {
        return None;
    }
    let mut remember = false;
    if request.can_remember {
        let choices = if request.remembered { "[Y/n]" } else { "[y/N]" };
        let _ = write!(
            err,
            "Remember it for this project, in the OS keyring? {choices} "
        );
        let _ = err.flush();
        let mut line = String::new();
        remember = match std::io::stdin().lock().read_line(&mut line) {
            Ok(read) if read > 0 => match line.trim().to_lowercase().as_str() {
                "y" | "yes" => true,
                "n" | "no" => false,
                _ => request.remembered,
            },
            _ => request.remembered,
        };
    }
    Some(SecretAnswer { value, remember })
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
        assert!(vault.ask("A", "why") == Err(NotSet::Declined));
        vault.set_prompt(Some(Box::new(|request| {
            assert_eq!(request.reason, "deploy");
            // No keyring is attached, so there is nothing to remember in.
            assert!(!request.can_remember && !request.remembered);
            (request.name == "A").then(|| SecretAnswer {
                value: "value-1".to_owned(),
                remember: true,
            })
        })));
        let stored = vault.ask("A", "deploy").unwrap();
        // Asked to remember, but nothing can: the session keeps it.
        assert!(!stored.remembered && stored.warning.is_none());
        assert!(vault.ask("B", "deploy") == Err(NotSet::Declined));
        assert_eq!(vault.names(), ["A"]);
    }

    #[test]
    fn a_value_too_short_to_hide_safely_is_not_held() {
        let vault = SecretVault::default();
        let answer = |value: &str| SecretAnswer {
            value: value.to_owned(),
            remember: false,
        };
        assert_eq!(vault.accept("PIN", answer("12345")), Err(NotSet::TooShort));
        assert!(vault.names().is_empty());
        // Characters, not bytes: five accented letters are still five.
        assert_eq!(vault.accept("PIN", answer("ééééé")), Err(NotSet::TooShort));
        assert!(vault.accept("PIN", answer("123456")).is_ok());
        assert_eq!(vault.names(), ["PIN"]);
        assert!(NotSet::TooShort
            .describe("PIN")
            .starts_with("secret PIN not set: it needs at least 6 characters"));
    }

    #[test]
    fn names_that_change_how_programs_run_are_reserved() {
        for name in [
            "PATH",
            "path",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "NODE_OPTIONS",
            "BASH_ENV",
            "GIT_SSH_COMMAND",
            "IFS",
        ] {
            assert!(reserved_name(name), "{name}");
        }
        for name in [
            "API_TOKEN",
            "MY_PATH",
            "OPENAI_API_KEY",
            "PATHS",
            "GITHUB_TOKEN",
        ] {
            assert!(!reserved_name(name), "{name}");
        }
    }

    /// A keyring in memory, shared between "sessions" like the real one.
    #[derive(Default)]
    struct FakeKeyring {
        entries: Mutex<Vec<((String, String), String)>>,
        refuse_writes: std::sync::atomic::AtomicBool,
    }

    impl FakeKeyring {
        fn accounts(&self) -> Vec<String> {
            self.entries
                .lock()
                .unwrap()
                .iter()
                .map(|((_, account), _)| account.clone())
                .collect()
        }

        fn dump(&self) -> String {
            format!("{:?}", self.entries.lock().unwrap())
        }
    }

    impl SecretStore for FakeKeyring {
        fn get_password(&self, service: &str, account: &str) -> Option<String> {
            self.entries
                .lock()
                .unwrap()
                .iter()
                .find(|(key, _)| key.0 == service && key.1 == account)
                .map(|(_, value)| value.clone())
        }

        fn set_password(
            &self,
            service: &str,
            account: &str,
            secret: &str,
        ) -> Result<(), crate::secrets::SecretStoreError> {
            if self.refuse_writes.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::secrets::SecretStoreError("locked".to_owned()));
            }
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|(key, _)| !(key.0 == service && key.1 == account));
            entries.push(((service.to_owned(), account.to_owned()), secret.to_owned()));
            Ok(())
        }

        fn delete_password(
            &self,
            service: &str,
            account: &str,
        ) -> Result<bool, crate::secrets::SecretStoreError> {
            let mut entries = self.entries.lock().unwrap();
            let before = entries.len();
            entries.retain(|(key, _)| !(key.0 == service && key.1 == account));
            Ok(entries.len() != before)
        }
    }

    fn kept_vault(keyring: &Arc<FakeKeyring>, workspace: &str) -> SecretVault {
        let vault = SecretVault::default();
        vault.attach_store(keyring.clone(), Path::new(workspace));
        vault
    }

    fn answering(vault: &SecretVault, value: &'static str, remember: bool) {
        vault.set_prompt(Some(Box::new(move |_| {
            Some(SecretAnswer {
                value: value.to_owned(),
                remember,
            })
        })));
    }

    #[test]
    fn a_remembered_secret_comes_back_in_a_new_session() {
        let keyring = Arc::new(FakeKeyring::default());
        let first = kept_vault(&keyring, ".");
        answering(&first, "tok_1111", true);
        let stored = first.ask("API_TOKEN", "deploy").unwrap();
        assert_eq!(
            stored,
            Stored {
                remembered: true,
                warning: None
            }
        );
        assert_eq!(
            stored.describe("API_TOKEN"),
            "secret API_TOKEN set and remembered for this project · the agent sees only its name"
        );
        // A secret entered for the session only is not remembered.
        answering(&first, "tok_2222", false);
        first.ask("ONCE", "").unwrap();
        assert_eq!(first.remembered_names(), ["API_TOKEN"]);

        // The keyring holds the value and a names-only index, and lists no other name.
        let accounts = keyring.accounts();
        assert!(accounts
            .iter()
            .any(|a| a.starts_with("secret:") && a.ends_with(":API_TOKEN")));
        assert!(accounts.iter().any(|a| a.starts_with("secret-index:")));
        assert!(!accounts.iter().any(|a| a.ends_with(":ONCE")));

        let second = kept_vault(&keyring, ".");
        assert!(second.is_empty());
        assert_eq!(second.load_remembered(), ["API_TOKEN"]);
        assert_eq!(
            second.entries(),
            [("API_TOKEN".to_owned(), "tok_1111".to_owned())]
        );
    }

    #[test]
    fn remembered_secrets_belong_to_their_workspace() {
        let keyring = Arc::new(FakeKeyring::default());
        let here = kept_vault(&keyring, "some/project");
        answering(&here, "tok_1111", true);
        here.ask("API_TOKEN", "").unwrap();

        let elsewhere = kept_vault(&keyring, "other/project");
        assert!(elsewhere.remembered_names().is_empty());
        assert!(elsewhere.load_remembered().is_empty());
        assert!(elsewhere.is_empty());
        assert_eq!(
            kept_vault(&keyring, "some/project").load_remembered(),
            ["API_TOKEN"]
        );
    }

    #[test]
    fn forgetting_drops_the_session_value_and_the_keyring_entries() {
        let keyring = Arc::new(FakeKeyring::default());
        let vault = kept_vault(&keyring, ".");
        answering(&vault, "tok_1111", true);
        vault.ask("A", "").unwrap();
        vault.ask("B", "").unwrap();

        assert_eq!(vault.forget("A"), Ok(true));
        assert_eq!(vault.forget("A"), Ok(false));
        assert_eq!(vault.names(), ["B"]);
        assert_eq!(vault.remembered_names(), ["B"]);
        assert_eq!(
            keyring.accounts().len(),
            2,
            "B's value and the index: {:?}",
            keyring.accounts()
        );
        // The last one out takes the index with it.
        assert_eq!(vault.forget("B"), Ok(true));
        assert!(keyring.accounts().is_empty());
        // A name only the keyring had (a new session that did not load it) is forgotten too.
        answering(&vault, "tok_1111", true);
        vault.ask("C", "").unwrap();
        let fresh = kept_vault(&keyring, ".");
        assert_eq!(fresh.forget("C"), Ok(true));
        assert!(keyring.accounts().is_empty());
    }

    #[test]
    fn a_new_value_replaces_the_remembered_one_or_stops_remembering() {
        let keyring = Arc::new(FakeKeyring::default());
        let vault = kept_vault(&keyring, ".");
        answering(&vault, "old-value", true);
        vault.ask("K", "").unwrap();

        // The prompt starts on "remember" for a name that is remembered.
        vault.set_prompt(Some(Box::new(|request| {
            assert!(request.remembered && request.replacing && request.can_remember);
            Some(SecretAnswer {
                value: "new-value".to_owned(),
                remember: true,
            })
        })));
        vault.ask("K", "").unwrap();
        assert_eq!(kept_vault(&keyring, ".").load_remembered(), ["K"]);
        let reloaded = kept_vault(&keyring, ".");
        reloaded.load_remembered();
        assert_eq!(reloaded.entries()[0].1, "new-value");

        // "Just this once" must not leave the old value in the keyring.
        answering(&vault, "once-value", false);
        let stored = vault.ask("K", "").unwrap();
        assert!(!stored.remembered);
        assert!(vault.remembered_names().is_empty());
        assert!(!keyring.dump().contains("new-value"), "{}", keyring.dump());
        assert_eq!(vault.entries(), [("K".to_owned(), "once-value".to_owned())]);
    }

    #[test]
    fn a_keyring_that_refuses_leaves_the_secret_in_the_session() {
        let keyring = Arc::new(FakeKeyring::default());
        keyring
            .refuse_writes
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let vault = kept_vault(&keyring, ".");
        answering(&vault, "tok_1111", true);
        let stored = vault.ask("K", "").unwrap();
        assert!(!stored.remembered);
        assert_eq!(
            stored.warning.as_deref(),
            Some("the keyring refused: locked")
        );
        assert!(stored
            .describe("K")
            .ends_with("· the keyring refused: locked"));
        assert_eq!(vault.names(), ["K"]);
        assert!(keyring.accounts().is_empty());
    }

    #[test]
    fn a_store_that_cannot_keep_anything_is_not_offered() {
        let vault = SecretVault::default();
        vault.attach_store(Arc::new(crate::secrets::NoSecretStore), Path::new("."));
        assert!(!vault.can_remember());
        assert!(!vault.request("K", "").can_remember);
        assert!(SecretVault::default().remembered_names().is_empty());
        assert!(SecretVault::default().load_remembered().is_empty());
        assert_eq!(SecretVault::default().forget("K"), Ok(false));
    }

    #[test]
    fn answers_never_print_their_value() {
        let answer = SecretAnswer {
            value: "hunter2".to_owned(),
            remember: true,
        };
        let printed = format!("{answer:?}");
        assert!(!printed.contains("hunter2") && printed.contains("remember: true"));
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
