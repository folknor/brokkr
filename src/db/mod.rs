mod compare;
mod format;
pub mod gate;
mod hotpath;
pub(crate) mod like;
mod migrate;
mod query;
mod run_store;
mod schema;
pub(crate) mod sidecar;
mod types;
mod write;

pub use format::{
    DatasetMatcher, format_compare_with_limit, format_details, format_single_result, format_table,
};
pub use hotpath::hotpath_data_from_json;
pub(crate) use run_store::{Resolved, RunId, RunRef, RunSelector, RunStore};
pub(crate) use types::KvClass;
pub use types::{
    Distribution, DistributionUs, HotpathData, HotpathFunction, HotpathThread, KvPair, KvValue,
    PreviousRun, QueryFilter, RunRow, StoredRow,
};

/// Handle to the results database.
pub struct ResultsDb {
    conn: rusqlite::Connection,
}
