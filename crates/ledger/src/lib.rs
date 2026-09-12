//! The QuotaMiser reservation ledger.
//!
//! This crate holds the one property whose violation costs money: for every
//! pool and epoch, settled consumption plus the maximum possible consumption
//! of every request that may have reached upstream never exceeds the grant.
//!
//! It has no network access by construction. It cannot send a request; it can
//! only decide whether one may be sent and record what happened.

mod error;
mod hwm;
mod ledger;
mod lock;
mod requests;
mod rollover;
mod schema;
mod state;
mod transitions;
mod trust;

#[cfg(test)]
mod model_tests;

pub use error::{LedgerError, Result};
pub use hwm::{ExternalHwm, HighWaterMark};
pub use ledger::{
    Admission, Ledger, LedgerConfig, OpenDispatch, PoolCounters, Refusal, ReservationId,
    ReservationRequest,
};
pub use lock::PoolLock;
pub use requests::{RequestSlot, RequestWindow};
pub use rollover::RolloverOutcome;
pub use state::{State, Transition};
pub use transitions::{CompletedResponse, Settlement, Usage, UsageDefect};
pub use trust::{Assessment, LedgerMeta, TrustVerdict, UnknownReason};
