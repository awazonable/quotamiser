//! Opening the ledger, deciding admission, and the helpers the transition and
//! rollover modules share.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::error::{LedgerError, Result};
use crate::hwm::ExternalHwm;
use crate::lock::PoolLock;
use crate::schema;
use crate::state::State;
use crate::trust::{self, Assessment, LedgerMeta, TrustVerdict, UnknownReason};

#[derive(Debug, Clone)]
pub struct LedgerConfig {
    pub ledger_path: PathBuf,
    /// Must live on a different volume from `ledger_path`, so that restoring
    /// one volume from a snapshot cannot roll both back together.
    pub external_hwm_path: PathBuf,
    pub lock_dir: PathBuf,
    pub organization: String,
    pub pools: Vec<String>,
    /// Waives the separate-volume check. For tests only: with both records
    /// on one volume, rollback detection is lost.
    pub allow_same_volume_external_record: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReservationId(pub i64);

#[derive(Debug, Clone)]
pub struct ReservationRequest {
    pub pool_id: String,
    /// The maximum the request can consume. Never a lighter estimate.
    pub liability: u64,
    pub request_digest: String,
    pub model_snapshot: String,
    pub service_tier: String,
    pub accounting_rev: i64,
    pub required_safety_inputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Untrusted(TrustVerdict),
    Latched { scope: String, reason: String },
    SafetyInputMissing(String),
    SafetyInputExpired(String),
    SafetyBudgetExhausted { key: String, remaining: i64 },
    PoolNotOpen { pool_id: String, epoch: i64 },
    InsufficientQuota { remaining: i64, liability: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Reserved(ReservationId),
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolCounters {
    pub granted: i64,
    pub consumed: i64,
    pub reserved: i64,
}

/// A reservation that was sent and is not yet settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDispatch {
    pub id: ReservationId,
    /// `DispatchedWithId` or `DispatchedIdUnknown`.
    pub state: State,
    pub response_id: Option<String>,
    pub created_at: i64,
}

/// Creates the parent directories of the ledger and the external record, and
/// refuses to put both on one volume.
fn prepare_storage_locations(config: &LedgerConfig) -> Result<()> {
    for path in [&config.ledger_path, &config.external_hwm_path] {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
    }
    if config.allow_same_volume_external_record {
        return Ok(());
    }
    match (volume_of(&config.ledger_path), volume_of(&config.external_hwm_path)) {
        (Some(ledger), Some(external)) if ledger != external => Ok(()),
        (Some(_), Some(_)) => Err(LedgerError::StorageContract(
            "the external record is on the same volume as the ledger, so one snapshot restore would roll both back"
                .into(),
        )),
        _ => Err(LedgerError::StorageContract(
            "could not determine which volumes hold the ledger and the external record".into(),
        )),
    }
}

/// Best-effort identity of the volume holding `path`.
fn volume_of(path: &std::path::Path) -> Option<String> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    volume_identity(&std::fs::canonicalize(dir).ok()?)
}

#[cfg(unix)]
fn volume_identity(canonical: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(canonical)
        .ok()
        .map(|m| m.dev().to_string())
}

/// The drive or share prefix of a canonical path.
#[cfg(not(unix))]
fn volume_identity(canonical: &std::path::Path) -> Option<String> {
    match canonical.components().next()? {
        std::path::Component::Prefix(prefix) => {
            Some(prefix.as_os_str().to_string_lossy().to_ascii_uppercase())
        }
        _ => None,
    }
}

pub struct Ledger {
    pub(crate) conn: Connection,
    pub(crate) hwm: ExternalHwm,
    pub(crate) trust: TrustVerdict,
    _locks: Vec<PoolLock>,
}

impl Ledger {
    /// Acquires the pool locks, verifies the storage contract and integrity,
    /// assesses trust, and normalizes reservations left by the previous run.
    pub fn open(config: &LedgerConfig) -> Result<(Ledger, Assessment)> {
        schema::reject_network_path(&config.ledger_path)?;
        prepare_storage_locations(config)?;

        let mut pools = config.pools.clone();
        pools.sort();
        pools.dedup();
        let locks = pools
            .iter()
            .map(|pool| PoolLock::acquire(&config.lock_dir, &config.organization, pool))
            .collect::<Result<Vec<_>>>()?;

        let conn = Connection::open(&config.ledger_path)?;
        schema::configure_and_verify(&conn)?;
        conn.execute_batch(schema::DDL)?;
        conn.execute(
            "INSERT OR IGNORE INTO ledger_meta
               (id, generation, cumulative_liability, current_epoch, clean_shutdown)
             VALUES (1, 0, 0, 0, 0)",
            [],
        )?;

        let mut ledger = Ledger {
            conn,
            hwm: ExternalHwm::new(&config.external_hwm_path),
            trust: TrustVerdict::Unknown(UnknownReason::Uninitialized),
            _locks: locks,
        };
        ledger.check_integrity()?;
        let assessment = ledger.assess_trust()?;
        ledger.normalize_after_restart(&assessment)?;
        ledger.trust = assessment.verdict;
        Ok((ledger, assessment))
    }

