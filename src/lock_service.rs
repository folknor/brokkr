//! Compatibility re-exports for the lock control service.

#[path = "lock/service.rs"]
mod service;

pub use self::service::*;
