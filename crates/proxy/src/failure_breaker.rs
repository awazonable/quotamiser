//! Holding a provider back after consecutive ambiguous failures.
//!
//! A 5xx, a timeout, or a stream that breaks before its terminal event holds
//! the reservation, because the request may have been processed (ADR-0006).
//! Codex answers each such failure with a new request, and each new request
//! holds another reservation. A provider failing this way would turn the pool
//! into held reservations.
//!
//! After `threshold` ambiguous failures with no completed response between
//! them, the breaker opens for a cooldown. Once it has opened, the next
//! ambiguous failure opens it again, with the cooldown doubled up to a cap. A
//! completed response resets both. Failures that arrive while it is open come
//! from requests sent before it opened, and do not extend it.
//!
//! Nothing is persisted: a restart closes the breaker.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerPolicy {
    pub threshold: u32,
    pub initial_cooldown: Duration,
    pub max_cooldown: Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            threshold: 3,
            initial_cooldown: Duration::from_secs(30),
            max_cooldown: Duration::from_secs(600),
        }
    }
}

#[derive(Debug)]
pub struct FailureBreaker {
    policy: BreakerPolicy,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    next_cooldown: Duration,
}

impl FailureBreaker {
    pub fn new(policy: BreakerPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(State {
                consecutive_failures: 0,
                open_until: None,
                next_cooldown: policy.initial_cooldown,
            }),
        }
    }

    /// How much longer the breaker stays open, or `None` when closed.
    pub fn open_for(&self, now: Instant) -> Option<Duration> {
        let open_until = self.lock().open_until;
        open_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }

    /// Sent, and it cannot be shown that generation did not start: a
    /// transport error, a 5xx or other unlisted status, or a stream that
    /// ended before its terminal event.
    pub fn record_ambiguous_failure(&self, now: Instant) {
        let mut state = self.lock();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let open = state.open_until.is_some_and(|until| until > now);
        if !open && state.consecutive_failures >= self.policy.threshold {
            state.open_until = Some(now + state.next_cooldown);
            state.next_cooldown = (state.next_cooldown * 2).min(self.policy.max_cooldown);
        }
    }

    /// A terminal event arrived: the provider is completing responses.
    pub fn record_completion(&self) {
        let mut state = self.lock();
        state.consecutive_failures = 0;
        state.next_cooldown = self.policy.initial_cooldown;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker() -> FailureBreaker {
        FailureBreaker::new(BreakerPolicy {
            threshold: 3,
            initial_cooldown: Duration::from_secs(30),
            max_cooldown: Duration::from_secs(100),
        })
    }

    #[test]
    fn it_opens_at_the_threshold() {
        let breaker = breaker();
        let now = Instant::now();
        breaker.record_ambiguous_failure(now);
        breaker.record_ambiguous_failure(now);
        assert_eq!(breaker.open_for(now), None);
        breaker.record_ambiguous_failure(now);
        assert_eq!(breaker.open_for(now), Some(Duration::from_secs(30)));
        assert_eq!(breaker.open_for(now + Duration::from_secs(30)), None);
    }

    #[test]
    fn a_completion_between_failures_keeps_it_closed() {
        let breaker = breaker();
        let now = Instant::now();
        for _ in 0..10 {
            breaker.record_ambiguous_failure(now);
            breaker.record_ambiguous_failure(now);
            breaker.record_completion();
        }
        assert_eq!(breaker.open_for(now), None);
    }

    #[test]
    fn failing_again_after_the_cooldown_reopens_it_for_longer() {
        let breaker = breaker();
        let mut now = Instant::now();
        for _ in 0..3 {
            breaker.record_ambiguous_failure(now);
        }
        now += Duration::from_secs(30);
        assert_eq!(breaker.open_for(now), None);

        breaker.record_ambiguous_failure(now);
        assert_eq!(breaker.open_for(now), Some(Duration::from_secs(60)));
        now += Duration::from_secs(60);
        breaker.record_ambiguous_failure(now);
        assert_eq!(
            breaker.open_for(now),
            Some(Duration::from_secs(100)),
            "capped"
        );
    }

    #[test]
    fn failures_while_open_do_not_extend_it() {
        let breaker = breaker();
        let now = Instant::now();
        for _ in 0..3 {
            breaker.record_ambiguous_failure(now);
        }
        for _ in 0..5 {
            breaker.record_ambiguous_failure(now + Duration::from_secs(1));
        }
        assert_eq!(breaker.open_for(now), Some(Duration::from_secs(30)));
    }

    #[test]
    fn a_completion_resets_the_cooldown() {
        let breaker = breaker();
        let mut now = Instant::now();
        for _ in 0..3 {
            breaker.record_ambiguous_failure(now);
        }
        now += Duration::from_secs(30);
        breaker.record_ambiguous_failure(now);
        now += Duration::from_secs(60);
        breaker.record_completion();
        for _ in 0..3 {
            breaker.record_ambiguous_failure(now);
        }
        assert_eq!(breaker.open_for(now), Some(Duration::from_secs(30)));
    }
}
