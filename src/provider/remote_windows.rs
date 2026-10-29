//! A hosted model's real context window from OpenRouter's `/models` endpoint, as
//! Python's `cognitive/remote_windows.py`.
//!
//! The name table in [`super::capabilities`] guesses from substrings, which is
//! wrong for hosted models (`openrouter:qwen/qwen3.7-max` matched the generic
//! `qwen` row: 32,768). OpenRouter reports `context_length` per model, so it is
//! asked: memoized per process, cached on disk for 24 hours, with a short timeout.
//! Every failure gives `None`, so the caller falls through to the table.
//!
//! Ollama is asked too (`/api/show`), for the window to run a model at
//! ([`ollama_window`]); that is beyond Python, which always asks Ollama for 8192.
//! It is memoized per process only: a local model can be re-pulled at any time.

use super::capabilities::DEFAULT_OLLAMA_NUM_CTX_MAX;
use super::catalog::{fetch_ollama_show, fetch_openai_compatible_context_lengths, OllamaShow};
use crate::config::{Config, ProviderConfig};
use crate::secrets::{resolve_api_key, SecretStore};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub const CACHE_TTL_S: f64 = 24.0 * 3600.0;
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Normalized base URL to {lowercased model id: context length}; an empty map is
/// a negative memo.
static MEMO: LazyLock<Mutex<HashMap<String, HashMap<String, u64>>>> =
    LazyLock::new(Default::default);

/// (normalized Ollama base URL, model) to what `/api/show` said; `None` is a
/// negative memo, so a server that is down is not asked again on every redraw.
type OllamaLengths = HashMap<(String, String), Option<OllamaShow>>;
static OLLAMA_MEMO: LazyLock<Mutex<OllamaLengths>> = LazyLock::new(Default::default);

/// The cache file Python shares: `~/.agent86/cache/model_context_windows.json`,
/// given the directory that holds the user config.
pub fn cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("cache").join("model_context_windows.json")
}

