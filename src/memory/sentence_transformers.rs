//! The sentence-transformers embedder, Python's `SentenceTransformerEmbedder`,
//! on candle instead of torch.
//!
//! Python's default model, `all-MiniLM-L6-v2`, is a BERT encoder followed by
//! sentence-transformers' pooling and normalization modules. This loads the
//! same files from the same Hugging Face cache and runs the same computation:
//! the tokenizer from `tokenizer.json` (the `tokenizers` crate, which is what
//! Python's tokenizers library runs), truncated to the model's
//! `max_seq_length`; BERT in `float32`; the pooling the model configures; and
//! L2 normalization. Vectors agree with Python's to within float rounding (the
//! kernels sum in a different order), so rows either implementation writes are
//! found by the other. Only BERT models with Transformer, Pooling, and
//! Normalize modules are supported; others fall back as an unloadable model
//! does in Python.

use super::embeddings::Embedder;
use super::hub::{self, Hub};
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{embedding, layer_norm, linear, Embedding, LayerNorm, Linear, VarBuilder};
use serde_json::Value;
use std::path::Path;
use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};

/// Why the model could not be loaded, named as the Python exception the same
/// failure raises, because that name is what Python's fallback note shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// Missing from the cache, offline, or a download or read that failed.
    Os(String),
    /// Files that are present but describe a model this cannot run.
    Value(String),
}

impl LoadError {
    /// The Python exception class the note names.
    pub fn python_name(&self) -> &'static str {
        match self {
            Self::Os(_) => "OSError",
            Self::Value(_) => "ValueError",
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Os(message) | Self::Value(message) => formatter.write_str(message),
        }
    }
}

fn invalid(error: impl std::fmt::Display) -> LoadError {
    LoadError::Value(error.to_string())
}

/// How token vectors become one sentence vector (`1_Pooling/config.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    Mean,
    Cls,
    Max,
    MeanSqrtLen,
}

/// A local sentence-transformers model.
pub struct SentenceTransformerEmbedder {
    spec: String,
    dim: usize,
    tokenizer: Tokenizer,
    bert: Bert,
    pooling: Pooling,
    normalize_modules: usize,
}

impl std::fmt::Debug for SentenceTransformerEmbedder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SentenceTransformerEmbedder")
            .field("spec", &self.spec)
            .field("dim", &self.dim)
            .finish_non_exhaustive()
    }
}

impl SentenceTransformerEmbedder {
    /// Load `model_name` as Python does: a local directory as is; otherwise
    /// from the Hugging Face cache, without a network call when `offline` and
    /// the model is cached (Python's `hf_offline`), else downloading it first.
    pub fn load(model_name: &str, offline: bool) -> Result<Self, LoadError> {
        Self::load_from(&Hub::from_env(), model_name, offline)
    }

    /// [`load`](Self::load) with explicit hub settings.
    pub fn load_from(hub: &Hub, model_name: &str, offline: bool) -> Result<Self, LoadError> {
        let spec = format!("sentence-transformers:{model_name}");
        let local = Path::new(model_name);
        if local.is_dir() {
            return Self::from_dir(local, spec);
        }
        let repo = hub::repo_id(model_name);
        let cached = hub::cached_snapshot(&hub.root, &repo);
        let dir = if hub.offline || (offline && hub::is_cached(&hub.root, model_name)) {
            cached.ok_or_else(|| {
                LoadError::Os(format!(
                    "{repo} is not in the local cache ({}) and the hub is offline",
                    hub.root.display()
                ))
            })?
        } else {
            // `huggingface_hub` checks for a newer revision when online and falls
            // back to the cache when the hub cannot be reached.
            match hub::download(hub, &repo) {
                Ok(dir) => dir,
                Err(error) => cached.ok_or(LoadError::Os(error.0))?,
            }
        };
        Self::from_dir(&dir, spec)
    }

