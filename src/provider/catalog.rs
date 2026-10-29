//! Live model catalogs, fetched from each provider's models endpoint, as Python's
//! `cognitive/catalog.py`.
//!
//! Nothing is hardcoded: the endpoint is the single source of truth, so the list
//! cannot go stale between releases. Three response shapes cover the seeded
//! providers:
//!
//! - OpenAI-compatible (OpenAI, OpenRouter, Groq, and any section with a `base_url`):
//!   `GET {base}/v1/models` with `Authorization: Bearer <key>`, answering
//!   `{"data": [{"id": "gpt-4o", ...}, ...]}`.
//! - Anthropic: `GET https://api.anthropic.com/v1/models` with `x-api-key` and
//!   `anthropic-version` (not Bearer), answering `{"data": [{"id", "display_name"}]}`.
//! - Ollama: `GET {base}/api/tags`, no auth, answering `{"models": [{"name": ...}]}`.
//!
//! llama.cpp and LM Studio expose no listing, so they report the catalog as
//! unavailable and the caller falls back to typing `provider:model`, the same
//! fallback every failed fetch takes. Never a dead end.
//!
//! Entries are `(ref, label)` pairs where `ref` is the bare model id; the caller
//! adds the `provider:` prefix.

use crate::config::ProviderConfig;
use crate::secrets::redact;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// A catalog fetch runs behind the interface on a worker; keep it short.
const TIMEOUT: Duration = Duration::from_secs(10);

const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// No model list could be obtained; the caller falls back to free-text entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CatalogUnavailable(pub String);

/// `(bare model id, display label)`.
pub type CatalogEntry = (String, String);

/// Tolerate a `base_url` given with or without the `/v1` suffix, as the provider does.
fn v1(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    }
}

fn bearer(api_key: Option<&str>) -> Vec<(&'static str, String)> {
    api_key
        .map(|key| vec![("Authorization", format!("Bearer {key}"))])
        .unwrap_or_default()
}

/// GET `url` and parse the body as JSON. Every failure (transport, status, body)
/// becomes the one message Python gives, with any key in it redacted.
fn get_json(
    url: &str,
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Value, CatalogUnavailable> {
    request_json(url, headers, None, timeout)
}

/// [`get_json`], or a POST of `body` when there is one.
fn request_json(
    url: &str,
    headers: &[(&str, String)],
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value, CatalogUnavailable> {
    let secrets: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| matches!(*name, "Authorization" | "x-api-key"))
        .map(|(_, value)| value.strip_prefix("Bearer ").unwrap_or(value))
        .collect();
    let fail = |detail: String| {
        CatalogUnavailable(redact(
            &format!("Could not fetch models from {url}: {detail}"),
            &secrets,
        ))
    };
    // Callers may be inside an async runtime or not; a thread of its own with its
    // own runtime works from either.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async {
                    let client = reqwest::Client::builder()
                        .timeout(timeout)
                        .redirect(reqwest::redirect::Policy::none())
                        .build()
                        .map_err(|error| error.to_string())?;
                    let mut request = match body {
                        Some(body) => client
                            .post(url)
                            .header("content-type", "application/json")
                            .body(body.to_string()),
                        None => client.get(url),
                    };
                    for (name, value) in headers {
                        request = request.header(*name, value);
                    }
                    let response = request.send().await.map_err(|error| error.to_string())?;
                    let status = response.status();
                    if !status.is_success() {
                        return Err(status_error(status, url));
                    }
                    let body = response.bytes().await.map_err(|error| error.to_string())?;
                    serde_json::from_slice::<Value>(&body).map_err(|error| error.to_string())
                })
            })
            .join()
            .unwrap_or_else(|_| Err("the request panicked".to_owned()))
    })
    .map_err(fail)
}

/// httpx's `raise_for_status` message, which Python shows verbatim.
fn status_error(status: reqwest::StatusCode, url: &str) -> String {
    let kind = match status.as_u16() {
        100..=199 => "Informational response",
        300..=399 => "Redirect response",
        400..=499 => "Client error",
        _ => "Server error",
    };
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("");
    format!(
        "{kind} '{code} {reason}' for url '{url}'\nFor more information check: \
         https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{code}"
    )
}

