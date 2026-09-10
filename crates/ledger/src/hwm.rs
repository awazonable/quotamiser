//! The external high-water mark, written outside the ledger's rollback domain.
//!
//! Before any dispatch the new generation and cumulative liability are written
//! and fsynced here, and only then committed to the ledger. The external
//! record therefore never trails the ledger, and a ledger restored from an
//! older snapshot shows up as a gap at startup.
//!
//! A checksum detects a torn or corrupted record. It does not detect a
//! rollback of this file; keeping it on a separate volume from the ledger is
//! what makes the two independent.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{LedgerError, Result};

const MAGIC: &str = "quotamiser-hwm v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighWaterMark {
    pub generation: i64,
    pub cumulative_liability: i64,
    /// The reservation whose dispatch produced this generation.
    pub last_reservation: i64,
}

#[derive(Debug, Clone)]
pub struct ExternalHwm {
    path: PathBuf,
}

impl ExternalHwm {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `Ok(None)` when the record has never been written. A record that
    /// exists but cannot be parsed is an integrity error, never `None`.
    pub fn read(&self) -> Result<Option<HighWaterMark>> {
        let mut text = String::new();
        match File::open(&self.path) {
            Ok(mut file) => {
                file.read_to_string(&mut text)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        }
        parse(&text).map(Some)
    }

    pub fn write(&self, hwm: HighWaterMark) -> Result<()> {
        let body = format!(
            "{MAGIC} {} {} {}",
            hwm.generation, hwm.cumulative_liability, hwm.last_reservation
        );
        let line = format!("{body} {:016x}\n", fnv1a(body.as_bytes()));

        let tmp = self.path.with_extension("tmp");
        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(line.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        sync_parent_dir(&self.path)?;
        Ok(())
    }
}

fn parse(text: &str) -> Result<HighWaterMark> {
    let corrupt = |why: &str| LedgerError::Integrity(format!("external high-water mark {why}"));

    let line = text.trim_end_matches('\n');
    let (body, checksum) = line
        .rsplit_once(' ')
        .ok_or_else(|| corrupt("is malformed"))?;
    let expected =
        u64::from_str_radix(checksum, 16).map_err(|_| corrupt("has a bad checksum field"))?;
    if fnv1a(body.as_bytes()) != expected {
        return Err(corrupt("fails its checksum"));
    }
    let fields = body
        .strip_prefix(MAGIC)
        .ok_or_else(|| corrupt("has an unknown format"))?;
    let numbers: Vec<i64> = fields
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| corrupt("has a non-numeric field"))?;
    match numbers.as_slice() {
        [generation, cumulative, last] if *generation >= 0 && *cumulative >= 0 => {
            Ok(HighWaterMark {
                generation: *generation,
                cumulative_liability: *cumulative,
                last_reservation: *last,
            })
        }
        _ => Err(corrupt("has the wrong fields")),
    }
}

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// NTFS journals the rename itself; Windows offers no directory handle to
/// fsync through the standard library.
#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_hwm() -> (tempfile::TempDir, ExternalHwm) {
        let dir = tempfile::tempdir().unwrap();
        let hwm = ExternalHwm::new(dir.path().join("hwm"));
        (dir, hwm)
    }

    #[test]
    fn missing_record_reads_as_none() {
        let (_dir, hwm) = temp_hwm();
        assert_eq!(hwm.read().unwrap(), None);
    }

    #[test]
    fn round_trips() {
        let (_dir, hwm) = temp_hwm();
        let mark = HighWaterMark {
            generation: 7,
            cumulative_liability: 128_000,
            last_reservation: 42,
        };
        hwm.write(mark).unwrap();
        assert_eq!(hwm.read().unwrap(), Some(mark));
    }

    #[test]
    fn a_corrupted_record_is_an_error_not_absence() {
        let (_dir, hwm) = temp_hwm();
        hwm.write(HighWaterMark {
            generation: 1,
            cumulative_liability: 10,
            last_reservation: 1,
        })
        .unwrap();
        let text = fs::read_to_string(hwm.path())
            .unwrap()
            .replace(" 10 ", " 99 ");
        fs::write(hwm.path(), text).unwrap();
        assert!(matches!(hwm.read(), Err(LedgerError::Integrity(_))));
    }
}
