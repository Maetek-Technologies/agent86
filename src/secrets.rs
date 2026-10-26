//! Secret redaction for any text that may be shown to a human.

/// Replacement text for a redacted secret, matching the Python implementation.
pub const REDACTED: &str = "***redacted***";

/// Credential prefixes and the minimum length of the body that follows them.
///
/// Deliberately conservative: a false positive only makes an error message less
/// precise, while a false negative leaks a key.
const KEY_SHAPES: &[KeyShape] = &[
    KeyShape::new("sk-", 16, is_sk_body),
    KeyShape::new("gsk_", 20, is_alphanumeric_body),
    KeyShape::new("xai-", 20, is_alphanumeric_body),
    KeyShape::new("AIza", 20, is_google_body),
];

struct KeyShape {
    prefix: &'static str,
    min_body: usize,
    is_body: fn(char) -> bool,
}

impl KeyShape {
    const fn new(prefix: &'static str, min_body: usize, is_body: fn(char) -> bool) -> Self {
        Self {
            prefix,
            min_body,
            is_body,
        }
    }
}

/// Resolve a provider's API key from the environment variable its config names.
///
/// A missing `api_key_env` means a keyless endpoint, so nothing is looked up.
/// The Python implementation falls back to the OS keyring; that is not ported yet.
pub fn resolve_api_key(api_key_env: Option<&str>) -> Option<String> {
    let name = api_key_env.filter(|name| !name.is_empty())?;
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Return `text` with every known secret and key-shaped token replaced.
///
/// Known secrets shorter than eight characters are ignored, as in the Python
/// implementation, so that short values cannot blank out unrelated text.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if secret.len() >= 8 {
            out = out.replace(secret, REDACTED);
        }
    }
    redact_key_shapes(&out)
}

fn redact_key_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut previous: Option<char> = None;

    'scan: while let Some(ch) = rest.chars().next() {
        if !previous.is_some_and(is_word_char) {
            for &KeyShape {
                prefix,
                min_body,
                is_body,
            } in KEY_SHAPES
            {
                if let Some(after) = rest.strip_prefix(prefix) {
                    let body = &after[..after.find(|c: char| !is_body(c)).unwrap_or(after.len())];
                    // A regex `\b` would backtrack off trailing punctuation such as `.`.
                    let body_len = body.trim_end_matches(|c: char| !is_word_char(c)).len();
                    let token_end = prefix.len() + body_len;
                    let next = rest[token_end..].chars().next();
                    if body_len >= min_body && !next.is_some_and(is_word_char) {
                        out.push_str(REDACTED);
                        previous = rest[..token_end].chars().next_back();
                        rest = &rest[token_end..];
                        continue 'scan;
                    }
                }
            }
        }
        out.push(ch);
        previous = Some(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_alphanumeric_body(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
}

fn is_sk_body(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

fn is_google_body(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_secrets_and_key_shapes_are_redacted() {
        let text = "key my-secret-value and sk-or-v1-abcdef0123456789abcdef in body";
        assert_eq!(
            redact(text, &["my-secret-value"]),
            "key ***redacted*** and ***redacted*** in body"
        );
    }

    #[test]
    fn short_secrets_and_short_tokens_are_left_alone() {
        assert_eq!(redact("abc sk-short", &["abc"]), "abc sk-short");
    }

    #[test]
    fn key_prefixes_inside_words_are_not_tokens() {
        let text = "task-0123456789abcdef0123 gsk_0123456789abcdefghijXYZ";
        assert_eq!(
            redact(text, &[]),
            "task-0123456789abcdef0123 ***redacted***"
        );
    }

    #[test]
    fn trailing_punctuation_is_kept() {
        assert_eq!(
            redact("bad key sk-0123456789abcdef0123.", &[]),
            "bad key ***redacted***."
        );
    }
}
