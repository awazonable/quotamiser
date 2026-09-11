//! State transitions. Each applies to a whole reservation group in one
//! immediate transaction and states exactly what it does to the counters.

use rusqlite::{Connection, TransactionBehavior, params};

use crate::error::{LedgerError, Result};
use crate::hwm::HighWaterMark;
use crate::ledger::{
    GroupRow, Ledger, ReservationId, adjust_pool, insert_latch, load_group, read_meta,
};
use crate::state::{State, Transition};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// What the provider reported for a finished response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedResponse {
    /// `None` when the response carried no usable usage object.
    pub usage: Option<Usage>,
    pub model: String,
    pub service_tier: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageDefect {
    Missing,
    Negative,
    Overflow,
    ModelMismatch,
    ServiceTierMismatch,
    AccountingRevisionMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    Settled {
        actual: i64,
        overrun: bool,
    },
    /// The usage could not be trusted: the full liability was consumed and
    /// the pool latched in the same transaction.
    WrittenOff(UsageDefect),
}

impl Ledger {
    /// Durably records that dispatch is about to begin. Must complete before
    /// the first operation that can hand bytes to the HTTP stack.
    ///
    /// The external high-water mark is written and fsynced before the ledger
    /// commit, so the external record never trails the ledger. If this
    /// returns an error the request must not be sent.
    pub fn begin_dispatch(&mut self, id: ReservationId) -> Result<()> {
        let (root, rows, state) = load_group(&self.conn, id.0)?;
        ensure_allowed(root, state, Transition::BeginDispatch)?;
        let meta = read_meta(&self.conn)?;
        let next = HighWaterMark {
            generation: meta
                .generation
                .checked_add(1)
                .ok_or(LedgerError::Overflow)?,
            cumulative_liability: meta
                .cumulative_liability
                .checked_add(rows[0].liability)
                .ok_or(LedgerError::Overflow)?,
            last_reservation: root,
        };
        self.hwm.write(next)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expect_rows(
            tx.execute("UPDATE reservation SET state = 'DISPATCHING' WHERE root_id = ?1 AND state = 'RESERVED'", [root])?,
            rows.len(),
            root,
        )?;
        expect_rows(
            tx.execute(
                "UPDATE ledger_meta SET generation = ?1, cumulative_liability = ?2 WHERE id = 1 AND generation = ?3",
                params![next.generation, next.cumulative_liability, meta.generation],
            )?,
            1,
            root,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn attach_response_id(&mut self, id: ReservationId, response_id: &str) -> Result<()> {
        self.relabel(id, Transition::AttachResponseId, Some(response_id))
    }

    pub fn mark_id_unknown(&mut self, id: ReservationId) -> Result<()> {
        self.relabel(id, Transition::MarkIdUnknown, None)
    }

    /// Settles against authoritative usage. Usage that fails validation never
    /// releases liability: the group is written off in full and latched.
    /// Usage above the liability is recorded as reported, not clamped, and
    /// latches the model.
    pub fn settle(
        &mut self,
        id: ReservationId,
        response: &CompletedResponse,
        accounting_rev: i64,
        now: i64,
    ) -> Result<Settlement> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (root, rows, state) = load_group(&tx, id.0)?;
        ensure_allowed(root, state, Transition::Settle)?;
        let head = &rows[0];

        let outcome = match validate(head, response, accounting_rev) {
            Err(defect) => {
                finish(
                    &tx,
                    root,
                    &rows,
                    state,
                    State::ConsumedUnrecoverable,
                    head.liability,
                    None,
                )?;
                insert_latch(
                    &tx,
                    &head.pool_id,
                    &format!("invalid usage: {defect:?}"),
                    Some(&format!("reservation {root}")),
                    now,
                )?;
                Settlement::WrittenOff(defect)
            }
            Ok(actual) => {
                let overrun = actual > head.liability;
                finish(
                    &tx,
                    root,
                    &rows,
                    state,
                    State::Settled,
                    actual,
                    Some(actual),
                )?;
                if overrun {
                    insert_latch(
                        &tx,
                        &format!("model:{}", head.model_snapshot),
                        "reported usage exceeded the reserved liability",
                        Some(&format!(
                            "reservation {root}: actual {actual} > liability {}",
                            head.liability
                        )),
                        now,
                    )?;
                }
                Settlement::Settled { actual, overrun }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Gives up on recovering authoritative usage: the full liability becomes
    /// consumption.
    pub fn write_off(&mut self, id: ReservationId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (root, rows, state) = load_group(&tx, id.0)?;
        ensure_allowed(root, state, Transition::Unrecoverable)?;
        let liability = rows[0].liability;
        finish(
            &tx,
            root,
            &rows,
            state,
            State::ConsumedUnrecoverable,
            liability,
            None,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Only for a request proven never to have reached the HTTP stack.
    /// Safety-input budgets are not refunded.
    pub fn release_unsent(&mut self, id: ReservationId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (root, rows, state) = load_group(&tx, id.0)?;
        ensure_allowed(root, state, Transition::ReleaseUnsent)?;
        finish(&tx, root, &rows, state, State::ReleasedUnsent, 0, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Only for a synchronous refusal of the create request itself, with a
    /// status proving that processing never started: 400, 401, 402, 403, 404,
    /// 422 or 429 (ADR-0006). The request did reach upstream, so this is not
    /// `release_unsent`, but no response exists that could consume anything.
    /// Safety-input budgets are not refunded.
    pub fn release_rejected(&mut self, id: ReservationId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (root, rows, state) = load_group(&tx, id.0)?;
        ensure_allowed(root, state, Transition::RejectBeforeProcessing)?;
        finish(
            &tx,
            root,
            &rows,
            state,
            State::RejectedBeforeProcessing,
            0,
            None,
        )?;
        tx.commit()?;
        Ok(())
    }

    fn relabel(
        &mut self,
        id: ReservationId,
        transition: Transition,
        response_id: Option<&str>,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (root, rows, state) = load_group(&tx, id.0)?;
        ensure_allowed(root, state, transition)?;
        expect_rows(
            tx.execute(
                "UPDATE reservation SET state = ?1, response_id = COALESCE(?2, response_id)
                 WHERE root_id = ?3 AND state = ?4",
                params![
                    transition.target().as_str(),
                    response_id,
                    root,
                    state.as_str()
                ],
            )?,
            rows.len(),
            root,
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn ensure_allowed(root_id: i64, from: State, transition: Transition) -> Result<()> {
    if transition.allowed_from(from) {
        Ok(())
    } else {
        Err(LedgerError::IllegalTransition {
            root_id,
            from,
            to: transition.target(),
        })
    }
}

/// Removes each row's liability from `reserved` and adds `consumed_per_row`
/// to `consumed` in that row's epoch, then relabels the group.
fn finish(
    conn: &Connection,
    root: i64,
    rows: &[GroupRow],
    from: State,
    target: State,
    consumed_per_row: i64,
    settled: Option<i64>,
) -> Result<()> {
    for row in rows {
        adjust_pool(
            conn,
            &row.pool_id,
            row.epoch,
            -row.liability,
            consumed_per_row,
        )?;
    }
    expect_rows(
        conn.execute(
            "UPDATE reservation SET state = ?1, settled = ?2 WHERE root_id = ?3 AND state = ?4",
            params![target.as_str(), settled, root, from.as_str()],
        )?,
        rows.len(),
        root,
    )
}

fn validate(
    head: &GroupRow,
    response: &CompletedResponse,
    accounting_rev: i64,
) -> std::result::Result<i64, UsageDefect> {
    let usage = response.usage.ok_or(UsageDefect::Missing)?;
    if usage.input_tokens < 0 || usage.output_tokens < 0 {
        return Err(UsageDefect::Negative);
    }
    if response.model != head.model_snapshot {
        return Err(UsageDefect::ModelMismatch);
    }
    if response.service_tier != head.service_tier {
        return Err(UsageDefect::ServiceTierMismatch);
    }
    if accounting_rev != head.accounting_rev {
        return Err(UsageDefect::AccountingRevisionMismatch);
    }
    // Pool consumption. output_tokens already includes reasoning tokens, and
    // cached tokens are a breakdown of input_tokens, not an addition.
    usage
        .input_tokens
        .checked_add(usage.output_tokens)
        .ok_or(UsageDefect::Overflow)
}

fn expect_rows(changed: usize, expected: usize, root: i64) -> Result<()> {
    if changed == expected {
        Ok(())
    } else {
        Err(LedgerError::Integrity(format!(
            "reservation group {root}: expected to update {expected} rows, updated {changed}"
        )))
    }
}