    pub fn trust(&self) -> TrustVerdict {
        self.trust
    }

    pub fn meta(&self) -> Result<LedgerMeta> {
        read_meta(&self.conn)
    }

    pub fn current_epoch(&self) -> Result<i64> {
        current_epoch(&self.conn)
    }

    pub fn counters(&self, pool_id: &str, epoch: i64) -> Result<Option<PoolCounters>> {
        pool_counters(&self.conn, pool_id, epoch)
    }

    /// The state shared by every row of the reservation's group.
    pub fn reservation_state(&self, id: ReservationId) -> Result<State> {
        load_group(&self.conn, id.0).map(|(_, _, state)| state)
    }

    /// Every reservation that was sent and is not yet settled, one per group.
    pub fn open_dispatches(&self) -> Result<Vec<OpenDispatch>> {
        let mut stmt = self.conn.prepare(
            "SELECT root_id, state, response_id, created_at FROM reservation
             WHERE id = root_id AND state IN ('DISPATCHED_WITH_ID', 'DISPATCHED_ID_UNKNOWN')
             ORDER BY root_id",
        )?;
        let rows = stmt.query_map([], |r| {
            let state_text: String = r.get(1)?;
            let state = State::parse(&state_text).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    format!("unknown reservation state {state_text}").into(),
                )
            })?;
            Ok(OpenDispatch {
                id: ReservationId(r.get(0)?),
                state,
                response_id: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Marks the shutdown clean. A ledger that fails its integrity check is
    /// never marked clean.
    pub fn close(self) -> Result<()> {
        self.check_integrity()?;
        self.conn
            .execute("UPDATE ledger_meta SET clean_shutdown = 1 WHERE id = 1", [])?;
        Ok(())
    }

    /// Compare-and-reserve. Checks and debits everything the admission
    /// depends on in one immediate transaction; nothing may be sent unless
    /// this returns `Reserved`.
    pub fn reserve(&mut self, request: &ReservationRequest, now: i64) -> Result<Admission> {
        if self.trust != TrustVerdict::Trusted {
            return Ok(Admission::Refused(Refusal::Untrusted(self.trust)));
        }
        let liability = i64::try_from(request.liability).map_err(|_| LedgerError::Overflow)?;
        let mut safety_keys = request.required_safety_inputs.clone();
        safety_keys.sort();
        safety_keys.dedup();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(refusal) = latch_refusal(&tx, &request.pool_id, &request.model_snapshot)? {
            return Ok(Admission::Refused(refusal));
        }

        for key in &safety_keys {
            let row: Option<(i64, i64, i64)> = tx
                .query_row(
                    "SELECT verified_at, ttl_seconds, budget_remaining FROM safety_input WHERE key = ?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((verified_at, ttl, remaining)) = row else {
                return Ok(Admission::Refused(Refusal::SafetyInputMissing(key.clone())));
            };
            let expires = verified_at.checked_add(ttl).ok_or(LedgerError::Overflow)?;
            // A clock that has moved backwards cannot vouch for freshness.
            if now >= expires || now < verified_at {
                return Ok(Admission::Refused(Refusal::SafetyInputExpired(key.clone())));
            }
            if remaining < liability {
                return Ok(Admission::Refused(Refusal::SafetyBudgetExhausted {
                    key: key.clone(),
                    remaining,
                }));
            }
        }

        let epoch = current_epoch(&tx)?;
        let Some(counters) = pool_counters(&tx, &request.pool_id, epoch)? else {
            return Ok(Admission::Refused(Refusal::PoolNotOpen {
                pool_id: request.pool_id.clone(),
                epoch,
            }));
        };
        let remaining = counters
            .granted
            .checked_sub(counters.consumed)
            .and_then(|v| v.checked_sub(counters.reserved))
            .ok_or(LedgerError::Overflow)?;
        if remaining < liability {
            return Ok(Admission::Refused(Refusal::InsufficientQuota {
                remaining,
                liability,
            }));
        }

        adjust_pool(&tx, &request.pool_id, epoch, liability, 0)?;
        for key in &safety_keys {
            tx.execute(
                "UPDATE safety_input SET budget_remaining = budget_remaining - ?1 WHERE key = ?2",
                params![liability, key],
            )?;
        }
        let id: i64 = tx.query_row(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM reservation",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO reservation
               (id, root_id, pool_id, epoch, state, liability, settled, response_id,
                request_digest, model_snapshot, service_tier, accounting_rev, created_at)
             VALUES (?1, ?1, ?2, ?3, 'RESERVED', ?4, NULL, NULL, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                request.pool_id,
                epoch,
                liability,
                request.request_digest,
                request.model_snapshot,
                request.service_tier,
                request.accounting_rev,
                now
            ],
        )?;
        tx.commit()?;
        Ok(Admission::Reserved(ReservationId(id)))
    }

    /// Records a successful verification. Resets both the freshness window
    /// and the liability budget; nothing else restores the budget.
    pub fn revalidate_safety_input(
        &mut self,
        key: &str,
        now: i64,
        ttl_seconds: i64,
        budget: u64,
    ) -> Result<()> {
        let budget = i64::try_from(budget).map_err(|_| LedgerError::Overflow)?;
        self.conn.execute(
            "INSERT INTO safety_input (key, verified_at, ttl_seconds, budget_initial, budget_remaining)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT (key) DO UPDATE SET
               verified_at = excluded.verified_at, ttl_seconds = excluded.ttl_seconds,
               budget_initial = excluded.budget_initial, budget_remaining = excluded.budget_remaining",
            params![key, now, ttl_seconds, budget],
        )?;
        Ok(())
    }

    /// A failed verification removes the input, which refuses admission.
    pub fn invalidate_safety_input(&mut self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM safety_input WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Scopes: `global`, a pool id, or `model:<snapshot>`.
    pub fn set_latch(
        &mut self,
        scope: &str,
        reason: &str,
        evidence: Option<&str>,
        now: i64,
    ) -> Result<()> {
        insert_latch(&self.conn, scope, reason, evidence, now)
    }

    /// Latches never clear on their own, including across a daily reset.
    pub fn clear_latches(&mut self, scope: &str, cleared_by: &str, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE latch SET cleared_at = ?1, cleared_by = ?2 WHERE scope = ?3 AND cleared_at IS NULL",
            params![now, cleared_by, scope],
        )?)
    }

    /// Every group agrees on its state, and each stored `reserved` equals the
    /// liabilities of the active rows it summarises.
    pub fn check_integrity(&self) -> Result<()> {
        let split_groups: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (
               SELECT root_id FROM reservation GROUP BY root_id
               HAVING COUNT(DISTINCT state) > 1 OR COUNT(DISTINCT liability) > 1)",
            [],
            |r| r.get(0),
        )?;
        if split_groups > 0 {
            return Err(LedgerError::Integrity(format!(
                "{split_groups} reservation groups disagree internally"
            )));
        }
        let mismatched: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM pool_epoch pe WHERE pe.reserved <> COALESCE((
                   SELECT SUM(r.liability) FROM reservation r
                   WHERE r.pool_id = pe.pool_id AND r.epoch = pe.epoch AND r.state IN {ACTIVE_STATES_SQL}), 0)"
            ),
            [],
            |r| r.get(0),
        )?;
        if mismatched > 0 {
            return Err(LedgerError::Integrity(format!(
                "{mismatched} pool epochs have a reserved total that does not match their active reservations"
            )));
        }
        Ok(())
    }

    fn assess_trust(&self) -> Result<Assessment> {
        let meta = self.meta()?;
        let external = self.hwm.read().map_err(|_| ());
        let conn = &self.conn;
        let row = |id: i64| -> Option<(State, i64)> {
            conn.query_row(
                "SELECT state, liability FROM reservation WHERE id = ?1",
                [id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|(state, liability)| State::parse(&state).map(|s| (s, liability)))
        };
        Ok(trust::assess(meta, external, row))
    }

    /// Settles what the previous run left behind. Only when the external
    /// record is consistent can a RESERVED row be known never to have been
    /// dispatched; otherwise every non-terminal row is treated as sent.
    fn normalize_after_restart(&mut self, assessment: &Assessment) -> Result<()> {
        let rows_interpretable = matches!(
            assessment.verdict,
            TrustVerdict::Trusted | TrustVerdict::Unknown(UnknownReason::UncleanShutdown)
        );
        let external = match assessment.promote_to_id_unknown {
            Some(_) => self.hwm.read()?,
            None => None,
        };

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let (Some(root), Some(external)) = (assessment.promote_to_id_unknown, external) {
            tx.execute(
                "UPDATE reservation SET state = 'DISPATCHED_ID_UNKNOWN' WHERE root_id = ?1 AND state = 'RESERVED'",
                [root],
            )?;
            tx.execute(
                "UPDATE ledger_meta SET generation = ?1, cumulative_liability = ?2 WHERE id = 1",
                params![external.generation, external.cumulative_liability],
            )?;
        }

        tx.execute(
            "UPDATE reservation SET state = 'DISPATCHED_ID_UNKNOWN' WHERE state = 'DISPATCHING'",
            [],
        )?;

        if rows_interpretable {
            let unsent: Vec<(String, i64, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT pool_id, epoch, liability FROM reservation WHERE state = 'RESERVED'",
                )?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for (pool_id, epoch, liability) in unsent {
                adjust_pool(&tx, &pool_id, epoch, -liability, 0)?;
            }
            tx.execute(
                "UPDATE reservation SET state = 'RELEASED_UNSENT' WHERE state = 'RESERVED'",
                [],
            )?;
        } else {
            tx.execute(
                "UPDATE reservation SET state = 'DISPATCHED_ID_UNKNOWN' WHERE state = 'RESERVED'",
                [],
            )?;
        }

        tx.execute("UPDATE ledger_meta SET clean_shutdown = 0 WHERE id = 1", [])?;
        tx.commit()?;
        Ok(())
    }
}

