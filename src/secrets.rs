//! API key resolution and secret redaction, matching the Python `secrets.py`.
//!
//! Precedence is strict: the environment variable named by a provider's
//! `api_key_env` wins, and only when it is unset is the OS keyring consulted.
//! Keyring entries use service `agent86` and the config section name
//! (`openrouter`, `groq`, ...) as the account, so keys stored by the Python
//! implementation are found here too.

/// The keyring service name shared with the Python implementation.
pub const SERVICE_NAME: &str = "agent86";

/// Somewhere API keys may be stored, consulted after the environment.
pub trait SecretStore {
    /// The secret for `account` under `service`, or `None` when absent or unreadable.
    fn get_password(&self, service: &str, account: &str) -> Option<String>;
}

/// The operating system's credential store: Windows Credential Manager, the
/// macOS Keychain, or the Secret Service on Linux and other Unix systems.
///
/// Every failure, including a missing backend on a headless machine, reads as
/// "no key stored", so lookups degrade to environment-only as in Python.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsKeyring;

impl SecretStore for OsKeyring {
    fn get_password(&self, service: &str, account: &str) -> Option<String> {
        os_keyring::get_password(service, account)
    }
}

/// A store with nothing in it, for callers that must not touch the OS keyring.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSecretStore;

impl SecretStore for NoSecretStore {
    fn get_password(&self, _service: &str, _account: &str) -> Option<String> {
        None
    }
}

/// Resolve a provider's API key: the environment variable first, then `store`.
///
/// `provider_name` is the config section name, which is the keyring account.
/// A missing `api_key_env` means a keyless endpoint, so neither the environment
/// nor the keyring is consulted and stray keyring entries cannot leak into it.
pub fn resolve_api_key(
    provider_name: &str,
    api_key_env: Option<&str>,
    store: &dyn SecretStore,
) -> Option<String> {
    let name = api_key_env.filter(|name| !name.is_empty())?;
    if let Some(value) = std::env::var(name).ok().filter(|value| !value.is_empty()) {
        return Some(value);
    }
    store.get_password(SERVICE_NAME, provider_name)
}

/// One generic credential in the Windows Credential Manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultCredential {
    pub username: String,
    pub password: String,
}

/// Find `account`'s secret using the Python `keyring` package's Windows layout.
///
/// Python stores the most recently saved account under target name `service`
/// and moves any account it displaces to `account@service`. A lookup therefore
/// tries `service` first, accepting it only when its user name matches, and
/// then `account@service`. `read` returns the credential with a given target name.
pub fn resolve_windows_credential(
    service: &str,
    account: &str,
    read: impl Fn(&str) -> Option<VaultCredential>,
) -> Option<String> {
    match read(service) {
        Some(credential) if credential.username == account => Some(credential.password),
        _ => read(&format!("{account}@{service}")).map(|credential| credential.password),
    }
}

#[cfg(windows)]
mod os_keyring {
    use super::{resolve_windows_credential, VaultCredential};
    use keyring_core::api::CredentialStoreApi;
    use std::collections::HashMap;

    pub fn get_password(service: &str, account: &str) -> Option<String> {
        let store = windows_native_keyring_store::Store::new().ok()?;
        let read = |target: &str| {
            let modifiers = HashMap::from([("target", target)]);
            let entry = store.build(service, account, Some(&modifiers)).ok()?;
            let password = entry.get_password().ok()?;
            let username = entry
                .get_attributes()
                .ok()?
                .remove("username")
                .unwrap_or_default();
            Some(VaultCredential { username, password })
        };
        resolve_windows_credential(service, account, read)
    }
}

#[cfg(target_os = "macos")]
mod os_keyring {
    use keyring_core::api::CredentialStoreApi;

    /// A generic password with the service and account attributes, as Python stores it.
    pub fn get_password(service: &str, account: &str) -> Option<String> {
        let store = apple_native_keyring_store::keychain::Store::new().ok()?;
        store
            .build(service, account, None)
            .ok()?
            .get_password()
            .ok()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod os_keyring {
    use keyring_core::api::CredentialStoreApi;

    /// An item matched on its `service` and `username` attributes, as Python stores it.
    pub fn get_password(service: &str, account: &str) -> Option<String> {
        let store = zbus_secret_service_keyring_store::Store::new().ok()?;
        store
            .build(service, account, None)
            .ok()?
            .get_password()
            .ok()
    }
}

#[cfg(not(any(windows, unix)))]
mod os_keyring {
    pub fn get_password(_service: &str, _account: &str) -> Option<String> {
        None
    }
}

/// Replacement text for a redacted secret, matching the Python implementation.
pub const REDACTED: &str = "***redacted***";

/// Credential prefixes and the minimum length of the body that follows them.
///
/// Deliberately conservative: a false positive only makes an error message less
/// precise, while a false negative leaks a key.
const KEY_SHAPES: &[KeyShape] = &[
    KeyShape::new("sk-", 16, is_sk_body),
    KeyShape::new("gsk_", 20, is_alphanumeric_body),
    KeyShape::new("xai-", 20, is_alphanumeric_body),
    KeyShape::new("AIza", 20, is_google_body),
];

struct KeyShape {
    prefix: &'static str,
    min_body: usize,
    is_body: fn(char) -> bool,
}

impl KeyShape {
    const fn new(prefix: &'static str, min_body: usize, is_body: fn(char) -> bool) -> Self {
        Self {
            prefix,
            min_body,
            is_body,
        }
    }
}

/// Return `text` with every known secret and key-shaped token replaced.
///
/// Known secrets shorter than eight characters are ignored, as in the Python
/// implementation, so that short values cannot blank out unrelated text.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if secret.len() >= 8 {
            out = out.replace(secret, REDACTED);
        }
    }
    redact_key_shapes(&out)
}

