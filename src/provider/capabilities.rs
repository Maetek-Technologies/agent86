//! Per-model API capability facts, matching the Python `capabilities.py`.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Model-id prefixes (provider prefix stripped, lowercased) that reject
/// `temperature`, `top_p`, and `top_k`; those parameters must be omitted.
const NO_SAMPLING_FAMILIES: &[&str] = &[
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-5",
    "claude-fable-5",
    "fable-5",
];

/// Strip a `provider:` prefix and lowercase.
pub fn normalize_model(model: &str) -> String {
    let ident = model.split_once(':').map_or(model, |(_, rest)| rest);
    ident.trim().to_lowercase()
}

/// The request parameters that were removed together.
const SAMPLING_PARAMS: &[&str] = &["temperature", "top_p", "top_k"];

/// Words an API uses when it refuses a parameter outright.
const REJECTION_WORDS: &[&str] = &[
    "deprecat",
    "unsupported",
    "not supported",
    "not permitted",
    "unexpected",
    "removed",
    "cannot be used",
    "no longer",
];

/// Models that refused a sampling parameter during this process, so a model
/// released after [`NO_SAMPLING_FAMILIES`] was written corrects itself after one
/// rejected request. Session-scoped, as in Python: no disk cache.
fn learned_no_sampling() -> &'static Mutex<HashSet<String>> {
    static LEARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    LEARNED.get_or_init(Mutex::default)
}

/// False when `model` rejects sampling parameters and they must be omitted.
pub fn supports_sampling_params(model: &str) -> bool {
    let ident = normalize_model(model);
    if learned_no_sampling()
        .lock()
        .is_ok_and(|learned| learned.contains(&ident))
    {
        return false;
    }
    !NO_SAMPLING_FAMILIES
        .iter()
        .any(|family| ident.starts_with(family))
}

/// Record, for this process, that `model` rejected a sampling parameter.
pub fn mark_sampling_unsupported(model: &str) {
    if let Ok(mut learned) = learned_no_sampling().lock() {
        learned.insert(normalize_model(model));
    }
}

/// True when an API error text says a sampling parameter was refused.
pub fn is_sampling_rejection(message: &str) -> bool {
    let low = message.to_lowercase();
    SAMPLING_PARAMS.iter().any(|param| low.contains(param))
        && REJECTION_WORDS.iter().any(|word| low.contains(word))
}

/// The window assumed for a model nobody knows anything about.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 8_192;

/// Model-id substring to context window (tokens), matched against the lowercased
/// model id. The first match wins, so specific ids come before their families.
const CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("claude", 200_000),
    ("gpt-5", 400_000),
    ("gpt-4.1", 1_047_576),
    ("gpt-4o", 128_000),
    ("gpt-4-turbo", 128_000),
    ("gpt-4", 8_192),
    ("gpt-3.5", 16_385),
    ("o4-mini", 200_000),
    ("o3", 200_000),
    ("o1", 200_000),
    ("llama-3.1", 131_072),
    ("llama3.1", 131_072),
    ("llama-3.3", 131_072),
    ("llama3.3", 131_072),
    ("qwen", 32_768),
    ("mixtral", 32_768),
    ("mistral", 32_768),
    ("gemma", 8_192),
    ("phi", 16_384),
];

/// [`context_window_with`] without asking any server, for callers that must not
/// touch the network.
pub fn context_window_for(model_ref: &str, config: &crate::config::Config) -> u64 {
    context_window_with(model_ref, config, |_, _| None)
}

/// Tokens of context the model behind `model_ref` can hold, as Python's
/// `context_window_for`: a `[model.context_window]` override (by the full ref, then
/// the bare id), then what `discover(provider, model)` reports (OpenRouter's own
/// figure, see [`super::remote_windows`]), then the server's own setting for Ollama
/// and llama.cpp, then the table, then [`DEFAULT_CONTEXT_WINDOW`].
pub fn context_window_with(
    model_ref: &str,
    config: &crate::config::Config,
    discover: impl FnOnce(&str, &str) -> Option<u64>,
) -> u64 {
    let (provider, model) = model_ref.split_once(':').unwrap_or(("", model_ref));
    for key in [model_ref, model] {
        if let Some(&window) = config.model.context_window.get(key).filter(|&&w| w > 0) {
            return window;
        }
    }
    if !provider.is_empty() {
        if let Some(window) = discover(provider, model).filter(|&window| window > 0) {
            return window;
        }
    }
    match provider {
        // Ollama serves whatever `num_ctx` asks for; llama.cpp's window is a launch
        // flag that the API does not report, so the default is the only honest answer.
        "ollama" => {
            return config
                .providers
                .get("ollama")
                .and_then(|provider| provider.num_ctx)
                .filter(|&num_ctx| num_ctx > 0)
                .map_or(DEFAULT_CONTEXT_WINDOW, u64::from)
        }
        "llamacpp" => return DEFAULT_CONTEXT_WINDOW,
        _ => {}
    }
    let ident = model.trim().to_lowercase();
    CONTEXT_WINDOWS
        .iter()
        .find(|(needle, _)| ident.contains(needle))
        .map_or(DEFAULT_CONTEXT_WINDOW, |&(_, window)| window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_windows_follow_overrides_then_the_server_then_the_table() {
        let paths = crate::config::ConfigPaths {
            user: None,
            project: std::path::PathBuf::from("no-such-dir/config.toml"),
        };
        let mut config = crate::config::load_config(&paths, &Default::default()).unwrap();
        assert_eq!(
            context_window_for("openrouter:openai/gpt-4o-mini", &config),
            128_000
        );
        assert_eq!(context_window_for("openai:gpt-4-0613", &config), 8_192);
        assert_eq!(
            context_window_for("openai:gpt-4.1-mini", &config),
            1_047_576
        );
        assert_eq!(context_window_for("local:mystery", &config), 8_192);
        assert_eq!(context_window_for("ollama:llama3.1", &config), 8_192);
        assert_eq!(context_window_for("llamacpp:qwen3", &config), 8_192);
        assert_eq!(
            context_window_with("openrouter:qwen/qwen3.7-max", &config, |provider, model| {
                assert_eq!((provider, model), ("openrouter", "qwen/qwen3.7-max"));
                Some(262_144)
            }),
            262_144,
            "a server's own figure beats the table"
        );
        config
            .model
            .context_window
            .insert("gpt-4o-mini".into(), 64_000);
        assert_eq!(
            context_window_for("openrouter:gpt-4o-mini", &config),
            64_000
        );
        config
            .model
            .context_window
            .insert("openrouter:gpt-4o-mini".into(), 32_000);
        assert_eq!(
            context_window_for("openrouter:gpt-4o-mini", &config),
            32_000
        );
    }

    #[test]
    fn removed_sampling_families_match_by_prefix() {
        assert!(!supports_sampling_params("claude-opus-5-20260101"));
        assert!(!supports_sampling_params("anthropic:Claude-Sonnet-5"));
        assert!(supports_sampling_params("gpt-4o-mini"));
        assert!(supports_sampling_params("claude-sonnet-4-5"));
    }

    #[test]
    fn a_rejected_sampling_parameter_is_learned_for_the_session() {
        assert!(is_sampling_rejection(
            "Error code: 400 - `temperature` is deprecated for this model."
        ));
        assert!(!is_sampling_rejection("temperature must be at most 1"));
        assert!(!is_sampling_rejection("model is unsupported"));

        assert!(supports_sampling_params("anthropic:claude-test-learned-9"));
        mark_sampling_unsupported("anthropic:Claude-Test-Learned-9");
        assert!(!supports_sampling_params("claude-test-learned-9"));
    }
}