pub(crate) const ACTIVE_STATES_SQL: &str =
    "('RESERVED','DISPATCHING','DISPATCHED_WITH_ID','DISPATCHED_ID_UNKNOWN')";

#[derive(Debug, Clone)]
pub(crate) struct GroupRow {
    pub root_id: i64,
    pub pool_id: String,
    pub epoch: i64,
    pub state: State,
    pub liability: i64,
    pub response_id: Option<String>,
    pub request_digest: String,
    pub model_snapshot: String,
    pub service_tier: String,
    pub accounting_rev: i64,
    pub created_at: i64,
}

pub(crate) const GROUP_COLUMNS: &str = "root_id, pool_id, epoch, state, liability, response_id, \
     request_digest, model_snapshot, service_tier, accounting_rev, created_at";

pub(crate) fn read_group_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupRow> {
    let state_text: String = row.get(3)?;
    let state = State::parse(&state_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("unknown reservation state {state_text}").into(),
        )
    })?;
    Ok(GroupRow {
        root_id: row.get(0)?,
        pool_id: row.get(1)?,
        epoch: row.get(2)?,
        state,
        liability: row.get(4)?,
        response_id: row.get(5)?,
        request_digest: row.get(6)?,
        model_snapshot: row.get(7)?,
        service_tier: row.get(8)?,
        accounting_rev: row.get(9)?,
        created_at: row.get(10)?,
    })
}

