//! `runs.db` - the piners corpus run store.
//!
//! Persists every `brokkr corpus` run's harness NDJSON - the per-probe
//! disposition lines, each stored whole as the authoritative record with its
//! fields projected as generated columns, and the per-trade `trade_diff`
//! drill-down lines - into the per-project SQLite database returned by
//! `resolve::corpus_runs_db_path`. Once a run
//! is in the DB its artefact dir can be discarded (pass or fail) and its data
//! stays queryable across runs via `brokkr corpus-results` (piners only).
//!
//! Store opening and schema-version migrations use the shared policy in
//! `db::store`; the corpus remains piners-specific. Each ingest transaction
//! keeps the newest `ingest::RUN_RETENTION_LIMIT` run ids and deletes older
//! runs with their detail rows. Writes use single-transaction bulk inserts ([`ingest`]) and
//! parameterized queries plus formatter-on-rows rendering
//! ([`query`]/[`mod@format`]).

pub mod compare;
pub mod format;
pub mod ingest;
mod migrate;
pub mod query;
mod schema;

pub use format::{
    compare_report, dispositions_table, gate_misses_block, raw_records, raw_table, run_header,
    runs_table, runtimes_table, trade_diffs_table, trend_table,
};
pub use ingest::{RunContext, RunRecord};
pub use query::{Shaped, resolve_columns};

/// Handle to the corpus runs database.
pub struct CorpusDb {
    conn: rusqlite::Connection,
}