    /// Load a model directory laid out as sentence-transformers saves one.
    pub fn from_dir(dir: &Path, spec: String) -> Result<Self, LoadError> {
        let modules = read_json(&dir.join("modules.json"))?;
        let mut pooling = None;
        let mut normalize_modules = 0;
        let mut transformer = false;
        for module in modules
            .as_array()
            .ok_or_else(|| invalid("modules.json is not a list"))?
        {
            let kind = module["type"].as_str().unwrap_or_default();
            let path = module["path"].as_str().unwrap_or_default();
            match kind.rsplit('.').next().unwrap_or_default() {
                "Transformer" if path.is_empty() => transformer = true,
                "Pooling" => {
                    pooling = Some(read_pooling(&read_json(
                        &dir.join(path).join("config.json"),
                    )?)?)
                }
                "Normalize" => normalize_modules += 1,
                _ => {
                    return Err(invalid(format!(
                        "unsupported sentence-transformers module {kind}"
                    )))
                }
            }
        }
        if !transformer {
            return Err(invalid(
                "modules.json has no Transformer module at the model root",
            ));
        }
        let pooling = pooling.unwrap_or(Pooling::Mean);

        let config = BertConfig::read(&read_json(&dir.join("config.json"))?)?;
        let max_seq_length = max_seq_length(dir, &config)?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(invalid)?;
        // sentence-transformers truncates to `max_seq_length` and pads only a
        // batch; one text at a time needs no padding.
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_seq_length,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
                direction: TruncationDirection::Right,
            }))
            .map_err(invalid)?;
        tokenizer.with_padding(None);

        let device = Device::Cpu;
        let tensors = candle_core::safetensors::load(dir.join("model.safetensors"), &device)
            .map_err(|error| LoadError::Os(error.to_string()))?;
        // Checkpoints saved from a task model nest the encoder under `bert.`.
        let prefixed = tensors.contains_key("bert.embeddings.word_embeddings.weight");
        let weights = VarBuilder::from_tensors(tensors, DType::F32, &device);
        let weights = if prefixed {
            weights.pp("bert")
        } else {
            weights
        };
        let bert = Bert::load(weights, &config).map_err(invalid)?;
        Ok(Self {
            spec,
            dim: config.hidden_size,
            tokenizer,
            bert,
            pooling,
            normalize_modules,
        })
    }

    fn try_encode(&self, text: &str) -> Result<Vec<f32>, Box<dyn std::error::Error + Send + Sync>> {
        let encoding = self.tokenizer.encode(text, true)?;
        let tokens = self.bert.forward(encoding.get_ids())?;
        let pooled = match self.pooling {
            Pooling::Mean => tokens.mean(0)?,
            Pooling::Cls => tokens.get(0)?,
            Pooling::Max => tokens.max(0)?,
            Pooling::MeanSqrtLen => {
                let count = tokens.dim(0)? as f64;
                (tokens.sum(0)? / count.sqrt())?
            }
        };
        // The model's Normalize modules, then `normalize_embeddings=True`.
        let mut vector = pooled.to_vec1::<f32>()?;
        for _ in 0..=self.normalize_modules {
            normalize(&mut vector);
        }
        Ok(vector)
    }
}

impl Embedder for SentenceTransformerEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn spec(&self) -> String {
        self.spec.clone()
    }

    fn encode_one(&self, text: &str) -> Vec<f64> {
        // Tokenizing and the forward pass cannot fail on a model that loaded; a
        // zero vector matches nothing rather than taking the turn down.
        self.try_encode(text)
            .unwrap_or_else(|_| vec![0.0; self.dim])
            .into_iter()
            .map(f64::from)
            .collect()
    }
}

/// torch's `F.normalize(p=2, eps=1e-12)` in `float32`.
fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in vector {
        *x /= norm;
    }
}

fn read_json(path: &Path) -> Result<Value, LoadError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| LoadError::Os(format!("{}: {error}", path.display())))?;
    serde_json::from_str(&text).map_err(|error| invalid(format!("{}: {error}", path.display())))
}