/// The list under `key` in a JSON object, or an empty list.
fn list<'a>(body: &'a Value, key: &str) -> &'a [Value] {
    body.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Entries from `items`, taking the id from `id_key` and the label from
/// `label_key` (falling back to the id).
fn entries(items: &[Value], id_key: &str, label_key: &str) -> Vec<CatalogEntry> {
    items
        .iter()
        .filter_map(|item| {
            let id = item.get(id_key)?.as_str()?;
            let label = item
                .get(label_key)
                .and_then(Value::as_str)
                .filter(|label| !label.is_empty())
                .unwrap_or(id);
            Some((id.to_owned(), label.to_owned()))
        })
        .collect()
}

pub fn fetch_openai_compatible(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<CatalogEntry>, CatalogUnavailable> {
    let body = get_json(
        &format!("{}/models", v1(base_url)),
        &bearer(api_key),
        TIMEOUT,
    )?;
    Ok(entries(list(&body, "data"), "id", "name"))
}

/// Lowercased model id to `context_length`, from an OpenRouter-style `/models`
/// list. Entries without a positive whole number are skipped.
pub fn fetch_openai_compatible_context_lengths(
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<HashMap<String, u64>, CatalogUnavailable> {
    let body = get_json(
        &format!("{}/models", v1(base_url)),
        &bearer(api_key),
        timeout,
    )?;
    Ok(list(&body, "data")
        .iter()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            // Python accepts ints and floats (truncated), but not booleans.
            let length = model.get("context_length")?.as_f64()?;
            (length >= 1.0).then(|| (id.to_lowercase(), length as u64))
        })
        .collect())
}

pub fn fetch_anthropic(api_key: Option<&str>) -> Result<Vec<CatalogEntry>, CatalogUnavailable> {
    let Some(key) = api_key else {
        return Err(CatalogUnavailable(
            "Anthropic's model list needs an API key.".to_owned(),
        ));
    };
    let headers = [
        ("x-api-key", key.to_owned()),
        ("anthropic-version", ANTHROPIC_VERSION.to_owned()),
    ];
    let body = get_json(ANTHROPIC_MODELS_URL, &headers, TIMEOUT)?;
    Ok(entries(list(&body, "data"), "id", "display_name"))
}

pub fn fetch_ollama(base_url: &str) -> Result<Vec<CatalogEntry>, CatalogUnavailable> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let body = get_json(&url, &[], TIMEOUT)?;
    Ok(entries(list(&body, "models"), "name", "name"))
}

/// The context length `model` was trained for, from Ollama's `POST {base}/api/show`:
/// `model_info["<architecture>.context_length"]`, or `None` when it reports none.
pub fn fetch_ollama_context_length(
    base_url: &str,
    model: &str,
    timeout: Duration,
) -> Result<Option<u64>, CatalogUnavailable> {
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let body = request_json(
        &url,
        &[],
        Some(&serde_json::json!({"model": model})),
        timeout,
    )?;
    let Some(info) = body.get("model_info").and_then(Value::as_object) else {
        return Ok(None);
    };
    let named = info
        .get("general.architecture")
        .and_then(Value::as_str)
        .and_then(|architecture| info.get(&format!("{architecture}.context_length")));
    // Without an architecture, any `*.context_length` key is the model's own.
    let length = named.or_else(|| {
        info.iter()
            .find(|(key, _)| key.ends_with(".context_length"))
            .map(|(_, value)| value)
    });
    Ok(length
        .and_then(Value::as_f64)
        .filter(|&length| length >= 1.0)
        .map(|length| length as u64))
}

