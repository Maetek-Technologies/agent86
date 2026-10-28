//! Building everything a turn needs from the configuration, shared by
//! `agent86 run` and the REPL, as Python builds one `Harness(config)` for both.

use crate::agent::Harness;
use crate::approval::{ApprovalGate, ApprovalPrompt};
use crate::config::{Config, ConfigPaths};
use crate::guardrails::Guardrails;
use crate::mcp::McpManager;
use crate::memory::embeddings::build_embedder;
use crate::observability::Recorder;
use crate::prompt::build_system_prompt;
use crate::provider::capabilities::context_window_with;
use crate::provider::remote_windows::{cache_path, openrouter_context_window};
use crate::provider::{provider_for_ref, Provider};
use crate::secrets::OsKeyring;
use crate::session::{expand_home, SessionStore};
use crate::tools::sandbox::build_executor;
use crate::tools::web::WebSettings;
use crate::tools::{default_registry, ToolContext};
use crate::types::ModelRef;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// What to do next when a model cannot be built: a bad ref, an unknown provider, a missing key.
///
/// Python points at `agent86 models`, which is not ported yet.
pub const MODEL_HELP: &str = "Pick an OpenAI-compatible provider with `--model provider:model` \
                              (e.g. `openrouter:openai/gpt-4o-mini`) and set its API key environment variable \
                              or store the key in the OS keyring.";

/// The provider for a `provider:model` ref, with the parsed ref.
pub fn build_provider(
    config: &Config,
    reference: &str,
) -> Result<(Box<dyn Provider>, ModelRef), String> {
    let reference = ModelRef::parse(reference).map_err(|error| error.to_string())?;
    let provider = provider_for_ref(&reference, config).map_err(|error| error.to_string())?;
    Ok((provider, reference))
}

/// Why the harness could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// The model could not be built; [`MODEL_HELP`] says what to do.
    Model(String),
    /// The workspace (the current directory) could not be opened.
    Workspace(String),
}

/// A built harness and what came with it.
pub struct Assembly {
    pub harness: Harness,
    /// The model's `provider:model` ref, which usage is priced under.
    pub model_ref: ModelRef,
    /// The session store, or `None` when memory is off or could not be opened.
    pub store: Option<SessionStore>,
    /// Why memory is unavailable or degraded (an embedder fallback), as Python's
    /// `memory_note`.
    pub memory_note: Option<String>,
    /// Why the configured sandbox was replaced, as Python's `sandbox_note`.
    pub sandbox_note: Option<String>,
}

/// Build the harness for `config.model.default`, jailed to the current directory,
/// with the MCP servers started and `prompt` asking for approvals.
pub fn assemble(config: &Config, prompt: Option<ApprovalPrompt>) -> Result<Assembly, SetupError> {
    let (provider, model_ref) =
        build_provider(config, &config.model.default).map_err(SetupError::Model)?;

    // Tools are jailed to the directory `agent86` runs in, as in Python.
    let mut context = std::env::current_dir()
        .and_then(|dir| ToolContext::new(&dir))
        .map_err(|error| SetupError::Workspace(format!("could not open the workspace: {error}")))?;
    context.policy.timeout = Duration::from_secs(config.limits.tool_timeout_s);
    context.policy.env_passthrough = config.sandbox.env_passthrough.clone();
    context.web = WebSettings {
        allow_private: config.tools.web_allow_private,
        user_agent: config.tools.web_user_agent.clone(),
        max_chars: config.tools.web_max_chars,
    };
    let (executor, sandbox_note) = build_executor(&config.sandbox);
    context.executor = executor;

    // Sessions, episodes, and facts live in the memory database; if it cannot be
    // opened, run statelessly.
    let (store, memory_note) = open_memory(config);
    let recorder = Recorder::from_config(&config.observability);
    if let Some(store) = &store {
        enforce_retention(config, store, &recorder);
    }
    context.memory = store.clone();

    // MCP servers that fail to start contribute no tools; their notes say why.
    let registry = default_registry(store.is_some());
    let mcp = McpManager::start(config).map(Arc::new);
    if let Some(manager) = &mcp {
        manager.attach(&registry);
    }
    let max_tokens = config.max_output_tokens_for(&model_ref.provider);
    let context_window = model_context_window(config, &model_ref.to_string());
    let harness = Harness {
        provider,
        registry,
        context,
        limits: config.limits.clone(),
        system: build_system_prompt(config),
        max_tokens,
        context_window,
        gate: ApprovalGate::new(config.guardrails.approval, prompt),
        guardrails: Guardrails::new(&config.guardrails),
        recorder,
        memory: store.clone(),
        mcp,
        cancel: Default::default(),
    };
    Ok(Assembly {
        harness,
        model_ref,
        store,
        memory_note,
        sandbox_note,
    })
}

/// Tokens of context the model behind `model_ref` can hold, as Python's
/// `context_window_for`: OpenRouter is asked for its own figure (memoized, and
/// cached beside the user config for a day), then the overrides and the table.
pub fn model_context_window(config: &Config, model_ref: &str) -> u64 {
    let cache = ConfigPaths::discover()
        .user
        .as_deref()
        .and_then(Path::parent)
        .map(cache_path);
    context_window_with(model_ref, config, |provider, model| {
        openrouter_context_window(provider, model, config, cache.as_deref(), &OsKeyring)
    })
}

/// The memory store for `[memory]`, or `None` with the reason, as Python's
/// `_auto_memory`: a locked or unwritable database is a reason to run without
/// recall, not a reason the user cannot talk to a model at all. The note also
/// carries an embedder fallback.
pub fn open_memory(config: &Config) -> (Option<SessionStore>, Option<String>) {
    if !config.memory.enabled {
        return (None, None);
    }
    let (embedder, note) = build_embedder(&config.memory.embeddings, config.memory.hf_offline);
    match SessionStore::open_with(&expand_home(&config.memory.path), embedder) {
        Ok(store) => (Some(store), note),
        Err(error) => (
            None,
            Some(format!(
                "unavailable, running without recall or session history. {error}"
            )),
        ),
    }
}

/// Prune the episode and session log to its caps at startup, as Python's
/// `_enforce_retention`, recording what was removed.
fn enforce_retention(config: &Config, store: &SessionStore, recorder: &Recorder) {
    let memory = &config.memory;
    if let Ok(removed) = store.enforce_retention(
        memory.retention_max_episodes,
        memory.retention_max_sessions,
        memory.retention_max_age_days,
    ) {
        if !removed.is_empty() {
            let fields = removed
                .into_iter()
                .map(|(table, count)| (table, serde_json::json!(count)))
                .collect();
            recorder.event("memory", "retention_prune", fields);
        }
    }
}
