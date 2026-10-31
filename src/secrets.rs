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

    /// Store (silently overwriting) the secret for `account` under `service`.
    fn set_password(
        &self,
        _service: &str,
        _account: &str,
        _secret: &str,
    ) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::read_only())
    }

    /// Delete the secret for `account`; `Ok(false)` when none was stored.
    fn delete_password(&self, _service: &str, _account: &str) -> Result<bool, SecretStoreError> {
        Err(SecretStoreError::read_only())
    }

    /// Whether keys can be kept here at all, as Python's `keyring_available`; it
    /// tells "no backend" apart from "no key stored".
    fn available(&self) -> bool {
        true
    }
}

/// The keyring refused to store or delete a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SecretStoreError(pub String);

impl SecretStoreError {
    fn read_only() -> Self {
        Self("this secret store is read-only".to_owned())
    }
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

    fn set_password(
        &self,
        service: &str,
        account: &str,
        secret: &str,
    ) -> Result<(), SecretStoreError> {
        os_keyring::set_password(service, account, secret).map_err(SecretStoreError)
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<bool, SecretStoreError> {
        os_keyring::delete_password(service, account).map_err(SecretStoreError)
    }
}

/// A store the vault can hold on to and use from any thread.
pub type SharedStore = std::sync::Arc<dyn SecretStore + Send + Sync>;

/// A store with nothing in it, for callers that must not touch the OS keyring.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSecretStore;

impl SecretStore for NoSecretStore {
    fn get_password(&self, _service: &str, _account: &str) -> Option<String> {
        None
    }

