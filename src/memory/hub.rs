//! The Hugging Face cache, as `huggingface_hub` lays it out, so a model either
//! implementation downloads is found by the other.
//!
//! The cache is `HF_HUB_CACHE`, else `HF_HOME/hub`, else
//! `~/.cache/huggingface/hub`. A repo lives in `models--<org>--<name>`;
//! `refs/main` names the commit, and `snapshots/<commit>/` holds the files.
//! (On Windows without symlink rights `huggingface_hub` copies the files into
//! the snapshot instead of linking them to `blobs/`; this writes them the same
//! way everywhere, which both libraries read.)

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The files sentence-transformers reads from a repo root (the optional ones
/// are skipped when the repo has none). Module directories add their own
/// `config.json`, e.g. `1_Pooling/config.json`.
const ROOT_FILES: &[&str] = &[
    "config.json",
    "config_sentence_transformers.json",
    "modules.json",
    "sentence_bert_config.json",
    "README.md",
    "special_tokens_map.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "vocab.txt",
    "added_tokens.json",
    "model.safetensors",
];

/// Files the Rust model cannot load without.
pub const REQUIRED_FILES: &[&str] = &[
    "config.json",
    "modules.json",
    "tokenizer.json",
    "model.safetensors",
];

/// Why a model could not be found or fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubError(pub String);

impl std::fmt::Display for HubError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where models come from: the cache, the endpoint, and whether the hub may be
/// contacted at all. Explicit, so tests never touch the real cache or network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hub {
    pub root: PathBuf,
    pub endpoint: String,
    /// `HF_HUB_OFFLINE`: never contact the hub.
    pub offline: bool,
}

impl Hub {
    /// The settings `huggingface_hub` reads from the environment: the cache is
    /// `HF_HUB_CACHE`, else `HF_HOME/hub`, else `~/.cache/huggingface/hub` (an
    /// empty variable counts as unset, as Python's `or` chain treats it); the
    /// endpoint is `HF_ENDPOINT`; and `HF_HUB_OFFLINE` forbids the network.
    pub fn from_env() -> Self {
        let root = if let Some(cache) = non_empty_env("HF_HUB_CACHE") {
            PathBuf::from(cache)
        } else if let Some(home) = non_empty_env("HF_HOME") {
            Path::new(&home).join("hub")
        } else {
            let mut root = crate::config::home_dir().unwrap_or_default();
            root.extend([".cache", "huggingface", "hub"]);
            root
        };
        let endpoint = non_empty_env("HF_ENDPOINT")
            .unwrap_or_else(|| "https://huggingface.co".to_owned())
            .trim_end_matches('/')
            .to_owned();
        let offline = std::env::var("HF_HUB_OFFLINE").is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_uppercase().as_str(),
                "1" | "ON" | "YES" | "TRUE"
            )
        });
        Self {
            root,
            endpoint,
            offline,
        }
    }
}

/// The repo a model name refers to: bare names belong to the
/// `sentence-transformers` organisation, as sentence-transformers resolves them.
pub fn repo_id(model_name: &str) -> String {
    if model_name.contains('/') {
        model_name.to_owned()
    } else {
        format!("sentence-transformers/{model_name}")
    }
}

fn repo_dir(root: &Path, repo: &str) -> PathBuf {
    root.join(format!("models--{}", repo.replace('/', "--")))
}

/// True if the model is already in the cache, as Python's `_hf_model_cached`: a
/// directory check only (a bare name also matches the `sentence-transformers`
/// organisation's directory).
pub fn is_cached(root: &Path, model_name: &str) -> bool {
    let mut repos = vec![model_name.to_owned()];
    if !model_name.contains('/') {
        repos.push(format!("sentence-transformers/{model_name}"));
    }
    repos.iter().any(|repo| repo_dir(root, repo).is_dir())
}

/// The cached snapshot `refs/main` points at, if it holds every required file.
pub fn cached_snapshot(root: &Path, repo: &str) -> Option<PathBuf> {
    let dir = repo_dir(root, repo);
    let commit = std::fs::read_to_string(dir.join("refs").join("main")).ok()?;
    let snapshot = dir.join("snapshots").join(commit.trim());
    complete(&snapshot).then_some(snapshot)
}

fn complete(snapshot: &Path) -> bool {
    REQUIRED_FILES
        .iter()
        .all(|file| snapshot.join(file).is_file())
}

