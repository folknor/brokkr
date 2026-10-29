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

pub(crate) use format::library as library_rows;
pub use format::{
    DatasetMatcher, Sides, format_compare_with_limit, format_details, format_single_result,
    format_table,
};
pub use hotpath::hotpath_data_from_json;
pub(crate) use run_store::{Resolved, RunId, RunRef, RunSelector, RunStore};
#[cfg(test)]
pub(crate) use format::fixture_row;
#[cfg(test)]
pub(crate) use types::RunRowBuilder;
pub use types::{
    Distribution, HotpathData, HotpathFunction, HotpathThread, KvKey, KvPair, KvValue, Percentile,
    PreviousRun, Quantity, QueryFilter, RunInvocation, RunRow, StoredRow, Wall,
};
pub(crate) use types::{KvClass, generate_uuid, short_uuid};

/// Handle to the results database.
pub struct ResultsDb {
    conn: rusqlite::Connection,
}

pub(crate) mod paths;
