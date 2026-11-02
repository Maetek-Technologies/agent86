//! The sentence-transformers embedder against Python's: tokens, vectors, and the
//! Hugging Face cache.
//!
//! `tests/fixtures/memory/tiny-st/` is a tiny random BERT sentence-transformers
//! model in the layout `all-MiniLM-L6-v2` ships (mean pooling, Normalize,
//! `max_seq_length` 256), and `tiny_st.json` holds the token ids and the vectors
//! sentence-transformers' `encode([text], normalize_embeddings=True)` returned
//! for it, which is what Python's `SentenceTransformerEmbedder.encode_one` does.
//! Both come from a local script run with sentence-transformers and torch; the
//! reference code is never run. `minilm.json` holds the same for the real model,
//! checked only when `AGENT86_TEST_MINILM=1` and the model is in the local cache.
//! Downloads are served by the mock server; no test touches the network.

mod support;

use agent86::memory::embeddings::{cosine, pack, unpack, Embedder};
use agent86::memory::hub::{cached_snapshot, Hub};
use agent86::memory::sentence_transformers::SentenceTransformerEmbedder;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use support::{fixture, MockServer, Reply};

/// Kernels sum in a different order than torch's, so components agree to float
/// rounding rather than bit for bit; scores much closer than this never decide
/// a recall.
const TOLERANCE: f64 = 1e-6;

fn tiny_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory/tiny-st")
}

fn golden(name: &str) -> Value {
    serde_json::from_str(&fixture(&format!("memory/{name}"))).unwrap()
}

fn floats(values: &Value) -> Vec<f64> {
    values
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().parse().unwrap())
        .collect()
}

/// Every golden case: the same token ids, and a vector within tolerance whose
/// stored `float32` form scores the same against Python's.
fn check_against(embedder: &dyn Embedder, golden: &Value) {
    let dim = golden["dim"].as_u64().unwrap() as usize;
    assert_eq!(embedder.dim(), dim);
    let mut worst = 0.0f64;
    for case in golden["cases"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let expected = floats(&case["vector"]);
        let actual = embedder.encode_one(text);
        assert_eq!(actual.len(), dim, "{text:?}");
        for (a, e) in actual.iter().zip(&expected) {
            worst = worst.max((a - e).abs());
        }
        let score = cosine(&unpack(&pack(&actual)), &expected);
        assert!((score - 1.0).abs() < 1e-5, "{text:?}: self-score {score}");
    }
    eprintln!(
        "{}: largest component difference {worst:e}",
        golden["model"]
    );
    assert!(worst < TOLERANCE, "largest component difference {worst:e}");
}

fn token_ids(dir: &Path, text: &str) -> Vec<u32> {
    // The tokenizer as the embedder configures it: `max_seq_length` truncation,
    // no padding. Rebuilt here so a failure names the tokens, not a vector.
    let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).unwrap();
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: 256,
            ..Default::default()
        }))
        .unwrap();
    tokenizer.with_padding(None);
    tokenizer.encode(text, true).unwrap().get_ids().to_vec()
}

#[test]
fn the_tiny_model_matches_sentence_transformers() {
    let golden = golden("tiny_st.json");
    for case in golden["cases"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let ids: Vec<u32> = serde_json::from_value(case["ids"].clone()).unwrap();
        assert_eq!(token_ids(&tiny_dir(), text), ids, "{text:?}");
    }
    let embedder =
        SentenceTransformerEmbedder::from_dir(&tiny_dir(), "sentence-transformers:tiny".into())
            .unwrap();
    assert_eq!(embedder.spec(), "sentence-transformers:tiny");
    check_against(&embedder, &golden);
}

#[test]
fn a_local_directory_loads_without_the_hub() {
    let temp = TempDir::new("local");
    let hub = Hub {
        root: temp.0.join("hub"),
        endpoint: "http://127.0.0.1:9".into(),
        offline: false,
    };
    let name = tiny_dir().to_string_lossy().into_owned();
    let embedder = SentenceTransformerEmbedder::load_from(&hub, &name, true).unwrap();
    assert_eq!(embedder.spec(), format!("sentence-transformers:{name}"));
    assert!(!hub.root.exists());
}

/// The files the hub would serve for the tiny model, with the revision listing.
fn hub_replies(commit: &str) -> (Vec<Reply>, Vec<&'static str>) {
    hub_replies_listing(commit, None)
}

