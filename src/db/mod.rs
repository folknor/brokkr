//! The measurement databases: [`ResultsDb`] (`.brokkr/results.db`) and
//! `sidecar::SidecarDb` (`.brokkr/sidecar.db`), linked by result UUID.
//! `store` owns the shared open policy, busy timeout and versioned
//! migration; a read-only query open migrates an older schema first and
//! refuses a newer one. `run_store` is the run identity and selection both
//! stores share. `schema` and `migrate` are results.db's DDL and migration
//! steps; `types`, `write`, `query` and `compare` its rows, inserts and
//! queries; `format` the rendering. `historical_qualifier` recognizes
//! historical typed pbfhogg argv for a results.db migration step.

mod compare;
mod format;
mod hotpath;
pub(crate) mod like;
mod migrate;
mod query;
mod run_store;
mod schema;
pub(crate) mod sidecar;
pub(crate) mod store;
mod types;
mod write;

pub use format::{
    DatasetMatcher, Sides, format_compare_with_limit, format_details, format_single_result,
    format_table,
};
pub use hotpath::hotpath_data_from_json;
pub(crate) use run_store::{Resolved, RunId, RunRef, RunSelector, RunStore};
#[cfg(test)]
pub(crate) use types::{RunRow, RunRowBuilder};
pub use types::{
    Distribution, HotpathData, HotpathFunction, HotpathThread, KvKey, KvPair, KvValue, Percentile,
    Quantity, QueryFilter, RunInvocation, StoredRow, Wall,
};
pub(crate) use types::{KvClass, generate_uuid, short_uuid};

/// Handle to the results database.
pub struct ResultsDb {
    conn: rusqlite::Connection,
}

pub(crate) mod paths;

mod historical_qualifier;
