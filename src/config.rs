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

use crate::types::ApprovalMode;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;
use toml::{Table, Value};

/// The built-in defaults, seeded first so partial layers deep-merge onto them.
const DEFAULTS: &str = r#"
[model]
default = "anthropic:claude-opus-4-8"
router = "off"

[model.route]
cheap = "ollama:llama3.1"
frontier = "anthropic:claude-opus-4-8"

[limits]
max_steps = 40
max_cost_usd = 5.0
max_wall_clock_s = 900
max_consecutive_errors = 3
max_context_tokens = 0
max_output_tokens = 8192
context_reserve_tokens = 4096
compaction = "summarize"
parallel_tools = true
tool_timeout_s = 60

[sandbox]
mode = "subprocess"
docker_image = "python:3.12-slim"
docker_memory = "512m"
docker_cpus = "1"
docker_network = false
env_passthrough = []

[mcp]
enabled = true

[skills]
enabled = true
paths = []

[agents]
enabled = true
max_depth = 2
max_steps = 8

[observability]
trace = true
path = "~/.agent86/traces"
otel = false
otel_exporter = "otlp"
redact = "secrets"
max_field_chars = 2000
max_trace_bytes = 50000000
keep_traces = 5

[memory]
enabled = true
path = "~/.agent86/memory.db"
embeddings = "sentence-transformers:all-MiniLM-L6-v2"
hf_offline = true
retention_max_episodes = 1000
retention_max_sessions = 500
retention_max_age_days = 0.0

[tools]
web_allow_private = false
web_max_chars = 8000
mention_max_bytes = 200000

[ui]
spinner = true
history_file = "~/.agent86/history"
history_size = 1000
markdown = true

[guardrails]
approval = "ask"
ingress = "warn"
egress = "warn"
scan_observations = true

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
    pub guardrails: GuardrailsConfig,
    pub tools: ToolsConfig,
    pub memory: MemoryConfig,
    pub observability: ObservabilityConfig,
    pub mcp: McpConfig,
    pub skills: SkillsConfig,
    pub agents: AgentsConfig,
    /// MCP servers in config order, from `[mcp_servers.<name>]` or `[mcp.servers.<name>]`.
    #[serde(default)]
    pub mcp_servers: indexmap::IndexMap<String, McpServerConfig>,
    #[serde(default)]
    pub providers: indexmap::IndexMap<String, ProviderConfig>,
    #[serde(default)]
    pub pricing: PricingConfig,
    pub ui: UiConfig,
    /// Every layer merged, before deserializing; `/config` shows it.
    #[serde(skip)]
    pub table: Table,
    /// The config files that contributed, in the order they were read.
    #[serde(skip)]
    pub sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelConfig {
    pub default: String,
    /// `off` or `triage`. Only reported: triage routing is not ported.
    pub router: RouterMode,
    pub route: ModelRoute,
    /// `[model.context_window]`: tokens of context per `provider:model` ref or bare
    /// model id, overriding the built-in table.
    #[serde(default)]
    pub context_window: BTreeMap<String, u64>,
}

/// `[model] router`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterMode {
    Off,
    Triage,
}

impl RouterMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Triage => "triage",
        }
    }
}

/// `[model.route]`: the models triage routing would choose between.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelRoute {
    pub cheap: String,
    pub frontier: String,
}

/// Interactive-surface settings (`[ui]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UiConfig {
    /// Use the full-screen interface when the terminal can host it. Python's
    /// pre-v0.6 spelling, `status_line`, is mapped onto it.
    #[serde(default = "default_true")]
    pub tui: bool,
    pub spinner: bool,
    /// Where submitted prompts are recorded; `~` and `$VARS` expand at use time.
    pub history_file: String,
    /// How many entries `history_file` keeps; `0` disables the cap.
    pub history_size: i64,
    /// Render answers as Markdown in the full-screen interface.
    pub markdown: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LimitsConfig {
    /// Model calls one turn may make before the circuit trips.
    pub max_steps: u32,
    /// Cumulative cost (USD) at which the circuit trips; `0` disables the cap.
    pub max_cost_usd: f64,
    /// Seconds one turn may run before the circuit trips; `0` disables the limit.
    pub max_wall_clock_s: u64,
    /// Failed tool results tolerated in a row; one more trips the circuit.
    pub max_consecutive_errors: u32,
    /// An optional hard cap on the conversation's tokens; `0` derives the budget
    /// from the model's real window alone.
    pub max_context_tokens: u64,
    pub max_output_tokens: u32,
    /// Headroom kept free inside the window on top of the output cap: slack for
    /// the provider counting tokens differently than the estimate does.
    pub context_reserve_tokens: u64,
    /// What happens to the oldest turns when the conversation outgrows its budget.
    pub compaction: CompactionMode,
    /// Run a step's read-only tool calls concurrently.
    pub parallel_tools: bool,
    /// Seconds a single sandboxed tool may run before its process tree is killed.
    pub tool_timeout_s: u64,
}

