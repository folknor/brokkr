//! `runs.db` - the piners corpus run store.
//!
//! Persists every `brokkr corpus` run's harness NDJSON - the per-probe
//! disposition lines, each stored whole as the authoritative record with its
//! fields projected as generated columns, and the per-trade `trade_diff`
//! drill-down lines - into a per-project SQLite database at
//! `.brokkr/piners/corpus/runs.db`. Once a run
//! is in the DB its artefact dir can be discarded (pass or fail) and its data
//! stays queryable across runs via `brokkr corpus-results` (piners only).
//!
//! Deliberately mirrors the `src/db` `ResultsDb` patterns - WAL, per-db
//! `PRAGMA user_version` migrations ([`migrate`]), single-transaction bulk
//! insert ([`ingest`]), parameterized queries + formatter-on-rows rendering
//! ([`query`]/[`mod@format`]) - but is piners-specific and append-only: it never
//! deletes, so it carries no FK-cascade machinery.

pub mod format;
pub mod ingest;
mod migrate;
pub mod query;
mod schema;

pub use format::{
    dispositions_table, gate_misses_block, raw_records, raw_table, runs_table, runtimes_table,
    trade_diffs_table, trend_table,
};
pub use ingest::RunRecord;
pub use query::{Shaped, resolve_columns};

/// Handle to the corpus runs database.
pub struct CorpusDb {
    conn: rusqlite::Connection,
}
