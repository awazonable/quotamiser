use crate::state::State;

/// Failures of the ledger itself. An admission refusal is not an error; see
/// [`crate::Refusal`].
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The storage configuration the safety argument depends on is not in
    /// effect, for example WAL could not be enabled.
    #[error("storage contract not met: {0}")]
    StorageContract(String),

    #[error("arithmetic overflow in ledger accounting")]
    Overflow,

    #[error("reservation {0} not found")]
    NotFound(i64),

    #[error("illegal transition for reservation group {root_id}: {from:?} does not allow {to:?}")]
    IllegalTransition {
        root_id: i64,
        from: State,
        to: State,
    },

    /// Stored counters disagree with the rows they summarise, or a row is in a
    /// shape the schema should have prevented.
    #[error("integrity violation: {0}")]
    Integrity(String),

    #[error("another process holds the ledger lock for this pool")]
    Locked,
}

pub type Result<T> = std::result::Result<T, LedgerError>;