/// [`hub_replies`], with the listing's SHA-256 for the weights replaced when given.
/// As on the hub, every file lists its size, and the weights (stored with LFS)
/// their SHA-256 too.
fn hub_replies_listing(
    commit: &str,
    weights_sha256: Option<&str>,
) -> (Vec<Reply>, Vec<&'static str>) {
    let files = [
        "config.json",
        "modules.json",
        "sentence_bert_config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "model.safetensors",
        "1_Pooling/config.json",
    ];
    let mut siblings: Vec<Value> = files
        .iter()
        .map(|file| {
            let bytes = std::fs::read(tiny_dir().join(file)).unwrap();
            let size = bytes.len();
            if *file == "model.safetensors" {
                let sha256 = weights_sha256
                    .map(str::to_owned)
                    .unwrap_or_else(|| agent86::trust::sha256_hex(&bytes));
                json!({"rfilename": file, "size": size, "lfs": {"sha256": sha256, "size": size}})
            } else {
                json!({"rfilename": file, "size": size})
            }
        })
        .collect();
    // Files sentence-transformers never reads are not fetched.
    siblings.push(json!({"rfilename": "onnx/model.onnx"}));
    siblings.push(json!({"rfilename": "pytorch_model.bin"}));
    let mut replies = vec![Reply::Status {
        status: 200,
        headers: Vec::new(),
        body: json!({"sha": commit, "siblings": siblings}).to_string(),
    }];
    for file in files {
        replies.push(Reply::Raw {
            status: 200,
            headers: Vec::new(),
            body: std::fs::read(tiny_dir().join(file)).unwrap(),
        });
    }
    (replies, files.to_vec())
}

