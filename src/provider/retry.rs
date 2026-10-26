//! Retry with exponential backoff for transient provider failures.
//!
//! Two rules every provider honours: never retry after text has reached the
//! consumer (a second attempt would duplicate it), and honour `Retry-After`.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    time::Duration,
};

const BASE_DELAY_S: f64 = 0.5;
const MAX_DELAY_S: f64 = 30.0;
const MAX_RETRY_AFTER_S: f64 = 60.0;

pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Seconds to wait per a delta-seconds `Retry-After` value, capped at 60s.
///
/// The HTTP-date form is not parsed yet; it falls back to normal backoff.
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let seconds: f64 = value?.trim().parse().ok()?;
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
    Duration::from_secs_f64(window / 2.0 + jitter() * window / 2.0)
}

/// A value in `[0, 1)` from the standard library's per-process random hash keys.
fn jitter() -> f64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0);
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_parsed_and_capped() {
        assert_eq!(parse_retry_after(Some("2")), Some(Duration::from_secs(2)));
        assert_eq!(
            parse_retry_after(Some(" 0.5 ")),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            parse_retry_after(Some("3600")),
            Some(Duration::from_secs(60))
        );
        assert_eq!(parse_retry_after(Some("-4")), Some(Duration::ZERO));
        assert_eq!(parse_retry_after(Some("soon")), None);
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn backoff_stays_inside_its_window() {
        for attempt in 0..8 {
            let window = (BASE_DELAY_S * 2f64.powi(attempt as i32)).min(MAX_DELAY_S);
            let delay = backoff_delay(attempt).as_secs_f64();
            assert!(
                delay >= window / 2.0 && delay <= window,
                "attempt {attempt}: {delay}"
            );
        }
    }

    #[test]
    fn only_transient_statuses_are_retried() {
        for status in [429, 500, 502, 503, 504] {
            assert!(is_retryable_status(status));
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(status));
        }
    }
}
