//! Retry with exponential backoff, matching the Python `retry.py`.
//!
//! Two rules providers must honour:
//!
//! - Never retry after a delta has been yielded; the caller has already seen that text.
//! - Honour `Retry-After` when the server names a delay.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

/// Default retry budget when the provider config does not set one.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

const BASE_DELAY_S: f64 = 0.5;
const MAX_DELAY_S: f64 = 30.0;
/// An interactive CLI must not silently sit on an absurd `Retry-After`.
const MAX_RETRY_AFTER_S: f64 = 60.0;

/// Rate limiting and transient upstream failures. Everything else is a caller
/// error that would fail identically on every retry.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Parse a delta-seconds `Retry-After` value, capped at 60 seconds.
///
/// HTTP-date values are not parsed yet and fall back to exponential backoff.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let seconds: f64 = value.trim().parse().ok()?;
    if !seconds.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(
        seconds.clamp(0.0, MAX_RETRY_AFTER_S),
    ))
}

/// Exponential backoff with equal jitter: half the window fixed, half random.
pub fn backoff_delay(attempt: u32) -> Duration {
    let window = (BASE_DELAY_S * 2f64.powi(attempt.min(16) as i32)).min(MAX_DELAY_S);
    Duration::from_secs_f64(window / 2.0 + jitter_fraction() * window / 2.0)
}

/// A value in `[0, 1)` from the standard library's randomly seeded hasher.
fn jitter_fraction() -> f64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0);
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Per-request retry budget.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    max_retries: u32,
    attempt: u32,
}

impl RetryPolicy {
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            attempt: 0,
        }
    }

    /// The delay before the next attempt, or `None` when no retry is allowed.
    ///
    /// `emitted` vetoes the retry: deltas already reached the caller.
    pub fn next_delay(&mut self, emitted: bool, retry_after: Option<Duration>) -> Option<Duration> {
        if emitted || self.attempt >= self.max_retries {
            return None;
        }
        let delay = retry_after.unwrap_or_else(|| backoff_delay(self.attempt));
        self.attempt += 1;
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transient_statuses_are_retryable() {
        for status in [429, 500, 502, 503, 504] {
            assert!(is_retryable_status(status));
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(status));
        }
    }

    #[test]
    fn retry_after_seconds_are_capped() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after(" 0.5 "), Some(Duration::from_millis(500)));
        assert_eq!(parse_retry_after("3600"), Some(Duration::from_secs(60)));
        assert_eq!(parse_retry_after("-5"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    #[test]
    fn backoff_stays_within_its_jitter_window() {
        for attempt in 0..8 {
            let window = (BASE_DELAY_S * 2f64.powi(attempt as i32)).min(MAX_DELAY_S);
            let delay = backoff_delay(attempt).as_secs_f64();
            assert!(
                delay >= window / 2.0 && delay <= window,
                "{attempt}: {delay}"
            );
        }
    }

    #[test]
    fn budget_is_spent_and_emission_vetoes_retries() {
        let mut policy = RetryPolicy::new(2);
        assert!(policy.next_delay(true, None).is_none());
        assert!(policy.next_delay(false, Some(Duration::ZERO)).is_some());
        assert!(policy.next_delay(false, Some(Duration::ZERO)).is_some());
        assert!(policy.next_delay(false, Some(Duration::ZERO)).is_none());
    }
}
