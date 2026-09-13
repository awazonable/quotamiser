//! Admission decisions that need no I/O.
//!
//! The ledger decides whether a liability fits; this crate decides when the
//! current day's pool may be spent at all, and how large a request's
//! liability is.

pub mod epoch;
pub mod liability;
