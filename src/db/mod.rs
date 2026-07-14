mod compare;
mod format;
pub mod gate;
mod hotpath;
mod migrate;
mod query;
mod schema;
pub(crate) mod sidecar;
mod types;
mod write;

pub use compare::CompareFilter;
pub use format::{
    DatasetMatcher, format_compare, format_details, format_single_result, format_table,
};
pub use hotpath::hotpath_data_from_json;
pub use types::{
    Distribution, HotpathData, HotpathFunction, HotpathThread, KvPair, KvValue, PreviousRun,
    QueryFilter, RunRow, StoredRow,
};

/// Handle to the results database.
pub struct ResultsDb {
    conn: rusqlite::Connection,
}
