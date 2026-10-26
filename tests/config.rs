//! Layered configuration resolution.

use agent86::config::{self, load_from, Overrides, DEFAULT_MODEL};
use std::path::PathBuf;
use tempfile::TempDir;

fn write(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn defaults_match_the_reference_providers() {
    let config = load_from(&[], &Overrides::default()).unwrap();

    assert_eq!(config.default_model, DEFAULT_MODEL);
    assert!(config.sources.is_empty());
    let openrouter = config.provider("openrouter");
    assert_eq!(
        openrouter.api_key_env.as_deref(),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(
        openrouter.base_url.as_deref(),
        Some("https://openrouter.ai/api/v1")
    );
    assert_eq!(
        openrouter.connect_timeout_s,
        config::DEFAULT_CONNECT_TIMEOUT_S
    );
    assert_eq!(openrouter.read_timeout_s, config::DEFAULT_READ_TIMEOUT_S);
    assert_eq!(openrouter.max_retries, config::DEFAULT_MAX_RETRIES);
    assert_eq!(openrouter.max_tokens, None);
    assert_eq!(
        config.provider("groq").base_url.as_deref(),
        Some("https://api.groq.com/openai/v1")
    );
}

#[test]
fn later_files_deep_merge_over_earlier_ones() {
    let dir = tempfile::tempdir().unwrap();
    let user = write(
        &dir,
        "user.toml",
        "[model]\ndefault = \"openrouter:a\"\n[providers.openrouter]\nread_timeout_s = 60\n",
    );
    let project = write(
        &dir,
        "project.toml",
        "[providers.openrouter]\nbase_url = \"http://localhost:9999/v1\"\nmax_tokens = 512\n",
    );

    let config = load_from(&[user.clone(), project.clone()], &Overrides::default()).unwrap();

    assert_eq!(config.default_model, "openrouter:a");
    assert_eq!(config.sources, [user, project]);
    let openrouter = config.provider("openrouter");
    assert_eq!(
        openrouter.base_url.as_deref(),
        Some("http://localhost:9999/v1")
    );
    assert_eq!(
        openrouter.api_key_env.as_deref(),
        Some("OPENROUTER_API_KEY"),
        "setting base_url must not wipe the default api_key_env"
    );
    assert_eq!(
        openrouter.read_timeout_s, 60.0,
        "integers are accepted for seconds"
    );
    assert_eq!(openrouter.max_tokens, Some(512));
}

#[test]
fn the_flag_beats_the_environment_which_beats_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(&dir, "c.toml", "[model]\ndefault = \"openai:from-file\"\n");
    let files = [file];

    let env_only = Overrides {
        env_model: Some("openai:from-env".into()),
        flag_model: None,
    };
    assert_eq!(
        load_from(&files, &env_only).unwrap().default_model,
        "openai:from-env"
    );

    let both = Overrides {
        env_model: Some("openai:from-env".into()),
        flag_model: Some("openai:from-flag".into()),
    };
    assert_eq!(
        load_from(&files, &both).unwrap().default_model,
        "openai:from-flag"
    );
}

#[test]
fn sections_this_port_does_not_use_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        &dir,
        "c.toml",
        "[sandbox]\nmode = \"docker\"\n[guardrails]\negress = \"redact\"\n\
         [providers.ollama]\nnum_ctx = 16384\n[mcp.servers.fs]\ncommand = \"x\"\n",
    );

    let config = load_from(&[file], &Overrides::default()).unwrap();
    assert_eq!(
        config.provider("ollama").base_url.as_deref(),
        Some("http://localhost:11434")
    );
}

#[test]
fn missing_files_are_skipped_and_malformed_ones_are_named() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.toml");
    let broken = write(&dir, "broken.toml", "[model\n");

    let config = load_from(&[missing], &Overrides::default()).unwrap();
    assert!(config.sources.is_empty());

    let err = load_from(std::slice::from_ref(&broken), &Overrides::default()).unwrap_err();
    let message = err.to_string();
    assert!(message.starts_with("Malformed config at "), "{message}");
    assert!(message.contains(&broken.display().to_string()), "{message}");
}

#[test]
fn ill_typed_values_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        &dir,
        "c.toml",
        "[providers.openrouter]\nread_timeout_s = \"slow\"\n",
    );

    let err = load_from(&[file], &Overrides::default()).unwrap_err();
    assert!(err.to_string().starts_with("Malformed config at "), "{err}");
}
