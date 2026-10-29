//! Building everything a turn needs from the configuration, shared by
//! `agent86 run` and the REPL, as Python builds one `Harness(config)` for both.

use crate::agent::Harness;
use crate::approval::{ApprovalGate, ApprovalPrompt};
use crate::config::{Config, ConfigPaths, RouterMode};
use crate::guardrails::Guardrails;
use crate::limits::output_setting;
use crate::mcp::McpManager;
use crate::memory::embeddings::build_embedder;
use crate::observability::Recorder;
use crate::prompt::build_system_prompt;
use crate::provider::capabilities::{context_window_with, ollama_num_ctx};
use crate::provider::remote_windows::{cache_path, discover_context_window, ollama_context_window};
use crate::provider::{provider_for_ref, Provider};
use crate::router::{RoutedModel, Router};
use crate::secrets::OsKeyring;
use crate::session::{expand_home, SessionStore};
use crate::skills::{discover_skills, skill_roots};
use crate::tools::sandbox::build_executor;
use crate::tools::web::WebSettings;
use crate::tools::{default_registry, ToolContext};
use crate::types::ModelRef;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// What to do next when a model cannot be built: a bad ref, an unknown provider, a missing key.
pub const MODEL_HELP: &str = "Run `agent86 models` to see the configured providers and which \
                              of them has a key, then pick one with `--model provider:model`.";

/// The plain loop's version, when the session cannot start, as Python's `run_repl`.
pub const REPL_MODEL_HELP: &str = "`agent86 models` lists the configured providers and which \
                                   of them has a key; `agent86 --model provider:model` picks \
                                   another. Then retry.";

/// The provider for a `provider:model` ref, with the parsed ref.
///
/// An Ollama model is asked for the window [`model_context_window`] reports
/// ([`ollama_num_ctx`]), so the request and the status line agree.
pub fn build_provider(
    config: &Config,
    reference: &str,
) -> Result<(Box<dyn Provider>, ModelRef), String> {
    let reference = ModelRef::parse(reference).map_err(|error| error.to_string())?;
    let provider = if reference.provider == "ollama" {
        let num_ctx = ollama_num_ctx(config.providers.get("ollama"), || {
            ollama_context_window("ollama", &reference.model, config)
        });
        let mut config = config.clone();
        config
            .providers
            .entry("ollama".to_owned())
            .or_default()
            .num_ctx = Some(num_ctx);
        provider_for_ref(&reference, &config)
    } else {
        provider_for_ref(&reference, config)
    }
    .map_err(|error| error.to_string())?;
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
    let recorder =
        Recorder::from_config(&config.observability).with_secrets(context.secrets.clone());
    if let Some(store) = &store {
        enforce_retention(config, store, &recorder);
    }
    context.memory = store.clone();

    // Skills come from the workspace and the home directory; their folders join the
    // jail so the files a skill bundles can be read.
    let workspace = crate::tools::plain_path(context.policy.workspace());
    let home = crate::config::home_dir();
    let skills = Arc::new(discover_skills(config, &workspace, home.as_deref()));
    context
        .policy
        .allow(skill_roots(config, &workspace, home.as_deref()));
    context.skills = Arc::clone(&skills);

    // Secrets are asked for where approvals are: at a terminal, hidden. With nobody
    // there, `request_secret` is not offered at all (the interface installs its own
    // panel over this prompt).
    let interactive = prompt.is_some();
    if interactive {
        context
            .secrets
            .set_prompt(Some(Box::new(crate::vault::terminal_prompt)));
    }

    // MCP servers that fail to start contribute no tools; their notes say why.
    let registry = default_registry(
        store.is_some(),
        !skills.is_empty(),
        config.agents.enabled,
        interactive,
    );
    let mcp = McpManager::start(config).map(Arc::new);
    if let Some(manager) = &mcp {
        manager.attach(&registry);
    }
    let context_window = model_context_window(config, &model_ref.to_string());
    let mut harness = Harness {
        provider: Arc::from(provider),
        router: build_router(config),
        tracer: crate::otel::Tracer::from_config(&config.observability),
        registry,
        context,
        limits: config.limits.clone(),
        agents: config.agents.clone(),
        system: build_system_prompt(config, &skills),
        max_tokens: 0,
        session_limits: Default::default(),
        context_window,
        gate: ApprovalGate::new(config.guardrails.approval, prompt),
        guardrails: Guardrails::new(&config.guardrails),
        recorder,
        memory: store.clone(),
        mcp,
        cancel: Default::default(),
    };
    harness.configure_output(config, &model_ref.provider);
    Ok(Assembly {
        harness,
        model_ref,
        store,
        memory_note,
        sandbox_note,
    })
}

/// The triage router for `config`, as Python's `ModelRouter`: routed models are
/// built from the config the session started with, as Python's harness keeps it.
pub fn build_router(config: &Config) -> Router {
    let routing = config.clone();
    let route = &config.model.route;
    Router::new(
        config.model.router == RouterMode::Triage,
        &route.cheap,
        &route.frontier,
        Box::new(move |reference| {
            let (provider, model_ref) = build_provider(&routing, reference)?;
            Ok(RoutedModel {
                reference: model_ref.to_string(),
                provider: Arc::from(provider),
                max_tokens: routing.max_output_tokens_for(&model_ref.provider),
                context_window: model_context_window(&routing, &model_ref.to_string()),
                output_setting: output_setting(&routing, &model_ref.provider).1,
            })
        }),
    )
}

/// Tokens of context the model behind `model_ref` can hold, as Python's
/// `context_window_for`: OpenRouter is asked for its own figure (memoized, and
/// cached beside the user config for a day) and Ollama for the model's maximum,
/// then the overrides and the table.
pub fn model_context_window(config: &Config, model_ref: &str) -> u64 {
    let cache = ConfigPaths::discover()
        .user
        .as_deref()
        .and_then(Path::parent)
        .map(cache_path);
    context_window_with(model_ref, config, |provider, model| {
        discover_context_window(provider, model, config, cache.as_deref(), &OsKeyring)
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
