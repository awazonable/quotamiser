//! Table definitions and the storage contract the safety argument rests on.

use std::path::Path;

use rusqlite::Connection;

use crate::error::{LedgerError, Result};

/// STRICT tables need SQLite 3.37.0 or later.
const MIN_SQLITE_VERSION: i32 = 3_037_000;

pub(crate) const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS pool_epoch (
    pool_id   TEXT    NOT NULL,
    epoch     INTEGER NOT NULL,
    granted   INTEGER NOT NULL CHECK (granted  >= 0),
    consumed  INTEGER NOT NULL CHECK (consumed >= 0),
    reserved  INTEGER NOT NULL CHECK (reserved >= 0),
    PRIMARY KEY (pool_id, epoch)
) STRICT;

CREATE TABLE IF NOT EXISTS reservation (
    id             INTEGER PRIMARY KEY,
    root_id        INTEGER NOT NULL,
    pool_id        TEXT    NOT NULL,
    epoch          INTEGER NOT NULL,
    state          TEXT    NOT NULL CHECK (state IN (
                     'RESERVED','DISPATCHING','DISPATCHED_WITH_ID',
                     'DISPATCHED_ID_UNKNOWN','SETTLED',
                     'CONSUMED_UNRECOVERABLE','RELEASED_UNSENT',
                     'REJECTED_BEFORE_PROCESSING')),
    liability      INTEGER NOT NULL CHECK (liability >= 0),
    settled        INTEGER CHECK (settled IS NULL OR settled >= 0),
    response_id    TEXT,
    request_digest TEXT    NOT NULL,
    model_snapshot TEXT    NOT NULL,
    service_tier   TEXT    NOT NULL,
    accounting_rev INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    UNIQUE (root_id, epoch),
    FOREIGN KEY (pool_id, epoch) REFERENCES pool_epoch (pool_id, epoch),
    -- A settled amount exists exactly when the row is settled.
    CHECK ((state = 'SETTLED') = (settled IS NOT NULL)),
    -- The response id is known in DISPATCHED_WITH_ID, may survive into the
    -- terminal states it leads to, and cannot exist before dispatch.
    CHECK (CASE
             WHEN state = 'DISPATCHED_WITH_ID' THEN response_id IS NOT NULL
             WHEN state IN ('SETTLED','CONSUMED_UNRECOVERABLE') THEN 1
             ELSE response_id IS NULL
           END)
) STRICT;

CREATE INDEX IF NOT EXISTS reservation_by_root ON reservation (root_id);

CREATE TABLE IF NOT EXISTS ledger_meta (
    id                   INTEGER PRIMARY KEY CHECK (id = 1),
    generation           INTEGER NOT NULL CHECK (generation >= 0),
    cumulative_liability INTEGER NOT NULL CHECK (cumulative_liability >= 0),
    current_epoch        INTEGER NOT NULL,
    clean_shutdown       INTEGER NOT NULL CHECK (clean_shutdown IN (0, 1))
) STRICT;

CREATE TABLE IF NOT EXISTS safety_input (
    key              TEXT    PRIMARY KEY,
    verified_at      INTEGER NOT NULL,
    ttl_seconds      INTEGER NOT NULL CHECK (ttl_seconds > 0),
    budget_initial   INTEGER NOT NULL CHECK (budget_initial >= 0),
    budget_remaining INTEGER NOT NULL CHECK (budget_remaining >= 0)
) STRICT;

CREATE TABLE IF NOT EXISTS latch (
    id         INTEGER PRIMARY KEY,
    scope      TEXT    NOT NULL,          -- 'global' or a pool_id
    reason     TEXT    NOT NULL,
    evidence   TEXT,
    set_at     INTEGER NOT NULL,
    cleared_at INTEGER,
    cleared_by TEXT,
    CHECK ((cleared_at IS NULL) = (cleared_by IS NULL))
) STRICT;
"#;

/// Applies the durability settings and reads them back. A setting that
/// silently failed to take effect is treated as fatal, because
/// `journal_mode` reports the previous mode rather than an error when WAL
/// cannot be enabled.
pub(crate) fn configure_and_verify(conn: &Connection) -> Result<()> {
    let version = rusqlite::version_number();
    if version < MIN_SQLITE_VERSION {
        return Err(LedgerError::StorageContract(format!(
            "SQLite {} is older than 3.37.0, which STRICT tables require",
            rusqlite::version()
        )));
    }

    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(LedgerError::StorageContract(format!(
            "journal_mode is {mode}, not wal"
        )));
    }

    conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;

    let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    if synchronous != 2 {
        return Err(LedgerError::StorageContract(format!(
            "synchronous is {synchronous}, not FULL"
        )));
    }
    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(LedgerError::StorageContract(
            "foreign_keys could not be enabled".into(),
        ));
    }
    Ok(())
}

/// WAL depends on shared memory that network filesystems do not provide
/// reliably, so a ledger on a network path is refused outright.
pub(crate) fn reject_network_path(path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    let normalized = text.replace('/', "\\");
    let is_network = (normalized.starts_with("\\\\") && !normalized.starts_with("\\\\?\\"))
        || normalized.to_ascii_uppercase().starts_with("\\\\?\\UNC\\");
    if is_network {
        return Err(LedgerError::StorageContract(format!(
            "ledger path {text} is on a network share"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_paths_are_refused() {
        assert!(reject_network_path(Path::new(r"\\server\share\ledger.db")).is_err());
        assert!(reject_network_path(Path::new(r"\\?\UNC\server\share\ledger.db")).is_err());
        assert!(reject_network_path(Path::new(r"\\?\C:\data\ledger.db")).is_ok());
        assert!(reject_network_path(Path::new(r"C:\data\ledger.db")).is_ok());
    }
}
