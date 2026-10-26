//! Layered configuration, matching the Python `config.py`.
//!
//! Resolution order (later overrides earlier):
//!
//! ```text
//! built-in defaults
//!   -> ~/.agent86/config.toml        (user)
//!   -> ./.agent86/config.toml        (project)
//!   -> environment variables         (AGENT86_*)
//!   -> explicit overrides            (CLI flags)
//! ```
//!
//! Secrets are never stored here: config only names the environment variable
//! that holds each provider's key (`api_key_env`).
//!
//! Only the sections the Rust port uses so far are read; other sections are
//! accepted and ignored.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;
use toml::{Table, Value};

/// The built-in defaults, seeded first so partial layers deep-merge onto them.
const DEFAULTS: &str = r#"
[model]
default = "anthropic:claude-opus-4-8"

[limits]
max_output_tokens = 8192

[sandbox]
mode = "subprocess"

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"

[providers.openai]
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"

[providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"
base_url = "https://openrouter.ai/api/v1"

[providers.groq]
api_key_env = "GROQ_API_KEY"
base_url = "https://api.groq.com/openai/v1"

[providers.ollama]
base_url = "http://localhost:11434"
num_ctx = 8192

[providers.llamacpp]
base_url = "http://localhost:8080"
"#;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub model: ModelConfig,
    pub limits: LimitsConfig,
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub pricing: PricingConfig,
    /// The config files that contributed, in the order they were read.
    #[serde(skip)]
    pub sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelConfig {
    pub default: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LimitsConfig {
    pub max_output_tokens: u32,
}

/// Where tool code runs (`[sandbox]`). Only `mode` is read so far.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// A local subprocess with resource limits.
    Subprocess,
    /// A throwaway container (opt-in).
    Docker,
}

impl SandboxMode {
    /// The config spelling, which is also how Python renders the mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subprocess => "subprocess",
            Self::Docker => "docker",
        }
    }
}

/// Per-model price overrides (`[pricing.models."<ref or id>"]`).
///
/// Keys are the `provider:model` ref or the bare model id; the ref form wins when
/// both are present. Dated snapshots resolve by prefix, so overriding
/// `claude-sonnet-5` also covers `claude-sonnet-5-20260101`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PricingConfig {
    #[serde(default)]
    pub models: BTreeMap<String, ModelPrice>,
}

/// A per-model price override, USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// One `[providers.<name>]` block.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub num_ctx: Option<u32>,
    /// A TCP connect that has not completed in this time is a down endpoint.
    #[serde(default = "default_connect_timeout_s")]
    pub connect_timeout_s: f64,
    /// The maximum gap between streamed chunks, not the total generation time.
    #[serde(default = "default_read_timeout_s")]
    pub read_timeout_s: f64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub prompt_cache: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key_env: None,
            base_url: None,
            num_ctx: None,
            connect_timeout_s: default_connect_timeout_s(),
            read_timeout_s: default_read_timeout_s(),
            max_retries: default_max_retries(),
            max_tokens: None,
            prompt_cache: true,
        }
    }
}

impl Config {
    /// The output cap for one call: the provider's `max_tokens`, else `limits.max_output_tokens`.
    pub fn max_output_tokens_for(&self, provider: &str) -> u32 {
        self.providers
            .get(provider)
            .and_then(|config| config.max_tokens)
            .filter(|&cap| cap > 0)
            .unwrap_or(if self.limits.max_output_tokens > 0 {
                self.limits.max_output_tokens
            } else {
                8192
            })
    }
}

/// Where the user and project config files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub user: Option<PathBuf>,
    pub project: PathBuf,
}

impl ConfigPaths {
    /// `~/.agent86/config.toml` and `./.agent86/config.toml`, as in Python.
    pub fn discover() -> Self {
        Self {
            user: home_dir().map(|home| home.join(".agent86").join("config.toml")),
            project: Path::new(".agent86").join("config.toml"),
        }
    }
}

