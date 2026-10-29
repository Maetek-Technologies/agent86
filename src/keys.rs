//! `agent86 keys`: store or delete a provider's API key in the OS keyring.
//!
//! Python stores keys from its interactive `/config model` screen, which is not
//! ported; this command writes the same entries (service `agent86`, account =
//! provider section name), so either implementation can read them.
//!
//! Every check runs before the keyring is touched, so a typo or a bad paste
//! never overwrites a working key.

use crate::config::Config;
use crate::secrets::validate_api_key;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::process::{Command, Stdio};

/// Check that `provider` is a configured section whose key is looked up.
///
/// A section without `api_key_env` is a keyless endpoint: `resolve_api_key`
/// never consults the keyring for it, so a stored key would be silently ignored.
pub fn check_provider(config: &Config, provider: &str) -> Result<(), String> {
    let Some(section) = config.providers.get(provider) else {
        let keyed: Vec<&str> = config
            .providers
            .iter()
            .filter(|(_, section)| section.api_key_env.is_some())
            .map(|(name, _)| name.as_str())
            .collect();
        return Err(format!(
            "Unknown provider '{provider}'. Providers that take a key: {}.",
            keyed.join(", ")
        ));
    };
    if section.api_key_env.as_deref().is_none_or(str::is_empty) {
        return Err(format!(
            "[providers.{provider}] has no api_key_env, so agent86 never looks up a key for it. \
             Add api_key_env to that block first."
        ));
    }
    Ok(())
}

/// The environment variable that would override a stored key for `provider`, if set.
pub fn overriding_variable(config: &Config, provider: &str) -> Option<String> {
    let name = config.providers.get(provider)?.api_key_env.clone()?;
    std::env::var(&name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|_| name)
}

/// Read the key, either from stdin (`from_stdin`) or from a hidden prompt.
pub fn read_key(provider: &str, from_stdin: bool) -> Result<String, String> {
    let raw = if from_stdin {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| format!("could not read the key from stdin: {error}"))?;
        text
    } else {
        if !std::io::stdin().is_terminal() {
            return Err(
                "stdin is not a terminal, so the key cannot be prompted for. \
                 Pass --stdin to read it from stdin instead."
                    .to_owned(),
            );
        }
        prompt_hidden(&format!(
            "API key for '{provider}' (input is hidden; paste it once, then press Enter): "
        ))?
    };
    validate_api_key(&raw)
}

/// Prompt without echoing what is typed or pasted.
pub(crate) fn prompt_hidden(prompt: &str) -> Result<String, String> {
    if in_msys_terminal() {
        prompt_hidden_with_stty(prompt)
    } else {
        rpassword::prompt_password(prompt)
            .map_err(|error| format!("could not read the key: {error}"))
    }
}

/// True inside Git Bash or another MSYS2/Cygwin shell on Windows.
///
/// Those terminals deliver keystrokes through a pipe rather than the Windows
/// console, so console-based hidden prompts (Python's `getpass`, `rpassword`)
/// hang or garble input there. That is how a key ends up stored four times over.
fn in_msys_terminal() -> bool {
    cfg!(windows) && std::env::var_os("MSYSTEM").is_some()
}

/// Turn echo off with the shell's own `stty`, read one line, and turn it back on.
fn prompt_hidden_with_stty(prompt: &str) -> Result<String, String> {
    let stty = |argument: &str| {
        Command::new("stty")
            .arg(argument)
            .stdin(Stdio::inherit())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !stty("-echo") {
        return Err(
            "could not hide input in this terminal (`stty -echo` failed). \
             Run this from PowerShell or Command Prompt, or pass --stdin."
                .to_owned(),
        );
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let result = std::io::stdin().lock().read_line(&mut line);
    stty("echo");
    eprintln!();
    result.map_err(|error| format!("could not read the key: {error}"))?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, ConfigPaths, Overrides};

    fn default_config() -> Config {
        let dir = std::env::temp_dir().join(format!("agent86-keys-unit-{}", std::process::id()));
        let paths = ConfigPaths {
            user: None,
            project: dir.join("absent.toml"),
        };
        load_config(&paths, &Overrides::default()).unwrap()
    }

    #[test]
    fn keyed_providers_are_accepted() {
        let config = default_config();
        for provider in ["openrouter", "openai", "groq", "anthropic"] {
            assert_eq!(check_provider(&config, provider), Ok(()), "{provider}");
        }
    }

    #[test]
    fn unknown_and_keyless_providers_are_refused() {
        let config = default_config();

        let unknown = check_provider(&config, "openroutr").unwrap_err();
        assert!(
            unknown.starts_with("Unknown provider 'openroutr'."),
            "{unknown}"
        );
        assert!(unknown.contains("openrouter"), "{unknown}");
        assert!(!unknown.contains("ollama"), "{unknown}");

        let keyless = check_provider(&config, "ollama").unwrap_err();
        assert!(keyless.contains("has no api_key_env"), "{keyless}");
    }
}