fn redact_key_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut previous: Option<char> = None;

    'scan: while let Some(ch) = rest.chars().next() {
        if !previous.is_some_and(is_word_char) {
            for &KeyShape {
                prefix,
                min_body,
                is_body,
            } in KEY_SHAPES
            {
                if let Some(after) = rest.strip_prefix(prefix) {
                    let body = &after[..after.find(|c: char| !is_body(c)).unwrap_or(after.len())];
                    // A regex `\b` would backtrack off trailing punctuation such as `.`.
                    let body_len = body.trim_end_matches(|c: char| !is_word_char(c)).len();
                    let token_end = prefix.len() + body_len;
                    let next = rest[token_end..].chars().next();
                    if body_len >= min_body && !next.is_some_and(is_word_char) {
                        out.push_str(REDACTED);
                        previous = rest[..token_end].chars().next_back();
                        rest = &rest[token_end..];
                        continue 'scan;
                    }
                }
            }
        }
        out.push(ch);
        previous = Some(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_alphanumeric_body(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
}

fn is_sk_body(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

fn is_google_body(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    /// An in-memory store keyed by (service, account).
    struct FakeStore(HashMap<(String, String), String>);

    impl FakeStore {
        fn with(account: &str, secret: &str) -> Self {
            let key = (SERVICE_NAME.to_owned(), account.to_owned());
            Self(HashMap::from([(key, secret.to_owned())]))
        }
    }

    impl SecretStore for FakeStore {
        fn get_password(&self, service: &str, account: &str) -> Option<String> {
            self.0
                .get(&(service.to_owned(), account.to_owned()))
                .cloned()
        }
    }

    #[test]
    fn the_environment_wins_over_the_keyring() {
        let store = FakeStore::with("openrouter", "from-keyring");
        std::env::set_var("AGENT86_TEST_ENV_WINS", "from-env");
        let key = resolve_api_key("openrouter", Some("AGENT86_TEST_ENV_WINS"), &store);
        std::env::remove_var("AGENT86_TEST_ENV_WINS");
        assert_eq!(key.as_deref(), Some("from-env"));
    }

    #[test]
    fn the_keyring_is_used_when_the_variable_is_unset_or_empty() {
        let store = FakeStore::with("openrouter", "from-keyring");
        let unset = resolve_api_key("openrouter", Some("AGENT86_TEST_UNSET_VAR"), &store);
        assert_eq!(unset.as_deref(), Some("from-keyring"));

        std::env::set_var("AGENT86_TEST_EMPTY_VAR", "");
        let empty = resolve_api_key("openrouter", Some("AGENT86_TEST_EMPTY_VAR"), &store);
        std::env::remove_var("AGENT86_TEST_EMPTY_VAR");
        assert_eq!(empty.as_deref(), Some("from-keyring"));

        assert_eq!(
            resolve_api_key("groq", Some("AGENT86_TEST_UNSET_VAR"), &store),
            None
        );
    }

    #[test]
    fn keyless_endpoints_never_consult_the_keyring() {
        let store = FakeStore::with("ollama", "stray");
        assert_eq!(resolve_api_key("ollama", None, &store), None);
        assert_eq!(resolve_api_key("ollama", Some(""), &store), None);
    }

    fn vault(entries: &[(&str, &str, &str)]) -> impl Fn(&str) -> Option<VaultCredential> {
        let entries: HashMap<String, VaultCredential> = entries
            .iter()
            .map(|(target, username, password)| {
                let credential = VaultCredential {
                    username: (*username).to_owned(),
                    password: (*password).to_owned(),
                };
                ((*target).to_owned(), credential)
            })
            .collect();
        move |target| entries.get(target).cloned()
    }

    #[test]
    fn windows_lookups_follow_the_python_keyring_layout() {
        // Python keeps the latest account at "agent86" and moves older ones to "<account>@agent86".
        let read = vault(&[
            ("agent86", "groq", "groq-key"),
            ("openrouter@agent86", "openrouter", "openrouter-key"),
        ]);
        assert_eq!(
            resolve_windows_credential("agent86", "groq", &read).as_deref(),
            Some("groq-key")
        );
        assert_eq!(
            resolve_windows_credential("agent86", "openrouter", &read).as_deref(),
            Some("openrouter-key")
        );
        assert_eq!(resolve_windows_credential("agent86", "openai", &read), None);
    }

    #[test]
    fn windows_lookups_find_a_lone_account_under_the_service_name() {
        let read = vault(&[("agent86", "openrouter", "only-key")]);
        assert_eq!(
            resolve_windows_credential("agent86", "openrouter", &read).as_deref(),
            Some("only-key")
        );
        assert_eq!(resolve_windows_credential("agent86", "groq", &read), None);
    }

    #[test]
    fn known_secrets_and_key_shapes_are_redacted() {
        let text = "key my-secret-value and sk-or-v1-abcdef0123456789abcdef in body";
        assert_eq!(
            redact(text, &["my-secret-value"]),
            "key ***redacted*** and ***redacted*** in body"
        );
    }

    #[test]
    fn short_secrets_and_short_tokens_are_left_alone() {
        assert_eq!(redact("abc sk-short", &["abc"]), "abc sk-short");
    }

    #[test]
    fn key_prefixes_inside_words_are_not_tokens() {
        let text = "task-0123456789abcdef0123 gsk_0123456789abcdefghijXYZ";
        assert_eq!(
            redact(text, &[]),
            "task-0123456789abcdef0123 ***redacted***"
        );
    }

    #[test]
    fn trailing_punctuation_is_kept() {
        assert_eq!(
            redact("bad key sk-0123456789abcdef0123.", &[]),
            "bad key ***redacted***."
        );
    }
}
