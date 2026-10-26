//! Model price table and cost estimation, matching the Python `pricing.py`.
//!
//! Prices are USD per million tokens. Three outcomes are deliberately distinct:
//!
//! - **priced**: a built-in or configured rate exists, and a cost is returned.
//! - **local**: Ollama and llama.cpp run on the user's own hardware, so `0.0` is
//!   the correct cost, not a missing one.
//! - **unknown**: no rate is known, so the UI says "cost n/a" instead of a
//!   fabricated `$0.00`.
//!
//! Lookup order (first hit wins): config overrides, local providers, then the
//! built-in table. Each step tries the full `provider:model` ref, then the bare
//! model id, then a dated-snapshot prefix match so `claude-sonnet-5-20260101`
//! resolves to `claude-sonnet-5`.

use crate::config::Config;
use crate::types::Usage;
use std::collections::BTreeMap;

/// A cache read is a tenth of the input price.
pub const CACHE_READ_MULTIPLIER: f64 = 0.1;
/// A 5-minute cache write is a 25% premium over the input price.
pub const CACHE_WRITE_MULTIPLIER: f64 = 1.25;

/// Models whose cache reads are cheaper than the usual multiplier.
const CACHE_READ_MULTIPLIER_OVERRIDES: &[(&str, f64)] = &[("claude-fable-5-1", 0.025)];

/// Providers whose models run on the user's own hardware. Zero is the real price.
const LOCAL_PROVIDERS: &[&str] = &["ollama", "llamacpp", "llama.cpp", "local"];

/// Where a price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    Builtin,
    Local,
    Config,
}

/// USD per million tokens for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub source: PriceSource,
    /// `None` derives the rate from the input rate via the standard multiplier.
    pub cache_read_per_mtok: Option<f64>,
    pub cache_write_per_mtok: Option<f64>,
}

impl Price {
    pub const fn new(input_per_mtok: f64, output_per_mtok: f64, source: PriceSource) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
            source,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        }
    }

    const fn builtin(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self::new(input_per_mtok, output_per_mtok, PriceSource::Builtin)
    }

    /// USD per million tokens served from the prompt cache.
    pub fn cache_read_rate(&self, model: &str) -> f64 {
        self.cache_read_per_mtok
            .unwrap_or_else(|| self.input_per_mtok * cache_read_multiplier(model))
    }

    /// USD per million tokens written to the prompt cache.
    pub fn cache_write_rate(&self) -> f64 {
        self.cache_write_per_mtok
            .unwrap_or(self.input_per_mtok * CACHE_WRITE_MULTIPLIER)
    }

    /// Cost of one call, with the cached portions of the prompt billed at their own rates.
    ///
    /// `input_tokens` is the whole prompt; the uncached remainder is what is left after
    /// the two cache classes are taken out. The arithmetic order matches Python so the
    /// result is bit-for-bit identical.
    pub fn cost(&self, usage: &Usage, model: &str) -> f64 {
        let cached = usage.cache_read_tokens + usage.cache_creation_tokens;
        let uncached = usage.input_tokens.saturating_sub(cached);
        (uncached as f64 / 1_000_000.0) * self.input_per_mtok
            + (usage.cache_read_tokens as f64 / 1_000_000.0) * self.cache_read_rate(model)
            + (usage.cache_creation_tokens as f64 / 1_000_000.0) * self.cache_write_rate()
            + (usage.output_tokens as f64 / 1_000_000.0) * self.output_per_mtok
    }
}

const LOCAL_PRICE: Price = Price::new(0.0, 0.0, PriceSource::Local);