/// `[skills]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SkillsConfig {
    pub enabled: bool,
    /// Extra directories searched after the project's and the user's.
    pub paths: Vec<String>,
}

/// `[agents]`: sub-agent spawning through the `delegate` tool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentsConfig {
    pub enabled: bool,
    /// How deep delegation may nest (the main agent is depth 0).
    pub max_depth: u32,
    /// Each sub-agent's step cap, kept below `limits.max_steps`.
    pub max_steps: u32,
}

/// `[mcp]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpConfig {
    pub enabled: bool,
}

/// One MCP server, reached over `stdio` (a local subprocess) or, in Python, `sse`/`http`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: indexmap::IndexMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: indexmap::IndexMap<String, String>,
    /// `stdio`, `sse`, or `http`; inferred from `command` or `url` when omitted.
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_mcp_connect_timeout_s")]
    pub connect_timeout_s: f64,
    #[serde(default = "default_mcp_call_timeout_s")]
    pub call_timeout_s: f64,
    #[serde(default = "default_mcp_max_result_chars")]
    pub max_result_chars: usize,
}

impl McpServerConfig {
    /// Validate and fill in `transport`, with Python's messages.
    pub fn resolve_transport(&mut self) -> Result<(), String> {
        let (has_command, has_url) = (
            self.command.as_deref().is_some_and(|c| !c.is_empty()),
            self.url.as_deref().is_some_and(|u| !u.is_empty()),
        );
        if has_command && has_url {
            return Err("MCP server sets both 'command' and 'url'; use one transport".to_owned());
        }
        if !has_command && !has_url {
            return Err("MCP server needs 'command' (stdio) or 'url' (sse/http)".to_owned());
        }
        let given = self
            .transport
            .clone()
            .unwrap_or_else(|| if has_command { "stdio" } else { "http" }.to_owned());
        let mut transport = given.to_lowercase().replace('-', "_");
        if transport == "streamable_http" {
            transport = "http".to_owned();
        }
        if !matches!(transport.as_str(), "stdio" | "sse" | "http") {
            return Err(format!("unknown MCP transport '{given}' (stdio|sse|http)"));
        }
        if transport == "stdio" && !has_command {
            return Err("stdio transport requires 'command'".to_owned());
        }
        if transport != "stdio" && !has_url {
            return Err(format!("{transport} transport requires 'url'"));
        }
        for (field, value) in [
            ("connect_timeout_s", self.connect_timeout_s),
            ("call_timeout_s", self.call_timeout_s),
        ] {
            if !(value > 0.0 && value.is_finite()) {
                return Err(format!("{field} must be greater than 0"));
            }
        }
        if self.max_result_chars == 0 {
            return Err("max_result_chars must be greater than 0".to_owned());
        }
        self.transport = Some(transport);
        Ok(())
    }

    /// The resolved transport (`stdio`, `sse`, or `http`).
    pub fn transport(&self) -> &str {
        self.transport.as_deref().unwrap_or("stdio")
    }
}

fn default_mcp_connect_timeout_s() -> f64 {
    30.0
}

fn default_mcp_call_timeout_s() -> f64 {
    120.0
}

fn default_mcp_max_result_chars() -> usize {
    8000
}