/// The overrides that sit above the config files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `AGENT86_MODEL`.
    pub env_model: Option<String>,
    /// `AGENT86_SANDBOX`.
    pub env_sandbox: Option<String>,
    /// `--model`.
    pub model: Option<String>,
}

impl Overrides {
    /// Read the supported `AGENT86_*` environment variables; empty values are ignored.
    pub fn from_env(model: Option<String>) -> Self {
        Self {
            env_model: non_empty_env("AGENT86_MODEL"),
            env_sandbox: non_empty_env("AGENT86_SANDBOX"),
            model,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("Malformed config at {path}: {detail}")]
    Malformed { path: String, detail: String },
    #[error("Invalid config: {detail}")]
    Invalid { detail: String },
}

/// Resolve configuration from all layers.
pub fn load_config(paths: &ConfigPaths, overrides: &Overrides) -> Result<Config, ConfigError> {
    let mut merged: Table = DEFAULTS.parse().expect("built-in defaults are valid TOML");
    let mut sources = Vec::new();

    let files = paths.user.iter().chain(std::iter::once(&paths.project));
    for path in files {
        let layer = read_toml(path)?;
        if !layer.is_empty() {
            deep_merge(&mut merged, layer);
            sources.push(path.clone());
        }
    }

    // Environment variables first, then CLI flags, as in Python.
    let layers = [
        ("model", "default", &overrides.env_model),
        ("sandbox", "mode", &overrides.env_sandbox),
        ("model", "default", &overrides.model),
    ];
    for (section, key, value) in layers {
        if let Some(value) = value {
            let mut table = Table::new();
            table.insert(key.to_owned(), Value::String(value.clone()));
            let mut layer = Table::new();
            layer.insert(section.to_owned(), Value::Table(table));
            deep_merge(&mut merged, layer);
        }
    }

    let mut config: Config =
        Value::Table(merged)
            .try_into()
            .map_err(|error: toml::de::Error| ConfigError::Invalid {
                detail: error.message().to_owned(),
            })?;
    validate(&config)?;
    config.sources = sources;
    Ok(config)
}

/// Checks serde cannot express, matching the Python schema's constraints.
fn validate(config: &Config) -> Result<(), ConfigError> {
    for (model, price) in &config.pricing.models {
        for (field, value) in [
            ("input_per_mtok", price.input_per_mtok),
            ("output_per_mtok", price.output_per_mtok),
        ] {
            if !(value >= 0.0 && value.is_finite()) {
                return Err(ConfigError::Invalid {
                    detail: format!(
                        "pricing.models.\"{model}\".{field} must be a non-negative number, not {value}"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn read_toml(path: &Path) -> Result<Table, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(error) => {
            return Err(ConfigError::Malformed {
                path: path.display().to_string(),
                detail: error.to_string(),
            })
        }
    };
    text.parse()
        .map_err(|error: toml::de::Error| ConfigError::Malformed {
            path: path.display().to_string(),
            detail: error.to_string().trim_end().to_owned(),
        })
}

/// Recursively merge `overlay` into `base`; tables merge, everything else replaces.
fn deep_merge(base: &mut Table, overlay: Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(existing)), Value::Table(incoming)) => {
                deep_merge(existing, incoming)
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The home directory as Python's `Path.home()` resolves it.
fn home_dir() -> Option<PathBuf> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn default_connect_timeout_s() -> f64 {
    10.0
}

fn default_read_timeout_s() -> f64 {
    300.0
}

fn default_max_retries() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &Path) -> ConfigPaths {
        ConfigPaths {
            user: Some(dir.join("user.toml")),
            project: dir.join("project.toml"),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agent86-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_include_the_builtin_providers() {
        let dir = temp_dir("defaults");
        let config = load_config(&paths_in(&dir), &Overrides::default()).unwrap();

        assert_eq!(config.model.default, "anthropic:claude-opus-4-8");
        let openrouter = &config.providers["openrouter"];
        assert_eq!(
            openrouter.api_key_env.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            openrouter.base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(openrouter.read_timeout_s, 300.0);
        assert_eq!(config.max_output_tokens_for("openrouter"), 8192);
        assert!(config.sources.is_empty());
    }

    #[test]
    fn layers_deep_merge_and_overrides_win() {
        let dir = temp_dir("layers");
        std::fs::write(
            dir.join("user.toml"),
            "[model]\ndefault = \"openrouter:a\"\n[providers.openrouter]\nmax_tokens = 512\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("project.toml"),
            "[providers.openrouter]\nbase_url = \"http://localhost:9\"\n[providers.mine]\nbase_url = \"http://x\"\n[tools]\nanything = 1\n",
        )
        .unwrap();

        let config = load_config(&paths_in(&dir), &Overrides::default()).unwrap();
        let openrouter = &config.providers["openrouter"];
        assert_eq!(config.model.default, "openrouter:a");
        assert_eq!(openrouter.base_url.as_deref(), Some("http://localhost:9"));
        assert_eq!(
            openrouter.api_key_env.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(config.max_output_tokens_for("openrouter"), 512);
        assert!(config.providers.contains_key("groq"));
        assert_eq!(config.providers["mine"].max_retries, 2);
        assert_eq!(config.sources.len(), 2);

        let env = Overrides {
            env_model: Some("groq:b".to_owned()),
            ..Overrides::default()
        };
        assert_eq!(
            load_config(&paths_in(&dir), &env).unwrap().model.default,
            "groq:b"
        );

        let flag = Overrides {
            env_model: Some("groq:b".to_owned()),
            model: Some("openai:c".to_owned()),
            ..Overrides::default()
        };
        assert_eq!(
            load_config(&paths_in(&dir), &flag).unwrap().model.default,
            "openai:c"
        );
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let dir = temp_dir("malformed");
        std::fs::write(dir.join("project.toml"), "[model\n").unwrap();

        let error = load_config(&paths_in(&dir), &Overrides::default()).unwrap_err();
        let ConfigError::Malformed { path, .. } = &error else {
            panic!("{error:?}");
        };
        assert!(path.ends_with("project.toml"));
        assert!(error.to_string().starts_with("Malformed config at "));
    }

    #[test]
    fn pricing_overrides_load_and_negative_prices_are_rejected() {
        let dir = temp_dir("pricing");
        std::fs::write(
            dir.join("project.toml"),
            "[pricing.models.\"openrouter:openai/gpt-4o-mini\"]\ninput_per_mtok = 0.15\noutput_per_mtok = 0.6\n",
        )
        .unwrap();
        let config = load_config(&paths_in(&dir), &Overrides::default()).unwrap();
        let price = config.pricing.models["openrouter:openai/gpt-4o-mini"];
        assert_eq!(price.input_per_mtok, 0.15);
        assert_eq!(price.output_per_mtok, 0.6);

        std::fs::write(
            dir.join("project.toml"),
            "[pricing.models.cheap]\ninput_per_mtok = -1\noutput_per_mtok = 0\n",
        )
        .unwrap();
        let error = load_config(&paths_in(&dir), &Overrides::default()).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error:?}");
        assert!(error.to_string().contains("input_per_mtok"), "{error}");
    }

    #[test]
    fn sandbox_mode_defaults_to_subprocess_and_rejects_unknown_values() {
        let dir = temp_dir("sandbox");
        let config = load_config(&paths_in(&dir), &Overrides::default()).unwrap();
        assert_eq!(config.sandbox.mode, SandboxMode::Subprocess);

        let env = Overrides {
            env_sandbox: Some("docker".to_owned()),
            ..Overrides::default()
        };
        assert_eq!(
            load_config(&paths_in(&dir), &env).unwrap().sandbox.mode,
            SandboxMode::Docker
        );

        std::fs::write(
            dir.join("project.toml"),
            "[sandbox]
mode = \"vm\"
",
        )
        .unwrap();
        let error = load_config(&paths_in(&dir), &Overrides::default()).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error:?}");
    }
}