#[tokio::test(flavor = "multi_thread")]
async fn weights_that_do_not_match_the_listed_sha256_never_reach_the_cache() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let wrong = "0".repeat(64);
    let (replies, _) = hub_replies_listing(commit, Some(&wrong));
    let server = MockServer::start(replies).await;
    let temp = TempDir::new("download-tampered");
    let hub = Hub {
        root: temp.0.join("hub"),
        endpoint: server.base_url.clone(),
        offline: false,
    };

    let loaded = tokio::task::spawn_blocking({
        let hub = hub.clone();
        move || SentenceTransformerEmbedder::load_from(&hub, "acme/tiny", true)
    })
    .await
    .unwrap();
    let Err(error) = loaded else {
        panic!("a tampered download must fail");
    };
    let message = error.to_string();
    assert!(
        message.contains("model.safetensors does not match its listed SHA-256"),
        "{message}"
    );

    // Neither the file nor its partial download is left where a load would find
    // it, and the snapshot is not recorded as the model's.
    let repo = hub.root.join("models--acme--tiny");
    let snapshot = repo.join("snapshots").join(commit);
    assert!(!snapshot.join("model.safetensors").exists());
    assert!(!snapshot.join("model.incomplete").exists());
    assert!(!repo.join("refs/main").exists());
    assert_eq!(cached_snapshot(&hub.root, "acme/tiny"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_model_is_downloaded_into_the_python_cache_layout() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let (replies, files) = hub_replies(commit);
    let server = MockServer::start(replies).await;
    let temp = TempDir::new("download");
    let hub = Hub {
        root: temp.0.join("hub"),
        endpoint: format!("{}/", server.base_url)
            .trim_end_matches('/')
            .to_owned(),
        offline: false,
    };

    let embedder = tokio::task::spawn_blocking({
        let hub = hub.clone();
        move || SentenceTransformerEmbedder::load_from(&hub, "acme/tiny", true)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(embedder.spec(), "sentence-transformers:acme/tiny");
    check_against(&embedder, &golden("tiny_st.json"));

    let requests: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.request_line)
        .collect();
    let mut expected =
        vec!["GET /api/models/acme/tiny/revision/main?blobs=true HTTP/1.1".to_owned()];
    expected.extend(
        files
            .iter()
            .map(|file| format!("GET /acme/tiny/resolve/{commit}/{file} HTTP/1.1")),
    );
    assert_eq!(requests, expected);

    // The layout `huggingface_hub` reads: refs/main names the commit and the
    // snapshot holds the files, so Python finds the model offline.
    let repo = hub.root.join("models--acme--tiny");
    assert_eq!(
        std::fs::read_to_string(repo.join("refs/main")).unwrap(),
        commit
    );
    let snapshot = repo.join("snapshots").join(commit);
    for file in &files {
        assert_eq!(
            std::fs::read(snapshot.join(file)).unwrap(),
            std::fs::read(tiny_dir().join(file)).unwrap(),
            "{file}"
        );
    }
    assert!(!snapshot.join("onnx").exists());
    assert_eq!(cached_snapshot(&hub.root, "acme/tiny"), Some(snapshot));

    // Cached and `hf_offline`: no request at all.
    let quiet = MockServer::start(Vec::new()).await;
    let hub = Hub {
        endpoint: quiet.base_url.clone(),
        ..hub
    };
    tokio::task::spawn_blocking(move || {
        SentenceTransformerEmbedder::load_from(&hub, "acme/tiny", true).unwrap()
    })
    .await
    .unwrap();
    assert!(quiet.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_hub_falls_back_to_the_cache_or_fails_as_os_error() {
    let temp = TempDir::new("fallback");
    let commit = "abc123";
    let snapshot = temp.0.join("hub/models--acme--tiny/snapshots").join(commit);
    copy_dir(&tiny_dir(), &snapshot);
    std::fs::create_dir_all(temp.0.join("hub/models--acme--tiny/refs")).unwrap();
    std::fs::write(temp.0.join("hub/models--acme--tiny/refs/main"), commit).unwrap();

    // `hf_offline = false` asks the hub for a newer revision; a server error
    // leaves the cached snapshot in use, as `huggingface_hub` does.
    let server = MockServer::start(vec![Reply::Status {
        status: 503,
        headers: Vec::new(),
        body: "{}".into(),
    }])
    .await;
    let hub = Hub {
        root: temp.0.join("hub"),
        endpoint: server.base_url.clone(),
        offline: false,
    };
    let loaded = {
        let hub = hub.clone();
        tokio::task::spawn_blocking(move || {
            SentenceTransformerEmbedder::load_from(&hub, "acme/tiny", false)
        })
        .await
        .unwrap()
    };
    assert!(loaded.is_ok());
    assert_eq!(server.requests().len(), 1);

    // Not cached: the failure is Python's OSError, offline or not.
    let unreachable = Hub {
        root: temp.0.join("empty"),
        endpoint: "http://127.0.0.1:9".into(),
        offline: false,
    };
    let error = tokio::task::spawn_blocking(move || {
        SentenceTransformerEmbedder::load_from(&unreachable, "acme/tiny", true).unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error.python_name(), "OSError");
    let offline = Hub {
        offline: true,
        ..hub
    };
    let error = SentenceTransformerEmbedder::load_from(&offline, "acme/other", false).unwrap_err();
    assert_eq!(error.python_name(), "OSError");
    assert!(error.to_string().contains("offline"));
}

#[test]
fn an_unsupported_model_is_a_value_error() {
    let temp = TempDir::new("unsupported");
    let dir = temp.0.join("model");
    copy_dir(&tiny_dir(), &dir);
    let config = std::fs::read_to_string(dir.join("config.json")).unwrap();
    std::fs::write(
        dir.join("config.json"),
        config.replace("\"bert\"", "\"roberta\""),
    )
    .unwrap();
    let error = SentenceTransformerEmbedder::from_dir(&dir, "x".into()).unwrap_err();
    assert_eq!(error.python_name(), "ValueError");
}

/// The real default model, from the local cache, when asked for.
#[test]
fn all_minilm_l6_v2_matches_sentence_transformers_when_cached() {
    if std::env::var("AGENT86_TEST_MINILM").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT86_TEST_MINILM=1 to check the cached all-MiniLM-L6-v2");
        return;
    }
    let hub = Hub {
        offline: true,
        ..Hub::from_env()
    };
    let embedder = SentenceTransformerEmbedder::load_from(&hub, "all-MiniLM-L6-v2", true)
        .expect("all-MiniLM-L6-v2 in the local Hugging Face cache");
    assert_eq!(embedder.spec(), "sentence-transformers:all-MiniLM-L6-v2");
    let golden = golden("minilm.json");
    let snapshot = cached_snapshot(&hub.root, "sentence-transformers/all-MiniLM-L6-v2").unwrap();
    for case in golden["cases"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let ids: Vec<u32> = serde_json::from_value(case["ids"].clone()).unwrap();
        assert_eq!(token_ids(&snapshot, text), ids, "{text:?}");
    }
    check_against(&embedder, &golden);
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent86-models-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
