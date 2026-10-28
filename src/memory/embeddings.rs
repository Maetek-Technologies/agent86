//! Embeddings, ported from Python's `memory/embeddings.py`.
//!
//! Python's default is a local `sentence-transformers` model. That needs torch,
//! which is not always installed, so `build_embedder` falls back to the
//! dependency-free [`HashingEmbedder`] and memory still works. The hashing
//! embedder is ported exactly (same tokens, same MD5 buckets, same signs), so a
//! `hash:<dim>` vector written by either implementation is found by the other.
//! The sentence-transformers model is not ported yet; its spec takes the same
//! fallback Python takes when the model cannot load.

use regex::Regex;
use std::sync::{Arc, LazyLock};

/// Turns text into unit-norm vectors.
pub trait Embedder: Send + Sync {
    /// Length of every vector this embedder produces.
    fn dim(&self) -> usize;

    /// The spec that names this embedder, e.g. `hash:256`.
    fn spec(&self) -> String;

    /// The vector for one text, in `f64` (it is stored as `f32`).
    fn encode_one(&self, text: &str) -> Vec<f64>;
}

/// Word tokens, as Python's `[a-z0-9]+` over the lowercased text.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new("[a-z0-9]+").expect("valid pattern"));

/// Deterministic hashing embedder: no model, stable across runs.
///
/// Hashes word tokens into `dim` signed buckets and L2-normalizes. Good enough
/// for near-duplicate and keyword-overlap recall; not semantically rich like a
/// trained model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashingEmbedder {
    dim: usize,
}

impl HashingEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(256)
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn spec(&self) -> String {
        format!("hash:{}", self.dim)
    }

    fn encode_one(&self, text: &str) -> Vec<f64> {
        let mut vector = vec![0.0; self.dim];
        if self.dim == 0 {
            return vector;
        }
        // Python's `str.lower()` and Rust's `to_lowercase` agree on everything the
        // ASCII-only token pattern can match.
        for token in TOKEN.find_iter(&text.to_lowercase()) {
            let digest = md5::compute(token.as_str().as_bytes()).0;
            let index = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]) as usize
                % self.dim;
            vector[index] += if digest[4] & 1 == 1 { 1.0 } else { -1.0 };
        }
        let norm = python_sum(vector.iter().map(|x| x * x)).sqrt();
        if norm == 0.0 {
            return vector;
        }
        vector.iter().map(|x| x / norm).collect()
    }
}

/// An embedder from a spec string, with a note when the requested one was
/// unavailable, as Python's `build_embedder` with fallback allowed.
pub fn build_embedder(spec: &str) -> (Arc<dyn Embedder>, Option<String>) {
    let (scheme, name) = spec.split_once(':').unwrap_or((spec, ""));
    match scheme.trim() {
        "hash" => {
            // Python's `str.isdigit()` on the stripped name, else 256.
            let digits = name.trim();
            let dim = if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                digits.parse().unwrap_or(256)
            } else {
                256
            };
            (Arc::new(HashingEmbedder::new(dim)), None)
        }
        "sentence-transformers" => (
            Arc::new(HashingEmbedder::default()),
            Some(
                "sentence-transformers unavailable (not ported to the Rust build yet); using the \
                 offline hash embedder."
                    .to_owned(),
            ),
        ),
        _ => (
            Arc::new(HashingEmbedder::default()),
            Some(format!("unknown embedder '{spec}'; using hash embedder.")),
        ),
    }
}

/// `vector` as the little-endian `float32` BLOB Python's `array("f")` writes.
pub fn pack(vector: &[f64]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|x| (*x as f32).to_le_bytes())
        .collect()
}

/// A stored BLOB back as `f64`s; a trailing partial float is ignored.
pub fn unpack(blob: &[u8]) -> Vec<f64> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f64::from(f32::from_le_bytes(*bytes)))
        .collect()
}

/// Cosine similarity of unit-norm vectors: their dot product, as Python's store
/// computes it.
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    python_sum(a.iter().zip(b).map(|(x, y)| x * y))
}

/// Python's built-in `sum` over floats. Since 3.12 it carries a Neumaier
/// compensation term, so a plain left fold can differ in the last bit, and a
/// score that differs in the last bit can sort differently. (Python 3.11 sums
/// naively; the oldest Python the reference supports may differ by an ulp.)
pub fn python_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0f64;
    let mut compensation = 0.0f64;
    for value in values {
        let sum = total + value;
        if total.abs() >= value.abs() {
            compensation += (total - sum) + value;
        } else {
            compensation += (value - sum) + total;
        }
        total = sum;
    }
    if compensation != 0.0 && compensation.is_finite() {
        total += compensation;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_pick_the_embedder_and_explain_fallbacks() {
        assert_eq!(build_embedder("hash:64").0.spec(), "hash:64");
        assert_eq!(build_embedder(" hash : 32 ").0.spec(), "hash:32");
        assert_eq!(build_embedder("hash").0.spec(), "hash:256");
        assert_eq!(build_embedder("hash:x").0.spec(), "hash:256");
        assert!(build_embedder("hash:8").1.is_none());

        let (embedder, note) = build_embedder("sentence-transformers:all-MiniLM-L6-v2");
        assert_eq!(embedder.spec(), "hash:256");
        assert!(note.unwrap().contains("using the offline hash embedder"));

        let (embedder, note) = build_embedder("word2vec:big");
        assert_eq!(embedder.spec(), "hash:256");
        assert_eq!(
            note.unwrap(),
            "unknown embedder 'word2vec:big'; using hash embedder."
        );
    }

    #[test]
    fn blobs_round_trip_through_float32() {
        let vector = vec![0.5, -0.25, 1.0 / 3.0];
        let unpacked = unpack(&pack(&vector));
        assert_eq!(unpacked[..2], [0.5, -0.25]);
        assert_eq!(unpacked[2], f64::from((1.0f64 / 3.0) as f32));
    }
}