/// The catalog for `provider`, sorted by id: Python's `fetch_catalog`.
pub fn fetch_catalog(
    provider: &str,
    section: &ProviderConfig,
    api_key: Option<&str>,
) -> Result<Vec<CatalogEntry>, CatalogUnavailable> {
    let mut entries = match provider {
        "anthropic" => fetch_anthropic(api_key)?,
        "ollama" => fetch_ollama(
            section
                .base_url
                .as_deref()
                .unwrap_or("http://localhost:11434"),
        )?,
        "llamacpp" => {
            return Err(CatalogUnavailable(
                "llama.cpp / LM Studio expose no model list -- enter provider:model directly."
                    .to_owned(),
            ))
        }
        _ if provider == "openai" || section.base_url.is_some() => fetch_openai_compatible(
            section
                .base_url
                .as_deref()
                .unwrap_or("https://api.openai.com/v1"),
            api_key,
        )?,
        _ => {
            return Err(CatalogUnavailable(format!(
                "Provider '{provider}' has no base_url configured, so its model list cannot be \
                 fetched -- enter provider:model directly."
            )))
        }
    };
    if entries.is_empty() {
        return Err(CatalogUnavailable(format!(
            "Provider '{provider}' returned an empty model list."
        )));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

/// Prefix bare catalog ids with `provider:`, as Python's `prefix_catalog_refs`.
///
/// "Already prefixed" means an exact `provider:` prefix, never "contains a colon":
/// Ollama ids such as `llama3.1:8b` have colons of their own. A label equal to its
/// id is prefixed too, so it keeps naming the full ref; a real display name is kept.
pub fn prefix_catalog_refs(provider: &str, entries: &[CatalogEntry]) -> Vec<CatalogEntry> {
    let prefix = format!("{provider}:");
    entries
        .iter()
        .map(|(id, label)| {
            let full = if id.starts_with(&prefix) {
                id.clone()
            } else {
                format!("{prefix}{id}")
            };
            let label = if label == id {
                full.clone()
            } else {
                label.clone()
            };
            (full, label)
        })
        .collect()
}

/// Does the catalog vouch for `id` verbatim? Exact and case-sensitive, so a typo
/// is never masked (Python's `catalog_has_ref`).
pub fn catalog_has_ref(id: &str, entries: &[CatalogEntry]) -> bool {
    entries.iter().any(|(entry, _)| entry == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_gain_v1_once() {
        assert_eq!(
            v1("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(v1("http://localhost:8080"), "http://localhost:8080/v1");
    }

    #[test]
    fn entries_take_the_label_or_fall_back_to_the_id() {
        let body: Value = serde_json::json!({"data": [
            {"id": "b", "name": "Model B"},
            {"id": "a"},
            {"name": "no id"},
            "not an object",
        ]});
        assert_eq!(
            entries(list(&body, "data"), "id", "name"),
            [("b".into(), "Model B".into()), ("a".into(), "a".into())]
        );
    }

    #[test]
    fn unlisted_providers_report_why() {
        let section = ProviderConfig::default();
        assert_eq!(
            fetch_catalog("llamacpp", &section, None).unwrap_err().0,
            "llama.cpp / LM Studio expose no model list -- enter provider:model directly."
        );
        assert!(fetch_catalog("mystery", &section, None)
            .unwrap_err()
            .0
            .starts_with("Provider 'mystery' has no base_url configured"));
        assert_eq!(
            fetch_anthropic(None).unwrap_err().0,
            "Anthropic's model list needs an API key."
        );
    }

    #[test]
    fn refs_are_prefixed_once_and_matched_exactly() {
        let entries = vec![
            ("llama3.1:8b".to_owned(), "llama3.1:8b".to_owned()),
            ("ollama:qwen".to_owned(), "Qwen".to_owned()),
        ];
        assert_eq!(
            prefix_catalog_refs("ollama", &entries),
            [
                ("ollama:llama3.1:8b".into(), "ollama:llama3.1:8b".into()),
                ("ollama:qwen".into(), "Qwen".into())
            ]
        );
        assert!(catalog_has_ref("llama3.1:8b", &entries));
        assert!(!catalog_has_ref("Llama3.1:8b", &entries));
    }
}