/// Loads every row of the group containing `id` and the state they share.
pub(crate) fn load_group(conn: &Connection, id: i64) -> Result<(i64, Vec<GroupRow>, State)> {
    let root: i64 = conn
        .query_row("SELECT root_id FROM reservation WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or(LedgerError::NotFound(id))?;
    let rows = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {GROUP_COLUMNS} FROM reservation WHERE root_id = ?1 ORDER BY epoch"
        ))?;
        stmt.query_map([root], read_group_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let head = rows.first().ok_or(LedgerError::NotFound(id))?;
    if rows
        .iter()
        .any(|r| r.state != head.state || r.liability != head.liability)
    {
        return Err(LedgerError::Integrity(format!(
            "reservation group {root} has rows that disagree"
        )));
    }
    Ok((root, rows.clone(), head.state))
}

pub(crate) fn read_meta(conn: &Connection) -> Result<LedgerMeta> {
    Ok(conn.query_row(
        "SELECT generation, cumulative_liability, current_epoch, clean_shutdown FROM ledger_meta WHERE id = 1",
        [],
        |r| {
            Ok(LedgerMeta {
                generation: r.get(0)?,
                cumulative_liability: r.get(1)?,
                current_epoch: r.get(2)?,
                clean_shutdown: r.get::<_, i64>(3)? == 1,
            })
        },
    )?)
}

