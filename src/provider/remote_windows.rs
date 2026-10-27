//! A hosted model's real context window from OpenRouter's `/models` endpoint, as
//! Python's `cognitive/remote_windows.py`.
//!
//! The name table in [`super::capabilities`] guesses from substrings, which is
//! wrong for hosted models (`openrouter:qwen/qwen3.7-max` matched the generic
//! `qwen` row: 32,768). OpenRouter reports `context_length` per model, so it is
//! asked: memoized per process, cached on disk for 24 hours, with a short timeout.
//! Every failure gives `None`, so the caller falls through to the table.

use super::catalog::fetch_openai_compatible_context_lengths;
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

#[cfg(test)]
mod tests {
    use super::*;

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
