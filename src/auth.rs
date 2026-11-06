//! `agent86 auth`: interactive provider and API-key setup, beyond Python.
//!
//! This is the command an ACP client runs for the `terminal` authentication
//! method (see `acp::AUTH_METHOD_ID`): it lists the providers that take a key,
//! asks which one, reads the key with input hidden, and stores it in the OS
//! keyring under the same service and account as `agent86 keys set`.
//!
//! The flow is written against a reader, a writer, a key reader, and a
//! [`SecretStore`], so tests drive it without a terminal or the real keyring.

use crate::config::Config;
use crate::secrets::{resolve_api_key, SecretStore, SERVICE_NAME};
use std::io::{BufRead, Write};

/// The providers whose key agent86 looks up (those with an `api_key_env`), in
/// config order, each with whether a key is already available.
pub fn keyed_providers(config: &Config, store: &dyn SecretStore) -> Vec<(String, bool)> {
    config
        .providers
        .iter()
        .filter(|(_, section)| {
            section
                .api_key_env
                .as_deref()
                .is_some_and(|n| !n.is_empty())
        })
        .map(|(name, section)| {
            let have = resolve_api_key(name, section.api_key_env.as_deref(), store).is_some();
            (name.clone(), have)
        })
        .collect()
}

/// Run the setup. `read_key` receives the provider name and returns the
/// validated key (hidden prompt in production). Returns the confirmation text.
pub fn run(
    config: &Config,
    store: &dyn SecretStore,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    read_key: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let providers = keyed_providers(config, store);
    if providers.is_empty() {
        return Err("No provider in the config takes an API key (none has api_key_env).".into());
    }
    let say = |out: &mut dyn Write, text: &str| {
        writeln!(out, "{text}").map_err(|error| format!("could not write: {error}"))
    };
    say(
        out,
        "Set up agent86: choose a provider to store an API key for.",
    )?;
    for (index, (name, have)) in providers.iter().enumerate() {
        let status = if *have { "key found" } else { "no key" };
        say(out, &format!("  {}. {name} ({status})", index + 1))?;
    }
    write!(out, "Provider (number or name): ")
        .and_then(|()| out.flush())
        .map_err(|error| format!("could not write: {error}"))?;
    let mut line = String::new();
    let read = input
        .read_line(&mut line)
        .map_err(|error| format!("could not read the choice: {error}"))?;
    if read == 0 {
        return Err("No provider chosen (end of input).".into());
    }
    let choice = line.trim();
    let provider = match choice.parse::<usize>() {
        Ok(number) if (1..=providers.len()).contains(&number) => providers[number - 1].0.clone(),
        _ => providers
            .iter()
            .map(|(name, _)| name)
            .find(|name| name.as_str() == choice)
            .cloned()
            .ok_or_else(|| format!("'{choice}' is not one of the listed providers."))?,
    };
    // The key is validated before the keyring is touched, as in `keys set`.
    let key = read_key(&provider)?;
    store
        .set_password(SERVICE_NAME, &provider, &key)
        .map_err(|error| format!("Could not store the key for '{provider}': {error}"))?;
    Ok(format!(
        "Stored the key for '{provider}' in the OS keyring \
         (service \"{SERVICE_NAME}\", account \"{provider}\")."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, ConfigPaths, Overrides, ProjectTrust};
    use crate::secrets::SecretStoreError;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake(RefCell<Vec<(String, String)>>);

    impl SecretStore for Fake {
        fn get_password(&self, _service: &str, _account: &str) -> Option<String> {
            None
        }
        fn set_password(
            &self,
            _s: &str,
            account: &str,
            secret: &str,
        ) -> Result<(), SecretStoreError> {
            self.0.borrow_mut().push((account.into(), secret.into()));
            Ok(())
        }
    }

    fn config() -> Config {
        let dir = std::env::temp_dir().join(format!("agent86-auth-unit-{}", std::process::id()));
        let paths = ConfigPaths {
            user: None,
            project: dir.join("absent.toml"),
            project_trust: ProjectTrust::Trusted,
        };
        load_config(&paths, &Overrides::default()).unwrap()
    }

    fn go(choice: &str, store: &Fake) -> (Result<String, String>, String) {
        let mut out = Vec::new();
        let mut input = choice.as_bytes();
        let mut key = |_: &str| Ok("sk-test-key".to_owned());
        let result = run(&config(), store, &mut input, &mut out, &mut key);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_number_or_a_name_stores_the_key_for_that_provider() {
        let first = keyed_providers(&config(), &Fake::default())[0].0.clone();
        let store = Fake::default();
        let (result, shown) = go("1\n", &store);
        assert!(result.unwrap().contains(&first));
        assert!(shown.contains("1. "), "{shown}");
        assert_eq!(
            store.0.borrow().as_slice(),
            [(first.clone(), "sk-test-key".into())]
        );

        let store = Fake::default();
        let (result, _) = go("openrouter\n", &store);
        assert!(result.is_ok());
        assert_eq!(store.0.borrow()[0].0, "openrouter");
    }

    #[test]
    fn keyless_unknown_and_empty_choices_store_nothing() {
        for choice in ["ollama\n", "nope\n", "0\n", "99\n", "\n", ""] {
            let store = Fake::default();
            let (result, _) = go(choice, &store);
            assert!(result.is_err(), "{choice:?}");
            assert!(store.0.borrow().is_empty(), "{choice:?}");
        }
    }

    #[test]
    fn a_rejected_key_is_not_stored() {
        let store = Fake::default();
        let mut out = Vec::new();
        let mut input = "openrouter\n".as_bytes();
        let mut key = |_: &str| Err("The API key looks wrong.".to_owned());
        let result = run(&config(), &store, &mut input, &mut out, &mut key);
        assert_eq!(result.unwrap_err(), "The API key looks wrong.");
        assert!(store.0.borrow().is_empty());
    }
}