    fn available(&self) -> bool {
        false
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

/// Generic credentials addressed by target name, as in the Windows Credential Manager.
pub trait CredentialVault {
    fn read(&self, target: &str) -> Option<VaultCredential>;
    /// Create or replace the credential at `target`, including its user name.
    fn write(&self, target: &str, username: &str, password: &str) -> Result<(), String>;
    fn delete(&self, target: &str) -> Result<(), String>;
}

/// Find `account`'s secret using the Python `keyring` package's Windows layout.
///
/// Python stores the most recently saved account under target name `service`
/// and moves any account it displaces to `account@service`. A lookup therefore
/// tries `service` first, accepting it only when its user name matches, and
/// then `account@service`.
pub fn resolve_windows_credential(
    vault: &dyn CredentialVault,
    service: &str,
    account: &str,
) -> Option<String> {
    match vault.read(service) {
        Some(credential) if credential.username == account => Some(credential.password),
        _ => vault
            .read(&compound_target(account, service))
            .map(|credential| credential.password),
    }
}

/// Store `account`'s secret in the Python `keyring` package's Windows layout.
///
/// As in Python, a different account already at `service` is first moved to
/// `<its account>@service`, and the new secret takes the `service` target.
/// Unlike Python, re-saving the same account does not leave a stale copy of the
/// old secret at `account@service`; lookups check `service` first either way.
pub fn store_windows_credential(
    vault: &dyn CredentialVault,
    service: &str,
    account: &str,
    secret: &str,
) -> Result<(), String> {
    if let Some(existing) = vault.read(service) {
        if existing.username != account {
            let target = compound_target(&existing.username, service);
            vault.write(&target, &existing.username, &existing.password)?;
        }
    }
    vault.write(service, account, secret)
}

/// Delete `account`'s secret from both Windows targets, as Python does.
///
/// Only credentials whose user name is `account` are touched. Returns whether
/// anything was deleted.
pub fn delete_windows_credential(
    vault: &dyn CredentialVault,
    service: &str,
    account: &str,
) -> Result<bool, String> {
    let mut deleted = false;
    for target in [service.to_owned(), compound_target(account, service)] {
        if vault.read(&target).is_some_and(|c| c.username == account) {
            vault.delete(&target)?;
            deleted = true;
        }
    }
    Ok(deleted)
}

fn compound_target(account: &str, service: &str) -> String {
    format!("{account}@{service}")
}

#[cfg(windows)]
mod os_keyring {
    use super::{
        delete_windows_credential, resolve_windows_credential, store_windows_credential,
        CredentialVault, VaultCredential,
    };
    use keyring_core::api::CredentialStoreApi;
    use keyring_core::Entry;
    use std::collections::HashMap;
    use std::sync::Arc;
    use windows_native_keyring_store::Store;

    /// The Credential Manager, addressed by explicit target names.
    struct Vault {
        store: Arc<Store>,
        service: String,
    }

    impl Vault {
        fn open(service: &str) -> Result<Self, String> {
            let store = Store::new().map_err(|error| error.to_string())?;
            Ok(Self {
                store,
                service: service.to_owned(),
            })
        }

        fn entry(&self, target: &str, username: &str) -> Result<Entry, String> {
            let modifiers = HashMap::from([("target", target)]);
            self.store
                .build(&self.service, username, Some(&modifiers))
                .map_err(|error| error.to_string())
        }
    }

    impl CredentialVault for Vault {
        fn read(&self, target: &str) -> Option<VaultCredential> {
            let entry = self.entry(target, &self.service).ok()?;
            let password = entry.get_password().ok()?;
            let username = entry
                .get_attributes()
                .ok()?
                .remove("username")
                .unwrap_or_default();
            Some(VaultCredential { username, password })
        }

        fn write(&self, target: &str, username: &str, password: &str) -> Result<(), String> {
            // The store leaves UserName empty on a credential created under an
            // explicit target, and keeps the old one when overwriting, so set it
            // explicitly: lookups match on it, as Python's do.
            let entry = self.entry(target, username)?;
            entry
                .set_password(password)
                .map_err(|error| error.to_string())?;
            entry
                .update_attributes(&HashMap::from([("username", username)]))
                .map_err(|error| error.to_string())
        }

        fn delete(&self, target: &str) -> Result<(), String> {
            let entry = self.entry(target, &self.service)?;
            entry.delete_credential().map_err(|error| error.to_string())
        }
    }

    pub fn get_password(service: &str, account: &str) -> Option<String> {
        resolve_windows_credential(&Vault::open(service).ok()?, service, account)
    }

    pub fn set_password(service: &str, account: &str, secret: &str) -> Result<(), String> {
        store_windows_credential(&Vault::open(service)?, service, account, secret)
    }

    pub fn delete_password(service: &str, account: &str) -> Result<bool, String> {
        delete_windows_credential(&Vault::open(service)?, service, account)
    }
}

#[cfg(target_os = "macos")]
mod os_keyring {
    use keyring_core::api::CredentialStoreApi;
    use keyring_core::Entry;

    /// A generic password with the service and account attributes, as Python stores it.
    fn entry(service: &str, account: &str) -> Result<Entry, String> {
        let store = apple_native_keyring_store::keychain::Store::new()
            .map_err(|error| error.to_string())?;
        store
            .build(service, account, None)
            .map_err(|error| error.to_string())
    }

    pub fn get_password(service: &str, account: &str) -> Option<String> {
        entry(service, account).ok()?.get_password().ok()
    }

    pub fn set_password(service: &str, account: &str, secret: &str) -> Result<(), String> {
        entry(service, account)?
            .set_password(secret)
            .map_err(|error| error.to_string())
    }

    pub fn delete_password(service: &str, account: &str) -> Result<bool, String> {
        super::delete_entry(entry(service, account)?)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod os_keyring {
    use keyring_core::api::CredentialStoreApi;
    use keyring_core::Entry;

    /// An item matched on its `service` and `username` attributes, as Python stores it.
    fn entry(service: &str, account: &str) -> Result<Entry, String> {
        let store =
            zbus_secret_service_keyring_store::Store::new().map_err(|error| error.to_string())?;
        store
            .build(service, account, None)
            .map_err(|error| error.to_string())
    }

    pub fn get_password(service: &str, account: &str) -> Option<String> {
        entry(service, account).ok()?.get_password().ok()
    }

    pub fn set_password(service: &str, account: &str, secret: &str) -> Result<(), String> {
        entry(service, account)?
            .set_password(secret)
            .map_err(|error| error.to_string())
    }

    pub fn delete_password(service: &str, account: &str) -> Result<bool, String> {
        super::delete_entry(entry(service, account)?)
    }
}

#[cfg(not(any(windows, unix)))]
mod os_keyring {
    const UNSUPPORTED: &str = "no OS keyring is supported on this platform";

    pub fn get_password(_service: &str, _account: &str) -> Option<String> {
        None
    }

    pub fn set_password(_service: &str, _account: &str, _secret: &str) -> Result<(), String> {
        Err(UNSUPPORTED.to_owned())
    }

    pub fn delete_password(_service: &str, _account: &str) -> Result<bool, String> {
        Err(UNSUPPORTED.to_owned())
    }
}

/// Delete one keyring entry; a missing entry is `Ok(false)`, not an error.
#[cfg(unix)]
fn delete_entry(entry: keyring_core::Entry) -> Result<bool, String> {
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring_core::Error::NoEntry) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// Check a pasted API key before it is stored, returning it trimmed.
///
/// Catches the mistakes that make a stored key silently fail later: nothing
/// entered, embedded whitespace, or a key pasted more than once (a terminal
/// whose hidden prompt misbehaves can repeat a paste).
pub fn validate_api_key(raw: &str) -> Result<String, String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err("No key was entered.".to_owned());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("The key contains whitespace; paste only the key itself.".to_owned());
    }
    let probe_len = 16;
    if key.len() >= probe_len * 2 && key.is_char_boundary(probe_len) {
        let probe = &key[..probe_len];
        if key[1..].contains(probe) {
            return Err(format!(
                "The key looks like it was pasted more than once (its first {probe_len} characters \
                 appear again). Paste it once and try again."
            ));
        }
    }
    Ok(key.to_owned())
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
    redact_key_shapes(&out, REDACTED)
}

/// Replace every key-shaped token (Python's `_KEY_SHAPES`) in `text` with `replacement`.
pub fn redact_key_shapes(text: &str, replacement: &str) -> String {
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
                        out.push_str(replacement);
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

    /// An in-memory Credential Manager following the `CredentialVault` contract.
    #[derive(Default)]
    struct FakeVault(std::cell::RefCell<HashMap<String, VaultCredential>>);

    impl FakeVault {
        fn with(entries: &[(&str, &str, &str)]) -> Self {
            let vault = Self::default();
            for (target, username, password) in entries {
                vault.0.borrow_mut().insert(
                    (*target).to_owned(),
                    VaultCredential {
                        username: (*username).to_owned(),
                        password: (*password).to_owned(),
                    },
                );
            }
            vault
        }

        fn get(&self, target: &str) -> Option<(String, String)> {
            let entries = self.0.borrow();
            let credential = entries.get(target)?;
            Some((credential.username.clone(), credential.password.clone()))
        }

        fn targets(&self) -> Vec<String> {
            let mut targets: Vec<String> = self.0.borrow().keys().cloned().collect();
            targets.sort();
            targets
        }
    }

    impl CredentialVault for FakeVault {
        fn read(&self, target: &str) -> Option<VaultCredential> {
            self.0.borrow().get(target).cloned()
        }

        fn write(&self, target: &str, username: &str, password: &str) -> Result<(), String> {
            let credential = VaultCredential {
                username: username.to_owned(),
                password: password.to_owned(),
            };
            self.0.borrow_mut().insert(target.to_owned(), credential);
            Ok(())
        }

        fn delete(&self, target: &str) -> Result<(), String> {
            self.0.borrow_mut().remove(target);
            Ok(())
        }
    }

    fn pair(username: &str, password: &str) -> Option<(String, String)> {
        Some((username.to_owned(), password.to_owned()))
    }

    #[test]
    fn windows_lookups_follow_the_python_keyring_layout() {
        // Python keeps the latest account at "agent86" and moves older ones to "<account>@agent86".
        let vault = FakeVault::with(&[
            ("agent86", "groq", "groq-key"),
            ("openrouter@agent86", "openrouter", "openrouter-key"),
        ]);
        assert_eq!(
            resolve_windows_credential(&vault, "agent86", "groq").as_deref(),
            Some("groq-key")
        );
        assert_eq!(
            resolve_windows_credential(&vault, "agent86", "openrouter").as_deref(),
            Some("openrouter-key")
        );
        assert_eq!(
            resolve_windows_credential(&vault, "agent86", "openai"),
            None
        );
    }

    #[test]
    fn windows_lookups_find_a_lone_account_under_the_service_name() {
        let vault = FakeVault::with(&[("agent86", "openrouter", "only-key")]);
        assert_eq!(
            resolve_windows_credential(&vault, "agent86", "openrouter").as_deref(),
            Some("only-key")
        );
        assert_eq!(resolve_windows_credential(&vault, "agent86", "groq"), None);
    }

    #[test]
    fn windows_stores_move_the_previous_account_aside_like_python() {
        let vault = FakeVault::default();
        store_windows_credential(&vault, "agent86", "groq", "groq-key").unwrap();
        store_windows_credential(&vault, "agent86", "openrouter", "openrouter-key").unwrap();

        assert_eq!(vault.get("agent86"), pair("openrouter", "openrouter-key"));
        assert_eq!(vault.get("groq@agent86"), pair("groq", "groq-key"));
        for account in ["groq", "openrouter"] {
            assert_eq!(
                resolve_windows_credential(&vault, "agent86", account),
                Some(format!("{account}-key"))
            );
        }
    }

    #[test]
    fn windows_resaving_an_account_replaces_it_without_a_stale_copy() {
        let vault = FakeVault::with(&[("agent86", "openrouter", "old-key")]);
        store_windows_credential(&vault, "agent86", "openrouter", "new-key").unwrap();

        assert_eq!(vault.get("agent86"), pair("openrouter", "new-key"));
        assert_eq!(vault.targets(), ["agent86"]);
    }

    #[test]
    fn windows_resaving_a_moved_account_takes_the_service_target_again() {
        let vault = FakeVault::with(&[
            ("agent86", "groq", "groq-key"),
            ("openrouter@agent86", "openrouter", "old-key"),
        ]);
        store_windows_credential(&vault, "agent86", "openrouter", "new-key").unwrap();

        assert_eq!(vault.get("agent86"), pair("openrouter", "new-key"));
        assert_eq!(vault.get("groq@agent86"), pair("groq", "groq-key"));
        assert_eq!(
            resolve_windows_credential(&vault, "agent86", "openrouter").as_deref(),
            Some("new-key")
        );
    }

    #[test]
    fn windows_deletes_touch_only_the_named_account() {
        let vault = FakeVault::with(&[
            ("agent86", "groq", "groq-key"),
            ("openrouter@agent86", "openrouter", "openrouter-key"),
        ]);

        assert!(delete_windows_credential(&vault, "agent86", "openrouter").unwrap());
        assert_eq!(vault.targets(), ["agent86"]);
        assert!(!delete_windows_credential(&vault, "agent86", "openrouter").unwrap());
        assert!(!delete_windows_credential(&vault, "agent86", "openai").unwrap());
        assert_eq!(vault.get("agent86"), pair("groq", "groq-key"));
    }

    #[test]
    fn pasted_keys_are_checked_before_storing() {
        let key = "sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab";
        assert_eq!(validate_api_key(&format!("  {key}\r\n")).unwrap(), key);
        assert!(validate_api_key("   \n").is_err());
        assert!(validate_api_key("sk-or-v1-abc def").is_err());

        let repeated = key.repeat(4);
        let error = validate_api_key(&repeated).unwrap_err();
        assert!(error.contains("pasted more than once"), "{error}");
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