/// Built-in prices, keyed by bare model id. Dated snapshots resolve by prefix.
///
/// Anthropic rates are from the `claude-api` skill (cached 2026-06-24); OpenAI rates
/// from developers.openai.com/api/docs/pricing (fetched 2026-09-19), as in Python.
/// Groq and OpenRouter are intentionally absent: OpenRouter ids are `vendor/model`
/// with per-route pricing that cannot be verified from the id alone. Unknown beats wrong.
const PRICES: &[(&str, Price)] = &[
    // Anthropic
    ("claude-fable-5-1", Price::builtin(10.00, 50.00)),
    ("claude-mythos-5-1", Price::builtin(10.00, 50.00)),
    ("claude-fable-5", Price::builtin(10.00, 50.00)),
    ("claude-opus-5", Price::builtin(5.00, 25.00)),
    ("claude-opus-4-8", Price::builtin(5.00, 25.00)),
    ("claude-opus-4-7", Price::builtin(5.00, 25.00)),
    ("claude-opus-4-6", Price::builtin(5.00, 25.00)),
    ("claude-sonnet-5", Price::builtin(2.00, 10.00)),
    ("claude-sonnet-4-6", Price::builtin(3.00, 15.00)),
    ("claude-haiku-4-5", Price::builtin(1.00, 5.00)),
    // OpenAI
    ("gpt-6-astra", Price::builtin(10.00, 50.00)),
    ("gpt-5.6-sol", Price::builtin(4.00, 20.00)),
    ("gpt-5.6-terra", Price::builtin(2.00, 12.00)),
    ("gpt-5.6-luna", Price::builtin(0.20, 1.20)),
    ("gpt-5.5", Price::builtin(5.00, 30.00)),
    ("gpt-5.5-pro", Price::builtin(30.00, 180.00)),
    ("gpt-5.4", Price::builtin(2.50, 15.00)),
    ("gpt-5.4-mini", Price::builtin(0.75, 4.50)),
    ("gpt-5.4-nano", Price::builtin(0.20, 1.25)),
    ("gpt-5.4-pro", Price::builtin(30.00, 180.00)),
    ("gpt-5.2", Price::builtin(1.75, 14.00)),
    ("gpt-5.2-pro", Price::builtin(21.00, 168.00)),
    ("gpt-5.1", Price::builtin(1.25, 10.00)),
    ("gpt-5", Price::builtin(1.25, 10.00)),
    ("gpt-5-mini", Price::builtin(0.25, 2.00)),
    ("gpt-5-nano", Price::builtin(0.05, 0.40)),
    ("gpt-5-pro", Price::builtin(15.00, 120.00)),
    ("gpt-4.1", Price::builtin(2.00, 8.00)),
    ("gpt-4.1-mini", Price::builtin(0.40, 1.60)),
    ("gpt-4.1-nano", Price::builtin(0.10, 0.40)),
    ("gpt-4o", Price::builtin(2.50, 10.00)),
    ("gpt-4o-mini", Price::builtin(0.15, 0.60)),
    ("o1", Price::builtin(15.00, 60.00)),
    ("o1-pro", Price::builtin(150.00, 600.00)),
    ("o3", Price::builtin(2.00, 8.00)),
    ("o3-pro", Price::builtin(20.00, 80.00)),
    ("o3-mini", Price::builtin(1.10, 4.40)),
    ("o4-mini", Price::builtin(1.10, 4.40)),
    ("gpt-3.5-turbo", Price::builtin(0.50, 1.50)),
];

/// The built-in table plus `[pricing.models]` overrides from config.
///
/// Python keeps overrides in a module-level table; here they travel with the
/// table so providers and the UI always consult the same rates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriceTable {
    overrides: BTreeMap<String, Price>,
}

impl PriceTable {
    /// The built-in prices with no overrides.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// The built-in prices plus the config's `[pricing.models]` overrides.
    pub fn from_config(config: &Config) -> Self {
        let overrides = config
            .pricing
            .models
            .iter()
            .map(|(model, price)| {
                let price = Price::new(
                    price.input_per_mtok,
                    price.output_per_mtok,
                    PriceSource::Config,
                );
                (model.clone(), price)
            })
            .collect();
        Self { overrides }
    }

