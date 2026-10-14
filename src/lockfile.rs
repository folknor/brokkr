//! Compatibility surface for lock acquisition and lock-record operations.
//!
//! The implementation lives under `lock/`; process identity primitives live
//! under `process/`.

#[path = "lock/mod.rs"]
mod lock;

pub use self::lock::*;
pub(crate) use self::lock::{AcqId, AuthHash, Nonce, RunToken};
pub(crate) use self::lock::{escape_value, unescape_value};