/// The `openrouter` section, or any section whose host is on openrouter.ai.
pub fn is_openrouter(provider: &str, section: Option<&ProviderConfig>) -> bool {
    if provider == "openrouter" {
        return true;
    }
    section
        .and_then(|section| section.base_url.as_deref())
        .and_then(|base| url::Url::parse(base).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

fn read_entry(path: &Path, base_url: &str) -> Option<(f64, HashMap<String, u64>)> {
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let entry = raw.get(base_url)?;
    let fetched_at = entry.get("fetched_at")?.as_f64()?;
    let models = entry
        .get("models")?
        .as_object()?
        .iter()
        .filter_map(|(id, length)| Some((id.clone(), length.as_u64().filter(|&n| n > 0)?)))
        .collect();
    Some((fetched_at, models))
}

/// Merge this base URL's lengths into the cache file, written whole then renamed.
fn write_entry(path: &Path, base_url: &str, models: &HashMap<String, u64>) {
    let mut existing = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    existing[base_url] = json!({"fetched_at": now(), "models": models});
    let Some(parent) = path.parent() else { return };
    let tmp = path.with_extension("tmp");
    let _ = std::fs::create_dir_all(parent)
        .and_then(|()| std::fs::write(&tmp, existing.to_string()))
        .and_then(|()| std::fs::rename(&tmp, path));
}

fn load_models(
    base_url: &str,
    cache: Option<&Path>,
    fetch: impl FnOnce() -> Option<HashMap<String, u64>>,
) -> HashMap<String, u64> {
    let mut memo = MEMO.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(models) = memo.get(base_url) {
        return models.clone();
    }
    let cached = cache.and_then(|path| read_entry(path, base_url));
    if let Some((fetched_at, models)) = &cached {
        if now() - fetched_at < CACHE_TTL_S && !models.is_empty() {
            memo.insert(base_url.to_owned(), models.clone());
            return models.clone();
        }
    }
    if let Some(models) = fetch().filter(|models| !models.is_empty()) {
        if let Some(path) = cache {
            write_entry(path, base_url, &models);
        }
        memo.insert(base_url.to_owned(), models.clone());
        return models;
    }
    // Offline or refused: a stale cache beats nothing, and nothing is remembered.
    let models = cached.map(|(_, models)| models).unwrap_or_default();
    memo.insert(base_url.to_owned(), models.clone());
    models
}

/// OpenRouter's context window for `model`, or `None` (never fails).
///
/// `cache` is the cache file ([`cache_path`]); `None` keeps the lookup in memory.
pub fn openrouter_context_window(
    provider: &str,
    model: &str,
    config: &Config,
    cache: Option<&Path>,
    secrets: &dyn SecretStore,
) -> Option<u64> {
    let section = config.providers.get(provider);
    if !is_openrouter(provider, section) {
        return None;
    }
    let base_url = section
        .and_then(|section| section.base_url.as_deref())
        .unwrap_or(DEFAULT_BASE_URL)
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let models = load_models(&base_url, cache, || {
        let key = resolve_api_key(
            provider,
            section.and_then(|section| section.api_key_env.as_deref()),
            secrets,
        );
        fetch_openai_compatible_context_lengths(&base_url, key.as_deref(), FETCH_TIMEOUT).ok()
    });
    models.get(&model.trim().to_lowercase()).copied()
}

/// The window to run an Ollama model at when `[providers.ollama] num_ctx` is unset:
/// the `num_ctx` its Modelfile sets, as it is, since someone chose that size for
/// this model; else the length it was trained for, up to `num_ctx_max` (32768 by
/// default), because Ollama allocates the whole window up front and a model's full
/// 256k could leave the GPU and crawl. `None` when Ollama reports neither.
pub fn ollama_window(section: Option<&ProviderConfig>, show: &OllamaShow) -> Option<u64> {
    if let Some(num_ctx) = show.num_ctx.filter(|&num_ctx| num_ctx > 0) {
        return Some(num_ctx);
    }
    let cap = section
        .and_then(|section| section.num_ctx_max)
        .filter(|&cap| cap > 0)
        .unwrap_or(DEFAULT_OLLAMA_NUM_CTX_MAX);
    show.context_length
        .filter(|&length| length > 0)
        .map(|length| length.min(u64::from(cap)))
}

/// [`ollama_window`] for `model`, asking the server (never fails).
pub fn ollama_context_window(provider: &str, model: &str, config: &Config) -> Option<u64> {
    if provider != "ollama" {
        return None;
    }
    let base_url = config
        .providers
        .get(provider)
        .and_then(|section| section.base_url.as_deref())
        .unwrap_or(super::ollama::DEFAULT_BASE_URL)
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let show = *OLLAMA_MEMO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry((base_url.clone(), model.to_owned()))
        .or_insert_with(|| fetch_ollama_show(&base_url, model, FETCH_TIMEOUT).ok());
    ollama_window(config.providers.get(provider), &show?)
}

/// What the server behind `provider` reports for `model`'s window: OpenRouter's
/// figure, or an Ollama model's maximum. The `discover` of
/// [`super::capabilities::context_window_with`].
pub fn discover_context_window(
    provider: &str,
    model: &str,
    config: &Config,
    cache: Option<&Path>,
    secrets: &dyn SecretStore,
) -> Option<u64> {
    openrouter_context_window(provider, model, config, cache, secrets)
        .or_else(|| ollama_context_window(provider, model, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ollama_modelfile_num_ctx_is_used_as_is_and_a_trained_length_is_capped() {
        let section = |num_ctx_max| ProviderConfig {
            num_ctx_max,
            ..Default::default()
        };
        let show = |context_length, num_ctx| OllamaShow {
            context_length,
            num_ctx,
        };
        // The trained length, capped at 32k by default or at `num_ctx_max`.
        assert_eq!(
            ollama_window(None, &show(Some(262_144), None)),
            Some(32_768)
        );
        assert_eq!(ollama_window(None, &show(Some(4_096), None)), Some(4_096));
        assert_eq!(
            ollama_window(Some(&section(Some(65_536))), &show(Some(262_144), None)),
            Some(65_536)
        );
        assert_eq!(
            ollama_window(Some(&section(Some(0))), &show(Some(262_144), None)),
            Some(32_768),
            "a zero cap is no cap setting"
        );
        // A Modelfile's own num_ctx was chosen for the model: no cap, even past the
        // trained length.
        assert_eq!(
            ollama_window(None, &show(Some(262_144), Some(262_144))),
            Some(262_144)
        );
        assert_eq!(
            ollama_window(
                Some(&section(Some(16_384))),
                &show(Some(8_192), Some(65_536))
            ),
            Some(65_536)
        );
        assert_eq!(ollama_window(None, &show(None, None)), None);
        assert_eq!(ollama_window(None, &show(Some(0), Some(0))), None);
    }

    #[test]
    fn openrouter_is_recognised_by_name_or_host() {
        let section = |url: &str| ProviderConfig {
            base_url: Some(url.to_owned()),
            ..Default::default()
        };
        assert!(is_openrouter("openrouter", None));
        assert!(is_openrouter(
            "or",
            Some(&section("https://openrouter.ai/api/v1"))
        ));
        assert!(is_openrouter(
            "or",
            Some(&section("https://eu.openrouter.ai/v1"))
        ));
        assert!(!is_openrouter(
            "or",
            Some(&section("https://notopenrouter.ai/v1"))
        ));
        assert!(!is_openrouter("groq", None));
    }

    #[test]
    fn the_cache_is_used_while_fresh_and_a_stale_one_is_a_fallback() {
        let dir = std::env::temp_dir().join(format!("agent86-windows-{}", std::process::id()));
        let path = cache_path(&dir);
        let fresh = HashMap::from([("vendor/model".to_owned(), 262_144)]);
        write_entry(&path, "http://fresh.test/v1", &fresh);
        assert_eq!(
            load_models("http://fresh.test/v1", Some(&path), || panic!("fetched")),
            fresh
        );

        // A stale entry is refreshed; when the fetch fails, it is still used.
        let stale = json!({"http://stale.test/v1": {
            "fetched_at": 1.0, "models": {"old/model": 4096, "bad": 0}
        }});
        std::fs::write(&path, stale.to_string()).unwrap();
        assert_eq!(
            load_models("http://stale.test/v1", Some(&path), || None),
            HashMap::from([("old/model".to_owned(), 4096)])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