    /// Resolve a price for a `provider:model` ref or bare model id.
    pub fn lookup(&self, model: &str) -> Option<Price> {
        if let Some(price) = find(self.overrides.iter().map(|(k, v)| (k.as_str(), v)), model) {
            return Some(price);
        }
        if LOCAL_PROVIDERS.contains(&provider_of(model).as_str()) {
            return Some(LOCAL_PRICE);
        }
        find(PRICES.iter().map(|(k, v)| (*k, v)), model)
    }

    /// True when a cost figure for `model` is meaningful, including free local models.
    pub fn is_priced(&self, model: &str) -> bool {
        self.lookup(model).is_some()
    }

    /// Estimated USD cost, or `None` when the model's price is unknown.
    pub fn estimate_cost(&self, model: &str, usage: &Usage) -> Option<f64> {
        self.lookup(model).map(|price| price.cost(usage, model))
    }

    /// `usage` with `cost_usd` filled in; an unknown model costs `0.0`.
    ///
    /// Ask [`PriceTable::is_priced`] before displaying the total, so an unpriced
    /// model reads as "n/a" rather than "$0.00".
    pub fn priced_usage(&self, model: &str, usage: Usage) -> Usage {
        Usage {
            cost_usd: self.estimate_cost(model, &usage).unwrap_or(0.0),
            ..usage
        }
    }
}

/// `"anthropic:claude-opus-5"` -> `["anthropic:claude-opus-5", "claude-opus-5"]`.
fn candidates(model: &str) -> Vec<&str> {
    let reference = model.trim();
    if reference.is_empty() {
        return Vec::new();
    }
    let mut out = vec![reference];
    if let Some((provider, rest)) = reference.split_once(':') {
        if !provider.is_empty() && !rest.is_empty() {
            out.push(rest);
        }
    }
    out
}

fn provider_of(model: &str) -> String {
    match model.trim().split_once(':') {
        Some((provider, rest)) if !rest.is_empty() => provider.to_lowercase(),
        _ => String::new(),
    }
}

fn find<'a>(
    table: impl Iterator<Item = (&'a str, &'a Price)> + Clone,
    model: &str,
) -> Option<Price> {
    let candidates = candidates(model);
    for candidate in &candidates {
        if let Some((_, price)) = table.clone().find(|(key, _)| key == candidate) {
            return Some(*price);
        }
    }
    // Dated-snapshot prefix match; the longest key wins.
    let mut best: Option<(usize, Price)> = None;
    for candidate in &candidates {
        for (key, price) in table.clone() {
            let is_snapshot = key.len() < candidate.len()
                && candidate.starts_with(key)
                && is_snapshot_suffix(&candidate[key.len()..]);
            if is_snapshot && best.is_none_or(|(length, _)| key.len() > length) {
                best = Some((key.len(), *price));
            }
        }
    }
    best.map(|(_, price)| price)
}

/// A dated snapshot suffix: `-20260101`, `@20251101` (Vertex), or `_20260101`.
///
/// Matching is restricted to this shape on purpose: a loose prefix match would
/// price `gpt-5.6-sol` off the `gpt-5` entry.
fn is_snapshot_suffix(suffix: &str) -> bool {
    let bytes = suffix.as_bytes();
    bytes.len() == 9
        && matches!(bytes[0], b'-' | b'@' | b'_')
        && bytes[1..3] == *b"20"
        && bytes[3..].iter().all(u8::is_ascii_digit)
}

fn cache_read_multiplier(model: &str) -> f64 {
    let ident = model
        .split_once(':')
        .map_or(model, |(_, rest)| rest)
        .trim()
        .to_lowercase();
    CACHE_READ_MULTIPLIER_OVERRIDES
        .iter()
        .filter(|(key, _)| ident.starts_with(key))
        .max_by_key(|(key, _)| key.len())
        .map_or(CACHE_READ_MULTIPLIER, |(_, multiplier)| *multiplier)
}

/// Render a cost, or the unpriced label when no price is known.
pub fn format_cost(cost_usd: f64, priced: bool) -> String {
    if priced {
        format!("${cost_usd:.4}")
    } else {
        UNPRICED_LABEL.to_owned()
    }
}