/// Resolve `main` on the hub and download the files sentence-transformers
/// reads into the cache, skipping any the snapshot already has. Returns the
/// snapshot directory.
pub fn download(hub: &Hub, repo: &str) -> Result<PathBuf, HubError> {
    // Callers may be inside an async runtime or not; a thread of its own with its
    // own runtime works from either.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| HubError(error.to_string()))?;
                runtime.block_on(fetch_snapshot(&hub.endpoint, &hub.root, repo))
            })
            .join()
            .unwrap_or_else(|_| Err(HubError("the download panicked".to_owned())))
    })
}

async fn fetch_snapshot(base: &str, root: &Path, repo: &str) -> Result<PathBuf, HubError> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| HubError(error.to_string()))?;
    let fail = |error: reqwest::Error| HubError(format!("Could not reach {base}: {error}"));

    let info_url = format!("{base}/api/models/{repo}/revision/main");
    let response = client.get(&info_url).send().await.map_err(fail)?;
    if !response.status().is_success() {
        return Err(HubError(format!(
            "{info_url} answered {}",
            response.status().as_u16()
        )));
    }
    let body = response.bytes().await.map_err(fail)?;
    let info: serde_json::Value =
        serde_json::from_slice(&body).map_err(|error| HubError(format!("{info_url}: {error}")))?;
    let commit = info["sha"]
        .as_str()
        .filter(|sha| !sha.is_empty() && sha.chars().all(|c| c.is_ascii_alphanumeric()))
        .ok_or_else(|| HubError(format!("{info_url} named no commit")))?
        .to_owned();
    let siblings: Vec<&str> = info["siblings"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file["rfilename"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let wanted: Vec<&str> = siblings
        .iter()
        .copied()
        .filter(|file| ROOT_FILES.contains(file) || is_module_config(file))
        .collect();
    if let Some(missing) = REQUIRED_FILES.iter().find(|file| !wanted.contains(file)) {
        return Err(HubError(format!("{repo} has no {missing}")));
    }

    let dir = repo_dir(root, repo);
    let snapshot = dir.join("snapshots").join(&commit);
    for file in wanted {
        let target = snapshot.join(file);
        if target.is_file() {
            continue;
        }
        let url = format!("{base}/{repo}/resolve/{commit}/{file}");
        let mut response = client.get(&url).send().await.map_err(fail)?;
        if !response.status().is_success() {
            return Err(HubError(format!(
                "{url} answered {}",
                response.status().as_u16()
            )));
        }
        let parent = target.parent().unwrap_or(&snapshot);
        std::fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        // Written beside the target and renamed into place, so an interrupted
        // download never leaves a truncated file a later load would trust.
        let partial = target.with_extension("incomplete");
        let mut out = std::fs::File::create(&partial).map_err(|error| io_error(&partial, error))?;
        while let Some(chunk) = response.chunk().await.map_err(fail)? {
            std::io::Write::write_all(&mut out, &chunk)
                .map_err(|error| io_error(&partial, error))?;
        }
        drop(out);
        std::fs::rename(&partial, &target).map_err(|error| io_error(&target, error))?;
    }
    let refs = dir.join("refs");
    std::fs::create_dir_all(&refs).map_err(|error| io_error(&refs, error))?;
    std::fs::write(refs.join("main"), &commit).map_err(|error| io_error(&refs, error))?;
    Ok(snapshot)
}

/// `<n>_<Module>/config.json`, a sentence-transformers module's settings.
fn is_module_config(file: &str) -> bool {
    file.split_once('/').is_some_and(|(dir, rest)| {
        rest == "config.json"
            && dir.split_once('_').is_some_and(|(index, name)| {
                !index.is_empty() && index.chars().all(|c| c.is_ascii_digit()) && !name.is_empty()
            })
    })
}

fn io_error(path: &Path, error: std::io::Error) -> HubError {
    HubError(format!("{}: {error}", path.display()))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_names_belong_to_the_sentence_transformers_org() {
        assert_eq!(
            repo_id("all-MiniLM-L6-v2"),
            "sentence-transformers/all-MiniLM-L6-v2"
        );
        assert_eq!(repo_id("BAAI/bge-small-en"), "BAAI/bge-small-en");
        assert_eq!(
            repo_dir(Path::new("hub"), "sentence-transformers/all-MiniLM-L6-v2"),
            Path::new("hub").join("models--sentence-transformers--all-MiniLM-L6-v2")
        );
    }

    #[test]
    fn module_configs_are_fetched_with_the_root_files() {
        assert!(is_module_config("1_Pooling/config.json"));
        assert!(is_module_config("2_Dense/config.json"));
        assert!(!is_module_config("onnx/config.json"));
        assert!(!is_module_config("1_Pooling/other.json"));
        assert!(!is_module_config("config.json"));
    }
}
