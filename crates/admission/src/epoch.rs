//! Daily epochs, and the window around each boundary in which admission stops.
//!
//! Upstream resets the free-token counter at 00:00 UTC. Two opposite mistakes
//! are both expensive:
//!
//! - Opening the next day's pool before upstream has reset.
//! - Continuing to spend the previous day's inventory after upstream has
//!   reset. Upstream charges that spending to the new day, and when the new
//!   day is later opened in full, the same quota has been issued twice.
//!
//! So admission closes at the earliest moment the boundary could have
//! passed, and the next epoch is only reported due once trusted time shows
//! the boundary passed beyond all uncertainty plus an opening delay. The
//! decision never trusts the local wall clock: it uses a reading from a
//! source that consumes no quota, advanced by local elapsed time.

pub const SECONDS_PER_DAY: i64 = 86_400;

/// The epoch (UTC day index) containing a unix time.
pub fn epoch_of(unix: i64) -> i64 {
    unix.div_euclid(SECONDS_PER_DAY)
}

/// The unix time at which `epoch` ends and the next begins upstream.
pub fn end_of(epoch: i64) -> i64 {
    (epoch + 1) * SECONDS_PER_DAY
}

/// A time reading from a source that consumes no quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedReading {
    /// UTC unix seconds reported by the source.
    pub upstream_unix: i64,
    /// Local clock reading taken at the same moment. Only the elapsed local
    /// time since then is used, never the local clock's absolute value.
    pub local_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPolicy {
    reading_uncertainty: i64,
    open_delay: i64,
    max_reading_age: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("reading uncertainty must be at least one second")]
    UncertaintyTooSmall,
    #[error("the opening delay must be at least {MIN_OPEN_DELAY} seconds")]
    OpenDelayTooShort,
    #[error("a trusted reading may be at most {MAX_READING_AGE} seconds old")]
    ReadingAgeOutOfRange,
}

/// The positive skew guard. The next day's pool is never opened sooner than
/// this after the upstream reset.
pub const MIN_OPEN_DELAY: i64 = 300;
pub const MAX_READING_AGE: i64 = 3_600;

impl ClockPolicy {
    /// `reading_uncertainty` covers the source's one-second resolution, the
    /// round trip, and local clock drift over `max_reading_age`.
    pub fn new(
        reading_uncertainty: i64,
        open_delay: i64,
        max_reading_age: i64,
    ) -> Result<Self, PolicyError> {
        if reading_uncertainty < 1 {
            return Err(PolicyError::UncertaintyTooSmall);
        }
        if open_delay < MIN_OPEN_DELAY {
            return Err(PolicyError::OpenDelayTooShort);
        }
        if !(1..=MAX_READING_AGE).contains(&max_reading_age) {
            return Err(PolicyError::ReadingAgeOutOfRange);
        }
        Ok(Self {
            reading_uncertainty,
            open_delay,
            max_reading_age,
        })
    }

    pub fn reading_uncertainty(&self) -> i64 {
        self.reading_uncertainty
    }