/// The pooling a `1_Pooling/config.json` asks for: the 2.x booleans
/// (`pooling_mode_mean_tokens`, ...) or 5.x's `pooling_mode` name.
pub fn read_pooling(config: &Value) -> Result<Pooling, LoadError> {
    if let Some(mode) = config.get("pooling_mode") {
        let modes: Vec<&str> = match mode {
            Value::String(mode) => vec![mode.as_str()],
            Value::Array(modes) => modes.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        return match modes.as_slice() {
            ["mean"] => Ok(Pooling::Mean),
            ["cls"] => Ok(Pooling::Cls),
            ["max"] => Ok(Pooling::Max),
            ["mean_sqrt_len_tokens"] => Ok(Pooling::MeanSqrtLen),
            _ => Err(invalid(format!("unsupported pooling mode {mode}"))),
        };
    }
    let flags = [
        ("pooling_mode_mean_tokens", Pooling::Mean),
        ("pooling_mode_cls_token", Pooling::Cls),
        ("pooling_mode_max_tokens", Pooling::Max),
        ("pooling_mode_mean_sqrt_len_tokens", Pooling::MeanSqrtLen),
    ];
    let unsupported = ["pooling_mode_weightedmean_tokens", "pooling_mode_lasttoken"];
    if unsupported
        .iter()
        .any(|flag| config[flag].as_bool() == Some(true))
    {
        return Err(invalid("unsupported pooling mode"));
    }
    let on: Vec<Pooling> = flags
        .iter()
        .filter(|(flag, _)| config[flag].as_bool() == Some(true))
        .map(|(_, pooling)| *pooling)
        .collect();
    match on.as_slice() {
        [pooling] => Ok(*pooling),
        // Several modes concatenate their vectors in Python; not supported.
        _ => Err(invalid("unsupported pooling mode")),
    }
}

/// The longest input in tokens: `sentence_bert_config.json`'s
/// `max_seq_length` (2.x), else the tokenizer's `model_max_length` capped by
/// the model's positions, as sentence-transformers decides it.
fn max_seq_length(dir: &Path, config: &BertConfig) -> Result<usize, LoadError> {
    let sbert = dir.join("sentence_bert_config.json");
    if sbert.is_file() {
        if let Some(length) = read_json(&sbert)?["max_seq_length"].as_u64() {
            return Ok(length as usize);
        }
    }
    let tokenizer_config = dir.join("tokenizer_config.json");
    let model_max = if tokenizer_config.is_file() {
        read_json(&tokenizer_config)?["model_max_length"].as_u64()
    } else {
        None
    };
    Ok(model_max.map_or(config.max_position_embeddings, |length| {
        (length as usize).min(config.max_position_embeddings)
    }))
}

/// The `config.json` fields BERT's forward pass uses.
#[derive(Debug, Clone, PartialEq)]
struct BertConfig {
    vocab_size: usize,
    hidden_size: usize,
    num_hidden_layers: usize,
    num_attention_heads: usize,
    intermediate_size: usize,
    max_position_embeddings: usize,
    type_vocab_size: usize,
    layer_norm_eps: f64,
}

impl BertConfig {
    fn read(config: &Value) -> Result<Self, LoadError> {
        if config["model_type"].as_str() != Some("bert") {
            return Err(invalid(format!(
                "unsupported model type {}",
                config["model_type"]
            )));
        }
        let activation = config["hidden_act"].as_str().unwrap_or("gelu");
        if activation != "gelu" {
            return Err(invalid(format!("unsupported activation {activation}")));
        }
        let position = config["position_embedding_type"]
            .as_str()
            .unwrap_or("absolute");
        if position != "absolute" {
            return Err(invalid(format!(
                "unsupported position embeddings {position}"
            )));
        }
        let size = |key: &str| {
            config[key]
                .as_u64()
                .map(|value| value as usize)
                .ok_or_else(|| invalid(format!("config.json has no {key}")))
        };
        let config = Self {
            vocab_size: size("vocab_size")?,
            hidden_size: size("hidden_size")?,
            num_hidden_layers: size("num_hidden_layers")?,
            num_attention_heads: size("num_attention_heads")?,
            intermediate_size: size("intermediate_size")?,
            max_position_embeddings: size("max_position_embeddings")?,
            type_vocab_size: config["type_vocab_size"].as_u64().unwrap_or(2) as usize,
            layer_norm_eps: config["layer_norm_eps"].as_f64().unwrap_or(1e-12),
        };
        if config.num_attention_heads == 0
            || !config
                .hidden_size
                .is_multiple_of(config.num_attention_heads)
        {
            return Err(invalid(
                "hidden_size is not a multiple of num_attention_heads",
            ));
        }
        Ok(config)
    }
}

/// BERT's encoder, as transformers' `BertModel` without the pooler (which
/// sentence-transformers does not use).
struct Bert {
    word: Embedding,
    position: Embedding,
    token_type: Embedding,
    norm: LayerNorm,
    layers: Vec<Layer>,
    heads: usize,
}

struct Layer {
    query: Linear,
    key: Linear,
    value: Linear,
    attention_out: Linear,
    attention_norm: LayerNorm,
    intermediate: Linear,
    output: Linear,
    output_norm: LayerNorm,
}

impl Bert {
    fn load(weights: VarBuilder, config: &BertConfig) -> candle_core::Result<Self> {
        let hidden = config.hidden_size;
        let eps = config.layer_norm_eps;
        let embeddings = weights.pp("embeddings");
        let layers = (0..config.num_hidden_layers)
            .map(|index| {
                let layer = weights.pp("encoder.layer").pp(index.to_string());
                let attention = layer.pp("attention");
                Ok(Layer {
                    query: linear(hidden, hidden, attention.pp("self.query"))?,
                    key: linear(hidden, hidden, attention.pp("self.key"))?,
                    value: linear(hidden, hidden, attention.pp("self.value"))?,
                    attention_out: linear(hidden, hidden, attention.pp("output.dense"))?,
                    attention_norm: layer_norm(hidden, eps, attention.pp("output.LayerNorm"))?,
                    intermediate: linear(
                        hidden,
                        config.intermediate_size,
                        layer.pp("intermediate.dense"),
                    )?,
                    output: linear(config.intermediate_size, hidden, layer.pp("output.dense"))?,
                    output_norm: layer_norm(hidden, eps, layer.pp("output.LayerNorm"))?,
                })
            })
            .collect::<candle_core::Result<_>>()?;
        Ok(Self {
            word: embedding(config.vocab_size, hidden, embeddings.pp("word_embeddings"))?,
            position: embedding(
                config.max_position_embeddings,
                hidden,
                embeddings.pp("position_embeddings"),
            )?,
            token_type: embedding(
                config.type_vocab_size,
                hidden,
                embeddings.pp("token_type_embeddings"),
            )?,
            norm: layer_norm(hidden, eps, embeddings.pp("LayerNorm"))?,
            layers,
            heads: config.num_attention_heads,
        })
    }

    /// The last hidden state for one unpadded sequence: `(tokens, hidden)`.
    fn forward(&self, ids: &[u32]) -> candle_core::Result<Tensor> {
        let device = Device::Cpu;
        let count = ids.len();
        let ids = Tensor::new(ids, &device)?;
        let positions = Tensor::arange(0u32, count as u32, &device)?;
        let types = ids.zeros_like()?;
        // transformers adds the token-type embeddings first, then the positions.
        let embedded = (self.word.forward(&ids)? + self.token_type.forward(&types)?)?;
        let mut hidden = self
            .norm
            .forward(&(embedded + self.position.forward(&positions)?)?)?;
        for layer in &self.layers {
            hidden = layer.forward(&hidden, self.heads)?;
        }
        Ok(hidden)
    }
}

impl Layer {
    fn forward(&self, hidden: &Tensor, heads: usize) -> candle_core::Result<Tensor> {
        let (count, size) = hidden.dims2()?;
        let head_size = size / heads;
        let split = |tensor: Tensor| {
            tensor
                .reshape((count, heads, head_size))?
                .transpose(0, 1)?
                .contiguous()
        };
        let query = split(self.query.forward(hidden)?)?;
        let key = split(self.key.forward(hidden)?)?;
        let value = split(self.value.forward(hidden)?)?;
        let scores = (query.matmul(&key.t()?)? / (head_size as f64).sqrt())?;
        let weights = candle_nn::ops::softmax_last_dim(&scores)?;
        let context = weights
            .matmul(&value)?
            .transpose(0, 1)?
            .contiguous()?
            .reshape((count, size))?;
        let attended = self
            .attention_norm
            .forward(&(self.attention_out.forward(&context)? + hidden)?)?;
        let inner = self.intermediate.forward(&attended)?.gelu_erf()?;
        self.output_norm
            .forward(&(self.output.forward(&inner)? + attended)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pooling_reads_both_config_layouts() {
        let old = json!({
            "word_embedding_dimension": 384,
            "pooling_mode_cls_token": false,
            "pooling_mode_mean_tokens": true,
            "pooling_mode_max_tokens": false,
            "pooling_mode_mean_sqrt_len_tokens": false
        });
        assert_eq!(read_pooling(&old), Ok(Pooling::Mean));
        let new = json!({"embedding_dimension": 16, "pooling_mode": "cls", "include_prompt": true});
        assert_eq!(read_pooling(&new), Ok(Pooling::Cls));
        assert_eq!(
            read_pooling(&json!({"pooling_mode": ["max"]})),
            Ok(Pooling::Max)
        );
        assert!(read_pooling(&json!({"pooling_mode": "lasttoken"})).is_err());
        assert!(read_pooling(&json!({
            "pooling_mode_mean_tokens": true,
            "pooling_mode_max_tokens": true
        }))
        .is_err());
        assert!(read_pooling(&json!({"pooling_mode_lasttoken": true})).is_err());
    }

    #[test]
    fn other_architectures_are_refused_as_python_would_fail_to_load_them() {
        let mut config = json!({
            "model_type": "bert", "vocab_size": 10, "hidden_size": 8,
            "num_hidden_layers": 1, "num_attention_heads": 2,
            "intermediate_size": 16, "max_position_embeddings": 32
        });
        assert!(BertConfig::read(&config).is_ok());
        config["num_attention_heads"] = json!(3);
        assert!(BertConfig::read(&config).is_err());
        config["num_attention_heads"] = json!(2);
        config["model_type"] = json!("xlm-roberta");
        assert_eq!(
            BertConfig::read(&config).unwrap_err().python_name(),
            "ValueError"
        );
    }
}
