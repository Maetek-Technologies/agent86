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
//! Google (beyond Python): `GET {base}/models?pageSize=1000` with `x-goog-api-key`,
//! following `nextPageToken`, keeping models that support `generateContent`,
//! [`fetch_google`].
//!
//! A vLLM section (beyond Python) is OpenAI-compatible here, its key sent when one
//! resolves; its `/v1/models` also gives each model's `max_model_len`, read by
//! [`fetch_vllm_model_lengths`] for the window.
//!
//! An LM Studio section (beyond Python) is asked through LM Studio's native REST
//! API (`GET {base without /v1}/api/v1/models`, [`fetch_lmstudio_models`]), which
//! lists every downloaded model with whether it is loaded, the context length it
//! was loaded with, and whether it was trained for tool use; `/v1/models` (ids
//! only) is the fallback for an older LM Studio or a proxy that passes only `/v1`.
//!
//! llama.cpp exposes no listing, so it reports the catalog as unavailable and the
//! caller falls back to typing `provider:model`, the same fallback every failed
//! fetch takes. Never a dead end.
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
pub(crate) fn get_json(
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
    context_lengths(base_url, api_key, timeout, "context_length")
}

/// Lowercased model id to `max_model_len`, from a vLLM server's `/v1/models`
/// (beyond Python). Every name the server serves (`--served-model-name`) has its
/// own card with the length; a LoRA adapter's card has `null`, which is skipped.
pub fn fetch_vllm_model_lengths(
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<HashMap<String, u64>, CatalogUnavailable> {
    context_lengths(base_url, api_key, timeout, "max_model_len")
}

fn context_lengths(
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
    field: &str,
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
            let length = model.get(field)?.as_f64()?;
            (length >= 1.0).then(|| (id.to_lowercase(), length as u64))
        })
        .collect())
}

/// One model from Google's `GET /models` (beyond Python).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleModel {
    /// `name` without `models/`.
    pub id: String,
    /// `displayName`, else the id.
    pub label: String,
    pub input_token_limit: Option<u64>,
}

