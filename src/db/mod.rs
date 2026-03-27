mod compare;
mod format;
mod hotpath;
mod migrate;
mod query;
mod schema;
mod types;
mod write;

pub use format::{format_compare, format_details, format_table};
pub use hotpath::hotpath_data_from_json;
pub use types::{
    CompareResult, Distribution, HotpathData, HotpathFunction, HotpathThread, KvPair, KvValue,
    QueryFilter, RunRow, StoredRow,
};

/// Handle to the results database.
pub struct ResultsDb {
    conn: rusqlite::Connection,
}
