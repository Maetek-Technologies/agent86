//! Per-model API capability facts, matching the Python `capabilities.py`.

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

/// False when `model` rejects sampling parameters and they must be omitted.
pub fn supports_sampling_params(model: &str) -> bool {
    let ident = normalize_model(model);
    !NO_SAMPLING_FAMILIES
        .iter()
        .any(|family| ident.starts_with(family))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_sampling_families_match_by_prefix() {
        assert!(!supports_sampling_params("claude-opus-5-20260101"));
        assert!(!supports_sampling_params("anthropic:Claude-Sonnet-5"));
        assert!(supports_sampling_params("gpt-4o-mini"));
        assert!(supports_sampling_params("claude-sonnet-4-5"));
    }
}
