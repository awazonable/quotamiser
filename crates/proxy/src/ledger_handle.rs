//! Shared, async access to the ledger.
//!
//! Ledger operations fsync, so they run on the blocking pool rather than the
//! async executor. One mutex serializes them, which also keeps the external
//! high-water mark write and the ledger commit of a dispatch from interleaving
//! with another dispatch.

use std::sync::{Arc, Mutex};

use quotamiser_ledger::{Ledger, LedgerError};

#[derive(Clone)]
pub struct LedgerHandle {
    inner: Arc<Mutex<Ledger>>,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerAccessError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("the ledger is unusable: an earlier operation panicked while holding it")]
    Poisoned,
    #[error("the ledger operation did not complete: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl LedgerHandle {
    pub fn new(ledger: Ledger) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ledger)),
        }
    }

    pub async fn with<T, F>(&self, operation: F) -> Result<T, LedgerAccessError>
    where
        F: FnOnce(&mut Ledger) -> quotamiser_ledger::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut ledger = inner.lock().map_err(|_| LedgerAccessError::Poisoned)?;
            operation(&mut ledger).map_err(LedgerAccessError::from)
        })
        .await?
    }
}