/// Shown instead of a dollar figure when no price is known for the model.
pub const UNPRICED_LABEL: &str = "cost n/a (unpriced model)";

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
            ..Usage::default()
        }
    }

    #[test]
    fn refs_and_bare_ids_resolve_and_openrouter_stays_unknown() {
        let table = PriceTable::builtin();
        assert_eq!(table.lookup("gpt-4o-mini").unwrap().input_per_mtok, 0.15);
        assert_eq!(
            table.lookup("openai:gpt-4o-mini").unwrap().output_per_mtok,
            0.60
        );
        assert!(table.lookup("openrouter:openai/gpt-4o-mini").is_none());
        assert!(table.lookup("openai/gpt-4o-mini").is_none());
        assert!(table.lookup("").is_none());
    }

    #[test]
    fn only_dated_snapshots_match_by_prefix() {
        let table = PriceTable::builtin();
        assert_eq!(
            table
                .lookup("claude-sonnet-5-20260101")
                .unwrap()
                .input_per_mtok,
            2.0
        );
        assert_eq!(
            table
                .lookup("anthropic:claude-opus-4-8@20260101")
                .unwrap()
                .input_per_mtok,
            5.0
        );
        // gpt-5.6-sol has its own entry and must not be priced off gpt-5.
        assert_eq!(table.lookup("gpt-5.6-sol").unwrap().input_per_mtok, 4.0);
        assert!(table.lookup("gpt-5-turbo").is_none());
        assert!(table.lookup("claude-sonnet-5-2026").is_none());
    }

    #[test]
    fn local_providers_are_free_not_unknown() {
        let table = PriceTable::builtin();
        let price = table.lookup("ollama:llama3.1").unwrap();
        assert_eq!(price.source, PriceSource::Local);
        assert_eq!(price.cost(&usage(1000, 1000, 0, 0), "ollama:llama3.1"), 0.0);
        assert!(table.lookup("llama3.1").is_none());
    }

    #[test]
    fn costs_split_cached_tokens_like_python() {
        let table = PriceTable::builtin();
        // 12 in / 7 out on gpt-4o-mini: Python computes 6.000000000000001e-06.
        let cost = table
            .estimate_cost("gpt-4o-mini", &usage(12, 7, 0, 0))
            .unwrap();
        assert_eq!(cost, 12.0 / 1e6 * 0.15 + 7.0 / 1e6 * 0.60);

        // 1M prompt tokens, 400k read from cache and 100k written, on claude-opus-5.
        let cost = table
            .estimate_cost("claude-opus-5", &usage(1_000_000, 0, 400_000, 100_000))
            .unwrap();
        let expected = 0.5 * 5.0 + 0.4 * (5.0 * 0.1) + 0.1 * (5.0 * 1.25);
        assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");

        // Fable 5.1 reads its cache at 0.025x.
        let fable = table
            .estimate_cost("claude-fable-5-1", &usage(1_000_000, 0, 1_000_000, 0))
            .unwrap();
        assert!((fable - 0.25).abs() < 1e-12, "{fable}");
    }

    #[test]
    fn unknown_models_cost_zero_but_are_reported_unpriced() {
        let table = PriceTable::builtin();
        let priced = table.priced_usage("mystery", usage(10, 10, 0, 0));
        assert_eq!(priced.cost_usd, 0.0);
        assert_eq!(priced.input_tokens, 10);
        assert!(!table.is_priced("mystery"));
        assert_eq!(format_cost(0.0, false), UNPRICED_LABEL);
        assert_eq!(format_cost(0.000006, true), "$0.0000");
        assert_eq!(format_cost(0.01234, true), "$0.0123");
    }

    #[test]
    fn snapshot_suffixes_have_a_strict_shape() {
        assert!(is_snapshot_suffix("-20260101"));
        assert!(is_snapshot_suffix("@20251101"));
        assert!(is_snapshot_suffix("_20260101"));
        assert!(!is_snapshot_suffix("-19990101"));
        assert!(!is_snapshot_suffix("-2026010"));
        assert!(!is_snapshot_suffix("-2026010a"));
    }
}
