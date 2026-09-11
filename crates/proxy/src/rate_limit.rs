//! Staying out of a provider's rate limit.
//!
//! A 429 costs nothing — its reservation is released (ADR-0006) — but sending
//! into a limit that has not reset only earns another, and a client that
//! retries each one turns that into a storm. So each upstream has a gate. It
//! closes on a 429 until the limit resets, and closes pre-emptively when a
//! successful response reports that nothing remains.

use std::sync::Mutex;
use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use tokio::time::Instant;

/// The shortest a 429 closes the gate, when the provider gives no reset time.
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);
/// The longest any single signal can close the gate.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug, Default)]
pub struct RateLimitGate {
    closed_until: Mutex<Option<Instant>>,
}

impl RateLimitGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// How much longer the gate stays closed, or `None` when open.
    pub fn closed_for(&self, now: Instant) -> Option<Duration> {
        let closed_until = *self.lock();
        closed_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    /// A 429: closed until the provider says the limit resets, or for
    /// [`MIN_BACKOFF`] when it gives no usable time.
    pub fn record_rate_limited(&self, headers: &HeaderMap, now: Instant) {
        let wait = [
            retry_after(headers),
            reset(headers, "requests"),
            reset(headers, "tokens"),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(MIN_BACKOFF)
        .clamp(MIN_BACKOFF, MAX_BACKOFF);
        self.close_until(now + wait);
    }

    /// A successful response: if it reports no requests or no tokens left in
    /// the current window, close until that window resets.
    pub fn record_headers(&self, headers: &HeaderMap, now: Instant) {
        for kind in ["requests", "tokens"] {
            let exhausted = header_text(headers, &format!("x-ratelimit-remaining-{kind}"))
                .and_then(|text| text.trim().parse::<u64>().ok())
                == Some(0);
            if exhausted && let Some(wait) = reset(headers, kind) {
                self.close_until(now + wait.min(MAX_BACKOFF));
            }
        }
    }

    /// Only ever extends the closed period, never shortens it.
    fn close_until(&self, until: Instant) {
        let mut closed_until = self.lock();
        if closed_until.is_none_or(|current| until > current) {
            *closed_until = Some(until);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.closed_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `Retry-After` in whole seconds. The HTTP-date form is ignored.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    header_text(headers, RETRY_AFTER.as_str())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn reset(headers: &HeaderMap, kind: &str) -> Option<Duration> {
    header_text(headers, &format!("x-ratelimit-reset-{kind}")).and_then(parse_reset)
}

/// Parses OpenAI's reset durations, such as `1s`, `6m0s`, `120ms` or `1h2m3.5s`.
pub fn parse_reset(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut rest = text;
    let mut total = 0.0f64;
    while !rest.is_empty() {
        let number_len = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        if number_len == 0 {
            return None;
        }
        let value: f64 = rest[..number_len].parse().ok()?;
        rest = &rest[number_len..];
        let (unit_seconds, unit_len) = if rest.starts_with("ms") {
            (0.001, 2)
        } else if rest.starts_with('h') {
            (3600.0, 1)
        } else if rest.starts_with('m') {
            (60.0, 1)
        } else if rest.starts_with('s') {
            (1.0, 1)
        } else {
            return None;
        };
        total += value * unit_seconds;
        rest = &rest[unit_len..];
    }
    (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn reset_durations_parse() {
        assert_eq!(parse_reset("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_reset("120ms"), Some(Duration::from_millis(120)));
        assert_eq!(parse_reset("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(
            parse_reset("1h2m3.5s"),
            Some(Duration::from_secs_f64(3723.5))
        );
        assert_eq!(parse_reset("0s"), Some(Duration::ZERO));
        for bad in ["", "soon", "5", "s5", "1x", "--1s"] {
            assert_eq!(parse_reset(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_rate_limit_closes_the_gate_until_the_longest_reset() {
        let gate = RateLimitGate::new();
        let now = Instant::now();
        assert_eq!(gate.closed_for(now), None);
        gate.record_rate_limited(
            &headers(&[
                ("x-ratelimit-reset-requests", "120ms"),
                ("x-ratelimit-reset-tokens", "6s"),
            ]),
            now,
        );
        assert_eq!(gate.closed_for(now), Some(Duration::from_secs(6)));
        assert_eq!(gate.closed_for(now + Duration::from_secs(6)), None);
    }

    #[test]
    fn a_rate_limit_without_timing_closes_for_the_minimum() {
        let gate = RateLimitGate::new();
        let now = Instant::now();
        gate.record_rate_limited(&HeaderMap::new(), now);
        assert_eq!(gate.closed_for(now), Some(MIN_BACKOFF));
    }

    #[test]
    fn retry_after_counts_and_the_wait_is_capped() {
        let gate = RateLimitGate::new();
        let now = Instant::now();
        gate.record_rate_limited(&headers(&[("retry-after", "86400")]), now);
        assert_eq!(gate.closed_for(now), Some(MAX_BACKOFF));
    }

    #[test]
    fn an_exhausted_window_closes_the_gate_before_any_429() {
        let gate = RateLimitGate::new();
        let now = Instant::now();
        gate.record_headers(
            &headers(&[
                ("x-ratelimit-remaining-requests", "12"),
                ("x-ratelimit-remaining-tokens", "0"),
                ("x-ratelimit-reset-tokens", "2s"),
            ]),
            now,
        );
        assert_eq!(gate.closed_for(now), Some(Duration::from_secs(2)));
    }

    #[test]
    fn remaining_capacity_leaves_the_gate_open() {
        let gate = RateLimitGate::new();
        let now = Instant::now();
        gate.record_headers(
            &headers(&[
                ("x-ratelimit-remaining-requests", "499"),
                ("x-ratelimit-remaining-tokens", "500000"),
                ("x-ratelimit-reset-tokens", "0s"),
            ]),
            now,
        );
        assert_eq!(gate.closed_for(now), None);
    }

    #[test]
    fn a_shorter_signal_never_reopens_the_gate_early() {
        let gate = RateLimitGate::new();
        let now = Instant::now();
        gate.record_rate_limited(&headers(&[("retry-after", "30")]), now);
        gate.record_rate_limited(&headers(&[("retry-after", "2")]), now);
        assert_eq!(gate.closed_for(now), Some(Duration::from_secs(30)));
    }
}
