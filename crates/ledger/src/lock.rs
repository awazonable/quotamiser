//! Single-writer lock over the resource whose grant is shared.
//!
//! The lock is keyed on organization and pool, not on the ledger file or the
//! credential. Two credentials in one organization draw on the same pool, so
//! keying on either of those would let two processes each see a full grant.
//!
//! Ownership is the kernel's: the lock is held through an open file handle
//! and released when the process exits for any reason, so a crash cannot
//! leave a stale lock that blocks recovery. The file's contents are
//! diagnostic only.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{LedgerError, Result};
use crate::hwm::fnv1a;

#[derive(Debug)]
pub struct PoolLock {
    _file: File,
    path: PathBuf,
}

impl PoolLock {
    pub fn path_for(lock_dir: &Path, organization: &str, pool_id: &str) -> PathBuf {
        let identity = format!("{organization}\u{0}{pool_id}");
        let readable: String = format!("{organization}__{pool_id}")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        lock_dir.join(format!(
            "{readable}.{:016x}.lock",
            fnv1a(identity.as_bytes())
        ))
    }

    pub fn acquire(lock_dir: &Path, organization: &str, pool_id: &str) -> Result<Self> {
        fs::create_dir_all(lock_dir)?;
        let path = Self::path_for(lock_dir, organization, pool_id);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(LedgerError::Locked),
            Err(fs::TryLockError::Error(err)) => return Err(err.into()),
        }
        // Diagnostic metadata, written only once the lock is ours.
        file.set_len(0)?;
        writeln!(file, "pid {}", std::process::id())?;
        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let first = PoolLock::acquire(dir.path(), "org-a", "openai:large").unwrap();
        assert!(matches!(
            PoolLock::acquire(dir.path(), "org-a", "openai:large"),
            Err(LedgerError::Locked)
        ));
        drop(first);
        PoolLock::acquire(dir.path(), "org-a", "openai:large").unwrap();
    }

    #[test]
    fn different_pools_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let _large = PoolLock::acquire(dir.path(), "org-a", "openai:large").unwrap();
        let _small = PoolLock::acquire(dir.path(), "org-a", "openai:small").unwrap();
    }

    #[test]
    fn identity_is_not_confused_by_sanitisation() {
        let dir = Path::new("locks");
        assert_ne!(
            PoolLock::path_for(dir, "org", "a:b"),
            PoolLock::path_for(dir, "org", "a_b")
        );
    }
}