/// Google's models that can chat, every page, with the key in `x-goog-api-key`.
pub fn fetch_google(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<GoogleModel>, CatalogUnavailable> {
    fetch_google_with(base_url, api_key, TIMEOUT)
}

/// [`fetch_google`] with its own timeout, for the window lookup.
pub fn fetch_google_with(
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<Vec<GoogleModel>, CatalogUnavailable> {
    let Some(key) = api_key else {
        return Err(CatalogUnavailable(
            "Google's model list needs an API key.".to_owned(),
        ));
    };
    let headers = [("x-goog-api-key", key.to_owned())];
    let base = base_url.trim_end_matches('/');
    let mut models = Vec::new();
    let mut token: Option<String> = None;
    // A server that never stops paging is cut off.
    for _ in 0..20 {
        let mut url = format!("{base}/models?pageSize=1000");
        if let Some(token) = &token {
            url.push_str("&pageToken=");
            url.push_str(&url_component(token));
        }
        let body = get_json(&url, &headers, timeout)?;
        for model in list(&body, "models") {
            let Some(name) = model.get("name").and_then(Value::as_str) else {
                continue;
            };
            let methods = model
                .get("supportedGenerationMethods")
                .or_else(|| model.get("supported_actions"))
                .and_then(Value::as_array);
            let chats = methods.is_some_and(|methods| {
                methods
                    .iter()
                    .any(|method| method.as_str() == Some("generateContent"))
            });
            if !chats {
                continue;
            }
            let id = name.strip_prefix("models/").unwrap_or(name).to_owned();
            models.push(GoogleModel {
                label: model
                    .get("displayName")
                    .and_then(Value::as_str)
                    .filter(|label| !label.is_empty())
                    .unwrap_or(&id)
                    .to_owned(),
                input_token_limit: model
                    .get("inputTokenLimit")
                    .and_then(Value::as_u64)
                    .filter(|&limit| limit > 0),
                id,
            });
        }
        token = body
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    Ok(models)
}

/// A query value with everything but unreserved characters percent-encoded.
fn url_component(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// One model from LM Studio's native `GET /api/v1/models` (beyond Python).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmStudioModel {
    /// The id requests name (`key`).
    pub key: String,
    pub display_name: String,
    /// `llm`, `embedding`, or `decision`; only `llm` can chat.
    pub kind: String,
    /// The first loaded instance's `config.context_length`, when loaded.
    pub loaded_context: Option<u64>,
    /// `max_context_length`, which LM Studio leaves null for some models.
    pub max_context: Option<u64>,
    /// `capabilities.trained_for_tool_use`, when reported.
    pub tool_use: Option<bool>,
}

/// LM Studio's native API root for an OpenAI-compatible `base_url`: the URL
/// without its `/v1`.
fn lmstudio_root(base_url: &str) -> &str {
    let base = base_url.trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base)
}

/// Every model LM Studio has downloaded, from its native API, with the key as a
/// bearer token when one is given (LM Studio's API tokens).
pub fn fetch_lmstudio_models(
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<Vec<LmStudioModel>, CatalogUnavailable> {
    let url = format!("{}/api/v1/models", lmstudio_root(base_url));
    let body = get_json(&url, &bearer(api_key), timeout)?;
    let length = |value: Option<&Value>| {
        value
            .and_then(Value::as_f64)
            .filter(|&length| length >= 1.0)
            .map(|length| length as u64)
    };
    Ok(list(&body, "models")
        .iter()
        .filter_map(|model| {
            let key = model.get("key")?.as_str()?.to_owned();
            let loaded_context = model
                .get("loaded_instances")
                .and_then(Value::as_array)
                .and_then(|instances| instances.first())
                .and_then(|instance| length(instance.pointer("/config/context_length")));
            Some(LmStudioModel {
                display_name: model
                    .get("display_name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&key)
                    .to_owned(),
                kind: model
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("llm")
                    .to_owned(),
                loaded_context,
                max_context: length(model.get("max_context_length")),
                tool_use: model
                    .pointer("/capabilities/trained_for_tool_use")
                    .and_then(Value::as_bool),
                key,
            })
        })
        .collect())
}

/// The catalog from LM Studio's list: chat models only, each label marking a
/// model that is loaded (it answers at once; others load on first use) and one
/// not trained for tool use (its tool calls will be unreliable).
fn lmstudio_entries(models: &[LmStudioModel]) -> Vec<CatalogEntry> {
    models
        .iter()
        .filter(|model| model.kind == "llm")
        .map(|model| {
            let mut label = model.display_name.clone();
            if model.loaded_context.is_some() {
                label.push_str(" · loaded");
            }
            if model.tool_use == Some(false) {
                label.push_str(" · no tool use");
            }
            (model.key.clone(), label)
        })
        .collect()
}

/// Is anything listening at `base_url`'s host and port? A TCP connect within
/// `timeout`, nothing sent. The editor's model lists ask it before a key-optional
/// server (vLLM), which is usually just not running, so that case stays quiet.
pub fn server_listening(base_url: &str, timeout: Duration) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (host, port)
        .to_socket_addrs()
        .map(|addrs| {
            addrs
                .into_iter()
                .any(|addr| TcpStream::connect_timeout(&addr, timeout).is_ok())
        })
        .unwrap_or(false)
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

/// What Ollama's `POST {base}/api/show` says about a model's context window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OllamaShow {
    /// The length it was trained for: `model_info["<architecture>.context_length"]`.
    pub context_length: Option<u64>,
    /// The `num_ctx` its Modelfile sets (`PARAMETER num_ctx ...`), if any.
    pub num_ctx: Option<u64>,
}

/// The context length `model` was trained for, or `None` when Ollama reports none.
pub fn fetch_ollama_context_length(
    base_url: &str,
    model: &str,
    timeout: Duration,
) -> Result<Option<u64>, CatalogUnavailable> {
    Ok(fetch_ollama_show(base_url, model, timeout)?.context_length)
}

/// The trained length and the Modelfile's `num_ctx` of `model`.
pub fn fetch_ollama_show(
    base_url: &str,
    model: &str,
    timeout: Duration,
) -> Result<OllamaShow, CatalogUnavailable> {
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let body = request_json(
        &url,
        &[],
        Some(&serde_json::json!({"model": model})),
        timeout,
    )?;
    Ok(OllamaShow {
        context_length: trained_context_length(&body),
        num_ctx: body
            .get("parameters")
            .and_then(Value::as_str)
            .and_then(modelfile_num_ctx),
    })
}

/// `num_ctx` from `/api/show`'s `parameters`, which lists one `name value` pair per
/// line, as the Modelfile's `PARAMETER` lines set them.
fn modelfile_num_ctx(parameters: &str) -> Option<u64> {
    parameters.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("num_ctx"))
            .then(|| words.next()?.parse::<u64>().ok())
            .flatten()
            .filter(|&num_ctx| num_ctx > 0)
    })
}

fn trained_context_length(body: &Value) -> Option<u64> {
    let info = body.get("model_info").and_then(Value::as_object)?;
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
    length
        .and_then(Value::as_f64)
        .filter(|&length| length >= 1.0)
        .map(|length| length as u64)
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
        "google" => fetch_google(
            section
                .base_url
                .as_deref()
                .unwrap_or(super::google::DEFAULT_BASE_URL),
            api_key,
        )?
        .into_iter()
        .map(|model| (model.id, model.label))
        .collect(),
        "llamacpp" => {
            return Err(CatalogUnavailable(
                "llama.cpp exposes no model list -- enter provider:model directly.".to_owned(),
            ))
        }
        _ if super::server_kind(provider, Some(section)) == Some(super::ServerKind::LmStudio) => {
            let base_url = section
                .base_url
                .as_deref()
                .unwrap_or(super::LMSTUDIO_DEFAULT_BASE_URL);
            match fetch_lmstudio_models(base_url, api_key, TIMEOUT) {
                Ok(models) => lmstudio_entries(&models),
                // An older LM Studio, or a proxy that passes only `/v1`: ids only.
                Err(_) => fetch_openai_compatible(base_url, api_key)?,
            }
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
    fn a_listening_port_is_found_and_a_closed_one_is_not() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = format!("http://{}/v1", listener.local_addr().unwrap());
        assert!(server_listening(&open, Duration::from_millis(500)));
        drop(listener);
        assert!(!server_listening(&open, Duration::from_millis(500)));
        assert!(!server_listening("not a url", Duration::from_millis(500)));
    }

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
            "llama.cpp exposes no model list -- enter provider:model directly."
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