pub(crate) fn current_epoch(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT current_epoch FROM ledger_meta WHERE id = 1",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn pool_counters(
    conn: &Connection,
    pool_id: &str,
    epoch: i64,
) -> Result<Option<PoolCounters>> {
    Ok(conn
        .query_row(
            "SELECT granted, consumed, reserved FROM pool_epoch WHERE pool_id = ?1 AND epoch = ?2",
            params![pool_id, epoch],
            |r| {
                Ok(PoolCounters {
                    granted: r.get(0)?,
                    consumed: r.get(1)?,
                    reserved: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Applies counter deltas with checked arithmetic. A counter that would go
/// negative means the books are already wrong, and is never clamped.
pub(crate) fn adjust_pool(
    conn: &Connection,
    pool_id: &str,
    epoch: i64,
    reserved_delta: i64,
    consumed_delta: i64,
) -> Result<()> {
    let counters = pool_counters(conn, pool_id, epoch)?.ok_or_else(|| {
        LedgerError::Integrity(format!("pool {pool_id} has no row for epoch {epoch}"))
    })?;
    let reserved = counters
        .reserved
        .checked_add(reserved_delta)
        .ok_or(LedgerError::Overflow)?;
    let consumed = counters
        .consumed
        .checked_add(consumed_delta)
        .ok_or(LedgerError::Overflow)?;
    if reserved < 0 || consumed < 0 {
        return Err(LedgerError::Integrity(format!(
            "pool {pool_id} epoch {epoch} counters would go negative"
        )));
    }
    conn.execute(
        "UPDATE pool_epoch SET reserved = ?1, consumed = ?2 WHERE pool_id = ?3 AND epoch = ?4",
        params![reserved, consumed, pool_id, epoch],
    )?;
    Ok(())
}

pub(crate) fn insert_latch(
    conn: &Connection,
    scope: &str,
    reason: &str,
    evidence: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO latch (scope, reason, evidence, set_at) VALUES (?1, ?2, ?3, ?4)",
        params![scope, reason, evidence, now],
    )?;
    Ok(())
}

fn latch_refusal(
    conn: &Connection,
    pool_id: &str,
    model_snapshot: &str,
) -> Result<Option<Refusal>> {
    Ok(conn
        .query_row(
            "SELECT scope, reason FROM latch
             WHERE cleared_at IS NULL AND scope IN ('global', ?1, 'model:' || ?2)
             ORDER BY id LIMIT 1",
            params![pool_id, model_snapshot],
            |r| {
                Ok(Refusal::Latched {
                    scope: r.get(0)?,
                    reason: r.get(1)?,
                })
            },
        )
        .optional()?)
}
