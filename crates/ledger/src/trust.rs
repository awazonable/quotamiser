//! Startup assessment of whether the ledger can be trusted.
//!
//! Agreement between the ledger and the external record is necessary but
//! never sufficient: a snapshot restored before its dispatches reached any
//! external record would agree with everything. So the only positive verdict
//! requires a clean shutdown as well, and every other shape is `Unknown`.

use crate::hwm::HighWaterMark;
use crate::state::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerMeta {
    pub generation: i64,
    pub cumulative_liability: i64,
    pub current_epoch: i64,
    pub clean_shutdown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustVerdict {
    Trusted,
    Unknown(UnknownReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    /// Nothing has ever been dispatched and no external record exists. A
    /// fresh install and a wholly lost ledger look identical from here.
    Uninitialized,
    ExternalMissing,
    ExternalUnreadable,
    /// The external record is older than the ledger, which the write order
    /// makes impossible unless the external record itself was rolled back.
    ExternalBehindLedger,
    /// The external record is ahead by more than one uncommitted dispatch.
    LedgerRolledBack,
    /// Same generation, different totals.
    Inconsistent,
    UncleanShutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    pub verdict: TrustVerdict,
    /// A reservation whose dispatch reached the external record but not the
    /// ledger. It may have been sent, so it must be treated as sent with an
    /// unknown id, and the ledger meta advanced to match the external record.
    pub promote_to_id_unknown: Option<i64>,
}

/// `row` returns the state and liability of a reservation by id.
pub fn assess(
    meta: LedgerMeta,
    external: std::result::Result<Option<HighWaterMark>, ()>,
    row: impl Fn(i64) -> Option<(State, i64)>,
) -> Assessment {
    let unknown = |reason| Assessment {
        verdict: TrustVerdict::Unknown(reason),
        promote_to_id_unknown: None,
    };

    let external = match external {
        Err(()) => return unknown(UnknownReason::ExternalUnreadable),
        Ok(None) if meta.generation == 0 => return unknown(UnknownReason::Uninitialized),
        Ok(None) => return unknown(UnknownReason::ExternalMissing),
        Ok(Some(external)) => external,
    };

    if external.generation < meta.generation
        || external.cumulative_liability < meta.cumulative_liability
    {
        return unknown(UnknownReason::ExternalBehindLedger);
    }

    let mut promote = None;
    match external.generation - meta.generation {
        0 => {
            if external.cumulative_liability != meta.cumulative_liability {
                return unknown(UnknownReason::Inconsistent);
            }
        }
        1 => {
            let gap = external.cumulative_liability - meta.cumulative_liability;
            match row(external.last_reservation) {
                Some((State::Reserved, liability)) if liability == gap => {
                    promote = Some(external.last_reservation);
                }
                _ => return unknown(UnknownReason::LedgerRolledBack),
            }
        }
        _ => return unknown(UnknownReason::LedgerRolledBack),
    }

    // A pending promotion means the process died mid-dispatch, which a clean
    // shutdown marker cannot coexist with.
    if promote.is_some() && meta.clean_shutdown {
        return unknown(UnknownReason::Inconsistent);
    }
    let verdict = if meta.clean_shutdown {
        TrustVerdict::Trusted
    } else {
        TrustVerdict::Unknown(UnknownReason::UncleanShutdown)
    };
    Assessment {
        verdict,
        promote_to_id_unknown: promote,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(generation: i64, cumulative: i64, clean: bool) -> LedgerMeta {
        LedgerMeta {
            generation,
            cumulative_liability: cumulative,
            current_epoch: 0,
            clean_shutdown: clean,
        }
    }
    fn mark(generation: i64, cumulative: i64, last: i64) -> Option<HighWaterMark> {
        Some(HighWaterMark {
            generation,
            cumulative_liability: cumulative,
            last_reservation: last,
        })
    }
    fn no_rows(_: i64) -> Option<(State, i64)> {
        None
    }

    #[test]
    fn clean_and_consistent_is_the_only_trusted_shape() {
        let a = assess(meta(3, 300, true), Ok(mark(3, 300, 9)), no_rows);
        assert_eq!(a.verdict, TrustVerdict::Trusted);
        assert_eq!(a.promote_to_id_unknown, None);
    }

    #[test]
    fn agreement_without_a_clean_shutdown_is_not_trust() {
        let a = assess(meta(3, 300, false), Ok(mark(3, 300, 9)), no_rows);
        assert_eq!(
            a.verdict,
            TrustVerdict::Unknown(UnknownReason::UncleanShutdown)
        );
    }

    #[test]
    fn a_fresh_ledger_is_uninitialized_not_trusted() {
        let a = assess(meta(0, 0, false), Ok(None), no_rows);
        assert_eq!(
            a.verdict,
            TrustVerdict::Unknown(UnknownReason::Uninitialized)
        );
    }

    #[test]
    fn one_uncommitted_dispatch_is_promoted_not_released() {
        let row = |id| (id == 9).then_some((State::Reserved, 50));
        let a = assess(meta(3, 300, false), Ok(mark(4, 350, 9)), row);
        assert_eq!(a.promote_to_id_unknown, Some(9));
        assert_eq!(
            a.verdict,
            TrustVerdict::Unknown(UnknownReason::UncleanShutdown)
        );
    }

    #[test]
    fn a_gap_that_does_not_match_the_named_row_is_rollback() {
        let row = |id| (id == 9).then_some((State::Reserved, 49));
        let a = assess(meta(3, 300, false), Ok(mark(4, 350, 9)), row);
        assert_eq!(
            a.verdict,
            TrustVerdict::Unknown(UnknownReason::LedgerRolledBack)
        );
    }

    #[test]
    fn a_ledger_restored_from_an_older_snapshot_is_detected() {
        let a = assess(meta(3, 300, true), Ok(mark(7, 900, 20)), no_rows);
        assert_eq!(
            a.verdict,
            TrustVerdict::Unknown(UnknownReason::LedgerRolledBack)
        );
    }

    #[test]
    fn an_external_record_behind_the_ledger_is_never_trusted() {
        let a = assess(meta(5, 500, true), Ok(mark(4, 450, 8)), no_rows);
        assert_eq!(
            a.verdict,
            TrustVerdict::Unknown(UnknownReason::ExternalBehindLedger)
        );
    }

    #[test]
    fn a_lost_ledger_with_a_surviving_external_record_is_rollback() {
        let a = assess(meta(0, 0, false), Ok(mark(12, 4_000, 30)), no_rows);
        assert_eq!(
            a.verdict,
            TrustVerdict::Unknown(UnknownReason::LedgerRolledBack)
        );
    }
}