    pub fn open_delay(&self) -> i64 {
        self.open_delay
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The current epoch's pool may be spent.
    Open,
    /// Nothing may be admitted against any epoch.
    Closed(ClosedReason),
    /// The boundary has passed beyond all uncertainty and the opening delay.
    /// Roll the ledger over to `epoch` before admitting anything.
    RolloverDue { epoch: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedReason {
    /// Within the uncertainty of the boundary, or inside the opening delay.
    NearBoundary,
    /// No reading, or one older than the policy allows.
    NoTrustedTime,
    /// The local clock reads earlier than when the reading was taken, so
    /// elapsed time cannot be measured.
    ClockWentBackwards,
}

/// Decides whether the pool of `current_epoch` may be spent now.
pub fn window(
    policy: &ClockPolicy,
    current_epoch: i64,
    local_now: i64,
    reading: Option<TrustedReading>,
) -> Window {
    let Some(reading) = reading else {
        return Window::Closed(ClosedReason::NoTrustedTime);
    };
    let elapsed = local_now - reading.local_unix;
    if elapsed < 0 {
        return Window::Closed(ClosedReason::ClockWentBackwards);
    }
    if elapsed > policy.max_reading_age {
        return Window::Closed(ClosedReason::NoTrustedTime);
    }

    let estimate = reading.upstream_unix + elapsed;
    let latest_possible = estimate + policy.reading_uncertainty;
    let earliest_possible = estimate - policy.reading_uncertainty;

    if latest_possible < end_of(current_epoch) {
        return Window::Open;
    }
    // The boundary may have passed. Only a boundary that has passed beyond
    // doubt, by at least the opening delay, lets a later epoch open.
    let due = epoch_of(earliest_possible - policy.open_delay);
    if due > current_epoch {
        Window::RolloverDue { epoch: due }
    } else {
        Window::Closed(ClosedReason::NearBoundary)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const DAY: i64 = SECONDS_PER_DAY;
    const D: i64 = 20_342; // an arbitrary day

    fn policy() -> ClockPolicy {
        ClockPolicy::new(5, 300, 600).unwrap()
    }

    /// A reading taken exactly at `true_now` with no error or elapsed time.
    fn at(true_now: i64) -> Option<TrustedReading> {
        Some(TrustedReading {
            upstream_unix: true_now,
            local_unix: 1_000,
        })
    }

    fn decide(true_now: i64) -> Window {
        window(&policy(), D, 1_000, at(true_now))
    }

    #[test]
    fn mid_day_is_open() {
        assert_eq!(decide(D * DAY + 12 * 3_600), Window::Open);
    }

    #[test]
    fn closes_before_the_boundary_by_the_reading_uncertainty() {
        assert_eq!(decide(end_of(D) - 6), Window::Open);
        assert_eq!(
            decide(end_of(D) - 5),
            Window::Closed(ClosedReason::NearBoundary)
        );
    }

    #[test]
    fn stays_closed_through_the_opening_delay() {
        assert_eq!(
            decide(end_of(D) + 120),
            Window::Closed(ClosedReason::NearBoundary)
        );
        assert_eq!(
            decide(end_of(D) + 304),
            Window::Closed(ClosedReason::NearBoundary)
        );
        assert_eq!(
            decide(end_of(D) + 305),
            Window::RolloverDue { epoch: D + 1 }
        );
    }

    #[test]
    fn several_missed_days_roll_over_to_the_latest_opened_day() {
        assert_eq!(
            decide(end_of(D + 2) + 3_600),
            Window::RolloverDue { epoch: D + 3 }
        );
        assert_eq!(
            decide(end_of(D + 2) + 60),
            Window::RolloverDue { epoch: D + 2 }
        );
    }

    #[test]
    fn without_fresh_trusted_time_nothing_is_open() {
        assert_eq!(
            window(&policy(), D, 1_000, None),
            Window::Closed(ClosedReason::NoTrustedTime)
        );
        let stale = TrustedReading {
            upstream_unix: D * DAY + 3_600,
            local_unix: 1_000,
        };
        assert_eq!(
            window(&policy(), D, 1_601, Some(stale)),
            Window::Closed(ClosedReason::NoTrustedTime)
        );
        assert_eq!(
            window(&policy(), D, 999, Some(stale)),
            Window::Closed(ClosedReason::ClockWentBackwards)
        );
    }

    #[test]
    fn unsafe_policies_are_refused() {
        assert_eq!(
            ClockPolicy::new(0, 300, 600),
            Err(PolicyError::UncertaintyTooSmall)
        );
        assert_eq!(
            ClockPolicy::new(5, 299, 600),
            Err(PolicyError::OpenDelayTooShort)
        );
        assert_eq!(
            ClockPolicy::new(5, 300, 0),
            Err(PolicyError::ReadingAgeOutOfRange)
        );
        assert_eq!(
            ClockPolicy::new(5, 300, 3_601),
            Err(PolicyError::ReadingAgeOutOfRange)
        );
    }

    proptest! {
        /// For any true time, any reading error within the stated uncertainty,
        /// and any elapsed time within the allowed age: the old day is never
        /// open once upstream has reset, and a new day is never reported due
        /// before upstream has reached it plus the opening delay.
        #[test]
        fn never_spends_across_the_reset_and_never_opens_early(
            true_now in (D - 1) * DAY..(D + 4) * DAY,
            error in -5i64..=5,
            elapsed in 0i64..=600,
            local_offset in -1_000_000i64..1_000_000,
            current in (D - 1)..(D + 4),
        ) {
            let reading_true = true_now - elapsed;
            let reading = TrustedReading {
                upstream_unix: reading_true + error,
                local_unix: reading_true + local_offset,
            };
            let local_now = true_now + local_offset;
            let decision = window(&policy(), current, local_now, Some(reading));

            if true_now >= end_of(current) {
                prop_assert_ne!(decision, Window::Open);
            }
            if let Window::RolloverDue { epoch } = decision {
                prop_assert!(epoch > current);
                prop_assert!(epoch * DAY + policy().open_delay() <= true_now);
            }
        }
    }
}
