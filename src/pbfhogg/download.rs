//! Download sources, stable input layout and dataset registration.

mod date;
mod layout;
mod modes;
mod registry;
mod snapshot;
mod source;

pub use modes::run;
pub(crate) use snapshot::{preflight_snapshot_collision, promote_snapshot};
