//! Layered configuration, limited to what the one-shot model turn needs.
//!
//! Resolution follows the Python reference: built-in defaults, then
//! `~/.agent86/config.toml`, then `./.agent86/config.toml`, then `AGENT86_MODEL`,
//! then the `--model` flag. Tables deep-merge, so a project file that sets one
//! provider's `base_url` keeps that provider's `api_key_env`. Keys this port does
//! not use yet are accepted and ignored rather than rejected.

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use thiserror::Error;
use toml::{Table, Value};

/// The Python reference's default model.
pub const DEFAULT_MODEL: &str = "anthropic:claude-opus-4-8";

/// A TCP connect that has not completed in 10s is a down endpoint, not a slow one.
pub const DEFAULT_CONNECT_TIMEOUT_S: f64 = 10.0;
/// The maximum gap between streamed chunks, not a total-request deadline.
pub const DEFAULT_READ_TIMEOUT_S: f64 = 300.0;
pub const DEFAULT_MAX_RETRIES: u32 = 2;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_s: f64,
    #[serde(default = "default_read_timeout")]
    pub read_timeout_s: f64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key_env: None,
            base_url: None,
            connect_timeout_s: DEFAULT_CONNECT_TIMEOUT_S,
            read_timeout_s: DEFAULT_READ_TIMEOUT_S,
            max_retries: DEFAULT_MAX_RETRIES,
            max_tokens: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The `provider:model` reference used when no override is given.
    pub default_model: String,
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Config files that existed and were merged, in resolution order.
    pub sources: Vec<PathBuf>,
}

impl Config {
    /// The provider section for `name`, or an all-defaults section when absent.
    pub fn provider(&self, name: &str) -> ProviderConfig {
        self.providers.get(name).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Malformed config at {path}: {message}")]
    Malformed { path: PathBuf, message: String },
    #[error("Could not read config at {path}: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Overrides applied after the files, highest priority last.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// `AGENT86_MODEL`.
    pub env_model: Option<String>,
    /// `--model`.
    pub flag_model: Option<String>,
}

impl Overrides {
    /// Read the environment-variable overrides this port understands.
    pub fn from_env(flag_model: Option<String>) -> Self {
        Self {
            env_model: std::env::var("AGENT86_MODEL")
                .ok()
                .filter(|value| !value.is_empty()),
            flag_model,
        }
    }
}

/// `~/.agent86/config.toml`, when a home directory is known.
pub fn user_config_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".agent86").join("config.toml"))
}

/// `./.agent86/config.toml`, relative to the working directory.
pub fn project_config_path() -> PathBuf {
    Path::new(".agent86").join("config.toml")
}

/// Resolve configuration from the default user and project paths.
pub fn load(overrides: &Overrides) -> Result<Config, ConfigError> {
    let mut paths: Vec<PathBuf> = user_config_path().into_iter().collect();
    paths.push(project_config_path());
    load_from(&paths, overrides)
}

/// Resolve configuration from explicit file paths (missing files are skipped).
pub fn load_from(paths: &[PathBuf], overrides: &Overrides) -> Result<Config, ConfigError> {
    let mut merged = defaults();
    let mut sources = Vec::new();

    for path in paths {
        if let Some(raw) = read_toml(path)? {
            deep_merge(&mut merged, raw);
            sources.push(path.clone());
        }
    }

    let model_override = overrides
        .flag_model
        .as_ref()
        .or(overrides.env_model.as_ref());
    if let Some(model) = model_override {
        let mut model_table = Table::new();
        model_table.insert("default".into(), Value::String(model.clone()));
        let mut overlay = Table::new();
        overlay.insert("model".into(), Value::Table(model_table));
        deep_merge(&mut merged, overlay);
    }

    let resolved: Resolved = Value::Table(merged)
        .try_into()
        .map_err(|err: toml::de::Error| ConfigError::Malformed {
            path: sources.last().cloned().unwrap_or_default(),
            message: err.message().to_owned(),
        })?;

    Ok(Config {
        default_model: resolved.model.default,
        providers: resolved.providers,
        sources,
    })
}

#[derive(Deserialize)]
struct Resolved {
    model: ModelSection,
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Deserialize)]
struct ModelSection {
    default: String,
}

fn defaults() -> Table {
    let mut providers = Table::new();
    let mut add = |name: &str, api_key_env: Option<&str>, base_url: Option<&str>| {
        let mut section = Table::new();
        if let Some(env) = api_key_env {
            section.insert("api_key_env".into(), Value::String(env.into()));
        }
        if let Some(url) = base_url {
            section.insert("base_url".into(), Value::String(url.into()));
        }
        providers.insert(name.into(), Value::Table(section));
    };
    add("anthropic", Some("ANTHROPIC_API_KEY"), None);
    add(
        "openai",
        Some("OPENAI_API_KEY"),
        Some("https://api.openai.com/v1"),
    );
    add(
        "openrouter",
        Some("OPENROUTER_API_KEY"),
        Some("https://openrouter.ai/api/v1"),
    );
    add(
        "groq",
        Some("GROQ_API_KEY"),
        Some("https://api.groq.com/openai/v1"),
    );
    add("ollama", None, Some("http://localhost:11434"));
    add("llamacpp", None, Some("http://localhost:8080"));

    let mut model = Table::new();
    model.insert("default".into(), Value::String(DEFAULT_MODEL.into()));

    let mut root = Table::new();
    root.insert("model".into(), Value::Table(model));
    root.insert("providers".into(), Value::Table(providers));
    root
}

fn read_toml(path: &Path) -> Result<Option<Table>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Unreadable {
                path: path.to_owned(),
                source,
            })
        }
    };
    text.parse::<Table>()
        .map(Some)
        .map_err(|err| ConfigError::Malformed {
            path: path.to_owned(),
            message: err.message().to_owned(),
        })
}

/// Recursively merge `overlay` into `base`; tables merge, everything else replaces.
fn deep_merge(base: &mut Table, overlay: Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(existing)), Value::Table(incoming)) => {
                deep_merge(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

const fn default_connect_timeout() -> f64 {
    DEFAULT_CONNECT_TIMEOUT_S
}

const fn default_read_timeout() -> f64 {
    DEFAULT_READ_TIMEOUT_S
}

const fn default_max_retries() -> u32 {
    DEFAULT_MAX_RETRIES
}
