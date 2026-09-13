//! Counting requests, for a provider whose free allowance is denominated in
//! them rather than in tokens.
//!
//! This is deliberately not the reservation ledger's state machine. There,
//! a liability is reserved, released when nothing was sent, and settled from
//! reported usage. Here there is nothing to settle and nothing to release: a
//! request that fails still spends the day's allowance, so the slot is taken
//! before sending and never given back. Being conservative in the same
//! direction as the provider is what keeps the count from running under.
//!
//! Both limits are checked and recorded in one immediate transaction: the
//! day's total, and a short sliding window. Because the window's timestamps
//! are durable, a restart cannot forget requests it made a moment ago and walk
//! straight into the provider's rate limit.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::error::{LedgerError, Result};
use crate::ledger::Ledger;

/// The short rate limit a provider enforces alongside the daily one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestWindow {
    pub max_in_window: u32,
    pub window_seconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestSlot {
    /// The request may be sent. The slot is already spent.
    Granted { used: i64, limit: i64 },
    /// The day's allowance is gone. It returns at the next epoch.
    Exhausted { used: i64, limit: i64 },
    /// The short window is full. Nothing was spent.
    WindowFull { retry_after_seconds: i64 },
}

impl Ledger {
    /// Takes one request slot, durably, before anything is sent.
    ///
    /// `now` is trusted time. The slot is spent whatever the request's
    /// outcome; there is no path that returns it.
    pub fn take_request_slot(
        &mut self,
        provider: &str,
        epoch: i64,
        limit_per_day: u64,
        window: RequestWindow,
        now: i64,
    ) -> Result<RequestSlot> {
        let limit = i64::try_from(limit_per_day).map_err(|_| LedgerError::Overflow)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        tx.execute(
            "INSERT INTO request_counter (provider, epoch, limit_per_day, used)
             VALUES (?1, ?2, ?3, 0)
             ON CONFLICT (provider, epoch) DO UPDATE SET limit_per_day = excluded.limit_per_day",
            params![provider, epoch, limit],
        )?;
        let used: i64 = tx.query_row(
            "SELECT used FROM request_counter WHERE provider = ?1 AND epoch = ?2",
            params![provider, epoch],
            |row| row.get(0),
        )?;
        if used >= limit {
            tx.commit()?;
            return Ok(RequestSlot::Exhausted { used, limit });
        }

        // Nothing older than the window can hold a request back.
        let window_start = now
            .checked_sub(window.window_seconds)
            .ok_or(LedgerError::Overflow)?;
        tx.execute(
            "DELETE FROM request_dispatch WHERE provider = ?1 AND at < ?2",
            params![provider, window_start],
        )?;
        let in_window: i64 = tx.query_row(
            "SELECT COUNT(*) FROM request_dispatch WHERE provider = ?1 AND at >= ?2",
            params![provider, window_start],
            |row| row.get(0),
        )?;
        if in_window >= i64::from(window.max_in_window) {
            let oldest: Option<i64> = tx
                .query_row(
                    "SELECT MIN(at) FROM request_dispatch WHERE provider = ?1 AND at >= ?2",
                    params![provider, window_start],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            tx.commit()?;
            // The window frees a slot once its oldest entry falls out of it.
            let retry_after_seconds = oldest
                .map(|oldest| (oldest + window.window_seconds - now).max(1))
                .unwrap_or(1);
            return Ok(RequestSlot::WindowFull {
                retry_after_seconds,
            });
        }

        tx.execute(
            "UPDATE request_counter SET used = used + 1 WHERE provider = ?1 AND epoch = ?2",
            params![provider, epoch],
        )?;
        tx.execute(
            "INSERT INTO request_dispatch (provider, at) VALUES (?1, ?2)",
            params![provider, now],
        )?;
        tx.commit()?;
        Ok(RequestSlot::Granted {
            used: used + 1,
            limit,
        })
    }

    /// What the day's count stands at, if the provider has been used at all.
    pub fn request_counts(&self, provider: &str, epoch: i64) -> Result<Option<(i64, i64)>> {
        let row = self
            .conn
            .query_row(
                "SELECT used, limit_per_day FROM request_counter WHERE provider = ?1 AND epoch = ?2",
                params![provider, epoch],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Requests sent within the window ending now. For reporting; the gate
    /// itself is the transaction above.
    pub fn requests_in_window(&self, provider: &str, since: i64) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM request_dispatch WHERE provider = ?1 AND at >= ?2",
            params![provider, since],
            |row| row.get(0),
        )?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ledger::{Ledger, LedgerConfig};

    const PROVIDER: &str = "openrouter:free";
    const WINDOW: RequestWindow = RequestWindow {
        max_in_window: 3,
        window_seconds: 60,
    };

    fn ledger(dir: &Path) -> Ledger {
        let config = LedgerConfig {
            ledger_path: dir.join("ledger.db"),
            external_hwm_path: dir.join("external").join("hwm"),
            lock_dir: dir.join("locks"),
            organization: "org-test".into(),
            pools: vec!["openai:small".into()],
            allow_same_volume_external_record: true,
        };
        Ledger::open(&config).unwrap().0
    }

    #[test]
    fn slots_are_taken_until_the_day_is_spent() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(dir.path());
        // Far apart in time, so only the daily limit can bite.
        for expected in 1..=5 {
            let slot = ledger
                .take_request_slot(PROVIDER, 1, 5, WINDOW, expected * 1_000)
                .unwrap();
            assert_eq!(
                slot,
                RequestSlot::Granted {
                    used: expected,
                    limit: 5
                }
            );
        }
        assert_eq!(
            ledger
                .take_request_slot(PROVIDER, 1, 5, WINDOW, 9_000)
                .unwrap(),
            RequestSlot::Exhausted { used: 5, limit: 5 }
        );
        assert_eq!(ledger.request_counts(PROVIDER, 1).unwrap(), Some((5, 5)));
    }

    #[test]
    fn a_failed_request_still_spends_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(dir.path());
        ledger
            .take_request_slot(PROVIDER, 1, 50, WINDOW, 1_000)
            .unwrap();
        // Whatever the outcome, there is no call that gives the slot back.
        assert_eq!(ledger.request_counts(PROVIDER, 1).unwrap(), Some((1, 50)));
    }

