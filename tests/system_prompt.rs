//! The system prompt must match the Python reference exactly.

#[path = "../crates/agent86-core/tests/support/mod.rs"]
mod support;

use agent86::config::{load_config, ConfigPaths, Overrides};
use agent86::prompt::{build_system_prompt, BASE_IDENTITY};
use agent86::types::Role;
use support::fixture;

#[test]
fn the_identity_matches_the_text_extracted_from_python() {
    assert_eq!(BASE_IDENTITY, fixture("prompt/base_identity.txt"));
}

#[test]
fn the_prompt_is_the_identity_plus_the_environment_line() {
    let dir = std::env::temp_dir().join(format!("agent86-prompt-{}", std::process::id()));
    let paths = ConfigPaths {
        user: Some(dir.join("missing-user.toml")),
        project: dir.join("missing-project.toml"),
        project_trust: agent86::config::ProjectTrust::Trusted,
    };
    let overrides = Overrides {
        model: Some("openrouter:openai/gpt-4o-mini".to_owned()),
        env_sandbox: Some("docker".to_owned()),
        ..Overrides::default()
    };
    let config = load_config(&paths, &overrides).unwrap();

    let message = build_system_prompt(&config, &Default::default());

    assert_eq!(message.role, Role::System);
    let expected = format!(
        // Beyond Python: the platform, before the sandbox.
        "{}\n\nEnvironment: agent86 v{}. Default model: openrouter:openai/gpt-4o-mini. \
         Platform: Linux, in a container; run_command runs each command with /bin/sh in the \
         workspace, and Python is `python`. Sandbox: docker.",
        fixture("prompt/base_identity.txt").trim(),
        env!("CARGO_PKG_VERSION"),
    );
    assert_eq!(message.content, expected);
}
