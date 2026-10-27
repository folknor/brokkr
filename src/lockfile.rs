//! Compatibility surface for lock acquisition and lock-record operations.
//!
//! The implementation lives under `lock/`; process identity primitives live
//! under `process/`.

#[path = "lock/mod.rs"]
mod lock;

pub use self::lock::*;
pub(crate) use self::lock::{AcqId, AuthHash, Nonce, RunToken};
pub(crate) use self::lock::{
    CAPABILITY_ENV, GUARD_HANDSHAKE_PREFIX, GUARD_INFO_ARG, GUARD_PROTOCOL, LEASE_MARKER_ENV,
};
pub(crate) use self::lock::{activity, collector, observe, report};
pub(crate) use self::lock::{escape_value, unescape_value};