/// Flight-recorder and OpenTelemetry settings (`[observability]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ObservabilityConfig {
    /// Write the JSONL flight recorder.
    pub trace: bool,
    /// The trace directory; `~` expands.
    pub path: String,
    /// Emit OpenTelemetry spans (`crate::otel`).
    #[serde(default)]
    pub otel: bool,
    /// Where the spans go.
    #[serde(default)]
    pub otel_exporter: OtelExporter,
    /// The collector, overriding `OTEL_EXPORTER_OTLP_ENDPOINT` when set.
    #[serde(default)]
    pub otel_endpoint: Option<String>,
    pub redact: RedactMode,
    /// Free-text fields are clipped to this many characters.
    pub max_field_chars: usize,
    /// The live trace rotates at this size; `0` disables rotation.
    pub max_trace_bytes: u64,
    /// Rotated generations kept; `0` keeps no history.
    pub keep_traces: usize,
}

/// Where OpenTelemetry spans go (`[observability] otel_exporter`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtelExporter {
    /// A collector over OTLP, honouring the standard `OTEL_*` variables.
    #[default]
    Otlp,
    /// Printed to stderr, for local debugging.
    Console,
    /// Recorded, exported nowhere.
    None,
}

/// What the recorder may write (`[observability] redact`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactMode {
    /// Scrub secret-shaped values and clip the big free-text fields.
    Secrets,
    /// Write events exactly as the loop emitted them.
    None,
}

/// Memory settings (`[memory]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MemoryConfig {
    pub enabled: bool,
    /// The SQLite database, shared with the Python implementation; `~` expands.
    pub path: String,
    /// The embedder spec: `hash:<dim>`, or `sentence-transformers:<model>`
    /// (falling back to `hash:256` when the model cannot load).
    pub embeddings: String,
    /// Skip the Hugging Face update check for a cached model.
    pub hf_offline: bool,
    /// Retention caps for the episode and session log, applied when the harness
    /// starts; 0 disables a cap. Curated facts are never pruned automatically.
    pub retention_max_episodes: i64,
    pub retention_max_sessions: i64,
    pub retention_max_age_days: f64,
}

/// Tool settings (`[tools]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolsConfig {
    /// Let `web_fetch` reach private, loopback, and link-local addresses.
    pub web_allow_private: bool,
    /// The User-Agent `web_fetch` sends; defaults to agent86's own.
    #[serde(default = "crate::tools::web::default_user_agent")]
    pub web_user_agent: String,
    /// Characters of page text `web_fetch` returns at most; `0` disables the cap.
    pub web_max_chars: usize,
    /// Largest file an `@path` mention inlines; `0` disables the cap.
    pub mention_max_bytes: u64,
}

/// Safety settings (`[guardrails]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GuardrailsConfig {
    /// What happens before a side-effecting tool runs: `auto`, `ask`, or `deny`.
    pub approval: ApprovalMode,
    /// Scanning of user input for injection and PII.
    pub ingress: IngressMode,
    /// Scanning of model output for secrets and PII.
    pub egress: EgressMode,
    /// Wrap tool output that looks like injected instructions as untrusted data.
    pub scan_observations: bool,
}

/// `[guardrails] ingress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressMode {
    Off,
    /// Scan and report findings.
    Warn,
    /// Also refuse turns carrying prompt injection.
    Block,
}

/// `[limits] compaction`: what happens to the oldest turns when the
/// conversation outgrows its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// Replace the oldest span with a model-written summary.
    Summarize,
    /// Silently drop the oldest span.
    Drop,
}

/// `[guardrails] egress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    Off,
    /// Scan and report findings.
    Warn,
    /// Also rewrite secrets and PII out of the text.
    Redact,
}

/// Where tool code runs (`[sandbox]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    pub docker_image: String,
    pub docker_memory: String,
    pub docker_cpus: String,
    /// Containers run with `--network none` unless this is true.
    pub docker_network: bool,
    /// Extra environment variable names forwarded into tool subprocesses. Names
    /// only: values are read at spawn time, and credential-looking names are refused.
    pub env_passthrough: Vec<String>,
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
    /// `AGENT86_APPROVAL`.
    pub env_approval: Option<String>,
    /// `--model`.
    pub model: Option<String>,
    /// `--sandbox`.
    pub sandbox: Option<String>,
    /// `--approval`.
    pub approval: Option<String>,
}