    #[test]
    fn the_short_window_holds_requests_back_without_spending_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(dir.path());
        for _ in 0..WINDOW.max_in_window {
            assert!(matches!(
                ledger
                    .take_request_slot(PROVIDER, 1, 50, WINDOW, 1_000)
                    .unwrap(),
                RequestSlot::Granted { .. }
            ));
        }
        let held = ledger
            .take_request_slot(PROVIDER, 1, 50, WINDOW, 1_010)
            .unwrap();
        assert_eq!(
            held,
            RequestSlot::WindowFull {
                retry_after_seconds: 50
            },
            "it waits for the oldest entry to leave the window"
        );
        assert_eq!(
            ledger.request_counts(PROVIDER, 1).unwrap(),
            Some((3, 50)),
            "being held back spends nothing"
        );

        // Once the window has passed, sending resumes.
        assert!(matches!(
            ledger
                .take_request_slot(PROVIDER, 1, 50, WINDOW, 1_061)
                .unwrap(),
            RequestSlot::Granted { used: 4, .. }
        ));
    }

    #[test]
    fn the_window_survives_reopening_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut ledger = ledger(dir.path());
            for _ in 0..WINDOW.max_in_window {
                ledger
                    .take_request_slot(PROVIDER, 1, 50, WINDOW, 1_000)
                    .unwrap();
            }
            ledger.close().unwrap();
        }
        let mut reopened = ledger(dir.path());
        assert_eq!(
            reopened
                .take_request_slot(PROVIDER, 1, 50, WINDOW, 1_005)
                .unwrap(),
            RequestSlot::WindowFull {
                retry_after_seconds: 55
            },
            "a restart does not forget what it just sent"
        );
    }

    #[test]
    fn each_epoch_counts_separately() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(dir.path());
        ledger
            .take_request_slot(PROVIDER, 1, 1, WINDOW, 1_000)
            .unwrap();
        assert_eq!(
            ledger
                .take_request_slot(PROVIDER, 1, 1, WINDOW, 2_000)
                .unwrap(),
            RequestSlot::Exhausted { used: 1, limit: 1 }
        );
        assert_eq!(
            ledger
                .take_request_slot(PROVIDER, 2, 1, WINDOW, 90_000)
                .unwrap(),
            RequestSlot::Granted { used: 1, limit: 1 },
            "the next day starts at zero"
        );
    }

    #[test]
    fn old_window_entries_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(dir.path());
        for at in [1_000, 1_010, 1_020] {
            ledger
                .take_request_slot(PROVIDER, 1, 50, WINDOW, at)
                .unwrap();
        }
        ledger
            .take_request_slot(PROVIDER, 1, 50, WINDOW, 2_000)
            .unwrap();
        assert_eq!(
            ledger.requests_in_window(PROVIDER, 0).unwrap(),
            1,
            "entries older than the window are removed"
        );
    }
}
