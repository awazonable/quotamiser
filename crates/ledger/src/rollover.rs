//! Daily rollover, and adoption of consumption recovered after the ledger
//! could not be trusted.

use rusqlite::{TransactionBehavior, params};

use crate::error::{LedgerError, Result};
use crate::hwm::HighWaterMark;
use crate::ledger::{
    ACTIVE_STATES_SQL, GROUP_COLUMNS, Ledger, adjust_pool, current_epoch, pool_counters,
    read_group_row, read_meta,
};
use crate::trust::TrustVerdict;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloverOutcome {
    AlreadyCurrent,
    Advanced { carried: usize },
}

impl Ledger {
    /// Opens `new_epoch` and carries every active reservation into it, in one
    /// transaction. Monotonic: an epoch at or before the current one changes
    /// nothing, so re-running after a crash cannot duplicate a liability.
    ///
    /// Inserting a shadow row is not enough on its own; the new epoch's
    /// `reserved` is raised by exactly the rows inserted here, or the next
    /// admission would read zero and hand out the full grant on top of a live
    /// liability.
    ///
    /// The caller must hold trusted time for the new epoch; the ledger cannot
    /// check that. `grants` must cover every pool with active reservations.
    pub fn rollover(
        &mut self,
        new_epoch: i64,
        grants: &[(String, u64)],
    ) -> Result<RolloverOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if new_epoch <= current_epoch(&tx)? {
            return Ok(RolloverOutcome::AlreadyCurrent);
        }

        for (pool_id, granted) in grants {
            let granted = i64::try_from(*granted).map_err(|_| LedgerError::Overflow)?;
            tx.execute(
                "INSERT INTO pool_epoch (pool_id, epoch, granted, consumed, reserved)
                 VALUES (?1, ?2, ?3, 0, 0) ON CONFLICT (pool_id, epoch) DO NOTHING",
                params![pool_id, new_epoch, granted],
            )?;
        }

        // The newest row of each active group not yet present in the new epoch.
        let carry = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {GROUP_COLUMNS} FROM reservation r
                 WHERE r.state IN {ACTIVE_STATES_SQL}
                   AND r.epoch = (SELECT MAX(x.epoch) FROM reservation x WHERE x.root_id = r.root_id)
                   AND NOT EXISTS (SELECT 1 FROM reservation y WHERE y.root_id = r.root_id AND y.epoch = ?1)"
            ))?;
            stmt.query_map([new_epoch], read_group_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut carried = 0;
        for row in &carry {
            if pool_counters(&tx, &row.pool_id, new_epoch)?.is_none() {
                return Err(LedgerError::Integrity(format!(
                    "no grant supplied for pool {} which has active reservations",
                    row.pool_id
                )));
            }
            let inserted = tx.execute(
                "INSERT INTO reservation
                   (id, root_id, pool_id, epoch, state, liability, settled, response_id,
                    request_digest, model_snapshot, service_tier, accounting_rev, created_at)
                 VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM reservation),
                         ?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (root_id, epoch) DO NOTHING",
                params![
                    row.root_id,
                    row.pool_id,
                    new_epoch,
                    row.state.as_str(),
                    row.liability,
                    row.response_id,
                    row.request_digest,
                    row.model_snapshot,
                    row.service_tier,
                    row.accounting_rev,
                    row.created_at
                ],
            )?;
            if inserted == 1 {
                adjust_pool(&tx, &row.pool_id, new_epoch, row.liability, 0)?;
                carried += 1;
            }
        }

        tx.execute(
            "UPDATE ledger_meta SET current_epoch = ?1 WHERE id = 1",
            [new_epoch],
        )?;
        tx.commit()?;
        Ok(RolloverOutcome::Advanced { carried })
    }

    /// Adopts per-pool consumption recovered from the provider's usage
    /// reporting. The caller is responsible for having obtained two agreeing
    /// readings far enough apart. Recorded consumption only ever rises.
    ///
    /// This is the one deliberate exception to "usage reporting never reopens
    /// capacity": it marks an untrusted ledger trusted. A ledger recovered
    /// this way carries no fail-closed guarantee, because reporting lag is
    /// absorbed rather than excluded.
    pub fn adopt_recovered_consumption(&mut self, adoptions: &[(String, i64, u64)]) -> Result<()> {
        let meta = read_meta(&self.conn)?;
        let external = self.hwm.read().ok().flatten();
        let anchor = HighWaterMark {
            generation: meta
                .generation
                .max(external.map_or(0, |e| e.generation))
                .checked_add(1)
                .ok_or(LedgerError::Overflow)?,
            cumulative_liability: meta
                .cumulative_liability
                .max(external.map_or(0, |e| e.cumulative_liability)),
            last_reservation: 0,
        };
        // Re-anchor externally first, preserving external >= ledger.
        self.hwm.write(anchor)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (pool_id, epoch, consumed) in adoptions {
            let consumed = i64::try_from(*consumed).map_err(|_| LedgerError::Overflow)?;
            let counters = pool_counters(&tx, pool_id, *epoch)?.ok_or_else(|| {
                LedgerError::Integrity(format!(
                    "cannot adopt consumption for {pool_id} epoch {epoch}: not open"
                ))
            })?;
            if consumed > counters.consumed {
                adjust_pool(&tx, pool_id, *epoch, 0, consumed - counters.consumed)?;
            }
        }
        tx.execute(
            "UPDATE ledger_meta SET generation = ?1, cumulative_liability = ?2 WHERE id = 1",
            params![anchor.generation, anchor.cumulative_liability],
        )?;
        tx.commit()?;
        self.trust = TrustVerdict::Trusted;
        Ok(())
    }
}