impl Overrides {
    /// Read the supported `AGENT86_*` environment variables; empty values are ignored.
    /// `model`, `sandbox`, and `approval` are the `--model`, `--sandbox`, and
    /// `--approval` flags.
    pub fn from_env(
        model: Option<String>,
        sandbox: Option<String>,
        approval: Option<String>,
    ) -> Self {
        Self {
            env_model: non_empty_env("AGENT86_MODEL"),
            env_sandbox: non_empty_env("AGENT86_SANDBOX"),
            env_approval: non_empty_env("AGENT86_APPROVAL"),
            model,
            sandbox,
            approval,
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
        let layer = normalize(read_toml(path)?);
        if !layer.is_empty() {
            deep_merge(&mut merged, layer);
            sources.push(path.clone());
        }
    }

    // Environment variables first, then CLI flags, as in Python.
    let layers = [
        ("model", "default", &overrides.env_model),
        ("sandbox", "mode", &overrides.env_sandbox),
        ("guardrails", "approval", &overrides.env_approval),
        ("model", "default", &overrides.model),
        ("sandbox", "mode", &overrides.sandbox),
        ("guardrails", "approval", &overrides.approval),
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

    accept_legacy_status_line(&mut merged);
    let mut config: Config =
        Value::Table(merged.clone())
            .try_into()
            .map_err(|error: toml::de::Error| ConfigError::Invalid {
                detail: error.message().to_owned(),
            })?;
    validate(&mut config)?;
    config.sources = sources;
    config.table = merged;
    Ok(config)
}

/// Checks serde cannot express, matching the Python schema's constraints.
/// Map `[mcp.servers.<name>]` onto `mcp_servers`, as Python's `_normalize`.
fn normalize(mut layer: Table) -> Table {
    if let Some(Value::Table(mcp)) = layer.get_mut("mcp") {
        if let Some(servers) = mcp.remove("servers") {
            let empty = mcp.is_empty();
            if empty {
                layer.remove("mcp");
            }
            layer.insert("mcp_servers".to_owned(), servers);
        }
    }
    layer
}

/// Map `[ui] status_line` onto `tui` (an explicit `tui` wins), as Python's
/// `UIConfig` validator does on the merged layers.
fn accept_legacy_status_line(merged: &mut Table) {
    if let Some(Value::Table(ui)) = merged.get_mut("ui") {
        if let Some(legacy) = ui.remove("status_line") {
            ui.entry("tui").or_insert(legacy);
        }
    }
}

fn validate(config: &mut Config) -> Result<(), ConfigError> {
    for (name, server) in &mut config.mcp_servers {
        server
            .resolve_transport()
            .map_err(|detail| ConfigError::Invalid {
                detail: format!("mcp_servers.{name}: {detail}"),
            })?;
    }
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
pub fn home_dir() -> Option<PathBuf> {
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
    fn ui_settings_default_and_accept_the_legacy_status_line_key() {
        let dir = temp_dir("ui");
        let paths = paths_in(&dir);
        let config = load_config(&paths, &Overrides::default()).unwrap();
        assert!(config.ui.tui);
        assert_eq!(config.ui.history_file, "~/.agent86/history");
        assert_eq!(config.ui.history_size, 1000);
        assert_eq!(config.tools.mention_max_bytes, 200_000);
        assert_eq!(config.model.router, RouterMode::Off);
        assert_eq!(config.model.route.cheap, "ollama:llama3.1");

        // Pre-v0.6 configs spelled `tui` as `status_line`...
        std::fs::write(&paths.project, "[ui]\nstatus_line = false\n").unwrap();
        assert!(!load_config(&paths, &Overrides::default()).unwrap().ui.tui);
        // ...and an explicit `tui` in any layer wins over it.
        std::fs::write(dir.join("user.toml"), "[ui]\ntui = true\n").unwrap();
        let config = load_config(&paths, &Overrides::default()).unwrap();
        assert!(config.ui.tui);
        assert!(!config.table["ui"]
            .as_table()
            .unwrap()
            .contains_key("status_line"));
        let _ = std::fs::remove_dir_all(&dir);
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
        // `--sandbox` wins over `AGENT86_SANDBOX`, as Python's CLI overrides do.
        let flag = Overrides {
            sandbox: Some("subprocess".to_owned()),
            ..env
        };
        assert_eq!(
            load_config(&paths_in(&dir), &flag).unwrap().sandbox.mode,
            SandboxMode::Subprocess
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
