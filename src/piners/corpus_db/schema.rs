//! Schema DDL and connection lifecycle for the corpus runs database.
//!
//! Mirrors `src/db/schema.rs`. The bundled SQLite enforces the FK
//! `REFERENCES` clauses (ingest writes the `run` row before its children, so
//! they hold), and the DB is append-only, so there is no cascade-delete path.
//!
//! The `disposition` table stores each harness line whole, as `raw_json`, and
//! every harness field is a generated column projected from it. The harness
//! adds diagnostics faster than brokkr learns their names, and the run dir
//! holding the NDJSON is deleted after ingest, so a field brokkr had no column
//! for used to be destroyed on arrival. With the record authoritative a new
//! field is stored the day it ships. Naming it is a [`PROJECTIONS`] entry,
//! which reaches fresh stores, plus a migration step for existing ones -
//! `ALTER TABLE disposition ADD COLUMN ... GENERATED ALWAYS AS (...) VIRTUAL`
//! behind a `has_column` guard, because `CREATE TABLE IF NOT EXISTS` never
//! touches a table that already exists. That step reaches every row already
//! stored. It also means there is one copy of each harness fact: a typed
//! column cannot disagree with the record because it *is* the record.
//!
//! What stays physical is what is not the harness's: the row key, brokkr's
//! own gate label `disposition` (derived by `ProbeLine::disposition`, never
//! trusted from the line), the pin's `expected`, the gate's `gate_ok`, and
//! `raw_source`. `raw_source` is `harness` for a parsed harness line and
//! `reconstructed` for a row migrated from the typed schema (v4 and older),
//! whose record was rebuilt from exactly the columns it kept - so on such a
//! row every diagnostic the old schema never had reads NULL, meaning "not
//! retained", not zero.

use std::path::Path;

use super::CorpusDb;
use super::migrate;
use crate::error::DevError;

/// How one harness field is projected out of `raw_json`.
///
/// Every projection is gated on the JSON type it expects and reads NULL for
/// anything else. The parser no longer type-checks the fields it does not
/// read, so a mistyped value reaches the store, and ungated it would project
/// as garbage: SQLite orders every integer before any text, so the count
/// clamp's `MIN` would turn a string into `i64::MAX`, and a text
/// `runtime_ms` would fail every typed read of the column.
#[derive(Clone, Copy)]
enum Projection {
    /// A JSON string.
    Text(&'static str),
    /// A JSON string, with `""` read as NULL - the old ingest's `ne()`.
    NonEmpty(&'static str),
    /// A JSON array, as its JSON text.
    Array(&'static str),
    /// A JSON number, as a real.
    Real(&'static str),
    /// A non-negative JSON integer, clamped to `i64::MAX` as the old
    /// ingest's `as_i64` did: the harness sends `u64`, and one past
    /// `i64::MAX` comes back from `json_extract` as a REAL no typed read
    /// accepts. NULL when absent.
    Count(&'static str),
    /// [`Projection::Count`], 0 when absent - the parser's serde default.
    CountOr0(&'static str),
    /// A JSON boolean, as 1/0.
    Flag(&'static str),
    /// The acceptance `failing` array, `'[]'` when absent.
    Failing,
    /// `dimension_breaches`, present exactly when a signature object is
    /// (0 when that object omits it), clamped like a count.
    Breaches,
}

impl Projection {
    fn sql(self) -> String {
        let at = |p: &str| format!("json_extract(raw_json, '{p}')");
        let when = |p: &str, types: &str, then: String| {
            format!("CASE WHEN json_type(raw_json, '{p}') IN ({types}) THEN {then} END")
        };
        let count = |p: &str| {
            when(p, "'integer', 'real'", format!("MIN({}, 9223372036854775807)", at(p)))
        };
        match self {
            Projection::Text(p) => when(p, "'text'", at(p)),
            Projection::NonEmpty(p) => when(p, "'text'", format!("NULLIF({}, '')", at(p))),
            Projection::Array(p) => when(p, "'array'", at(p)),
            Projection::Real(p) => when(p, "'integer', 'real'", at(p)),
            Projection::Flag(p) => when(p, "'true', 'false'", at(p)),
            Projection::Count(p) => count(p),
            Projection::CountOr0(p) => format!("COALESCE({}, 0)", count(p)),
            Projection::Failing => format!(
                "COALESCE({}, '[]')",
                when("$.acceptance.failing", "'array'", at("$.acceptance.failing"))
            ),
            Projection::Breaches => format!(
                "CASE WHEN json_type(raw_json, '$.signature') = 'object' THEN COALESCE({}, 0) END",
                count("$.signature.dimension_breaches")
            ),
        }
    }

    fn sql_type(self) -> &'static str {
        match self {
            Projection::Text(_)
            | Projection::NonEmpty(_)
            | Projection::Array(_)
            | Projection::Failing => "TEXT",
            Projection::Real(_) => "REAL",
            _ => "INTEGER",
        }
    }
}

/// The `disposition` table's columns after the physical ones: every harness
/// field, projected from `raw_json`. The projections reproduce what the typed
/// schema stored, which the v5 migration verifies row by row: counts default
/// to 0 and clamp like the old ingest, an empty acceptance/signature string is
/// NULL, `acc_failing` is `'[]'` when absent, and `sig_breaches` is present
/// exactly when a signature object is. Declared types give the projections
/// their affinity, so an integral p90 reads back REAL as it was stored.
const PROJECTIONS: [(&str, Projection); 39] = [
    ("outcome", Projection::Text("$.outcome")),
    ("matched", Projection::CountOr0("$.matched")),
    ("ours_only", Projection::CountOr0("$.ours_only")),
    ("tv_only", Projection::CountOr0("$.tv_only")),
    ("boundary_ours", Projection::CountOr0("$.boundary_ours")),
    ("boundary_tv", Projection::CountOr0("$.boundary_tv")),
    ("count_tier", Projection::Text("$.count_tier")),
    ("acc_tier", Projection::NonEmpty("$.acceptance.tier")),
    ("acc_profile", Projection::NonEmpty("$.acceptance.profile")),
    ("acc_failing", Projection::Failing),
    ("p90_entry", Projection::Real("$.acceptance.p90.entry")),
    ("p90_exit", Projection::Real("$.acceptance.p90.exit")),
    ("p90_pnl", Projection::Real("$.acceptance.p90.pnl")),
    ("sig_domain", Projection::NonEmpty("$.signature.domain")),
    ("sig_leg", Projection::NonEmpty("$.signature.leg")),
    ("sig_dimension", Projection::NonEmpty("$.signature.dimension")),
    ("sig_detail", Projection::NonEmpty("$.signature.detail")),
    ("sig_breaches", Projection::Breaches),
    ("error", Projection::Text("$.error")),
    ("runtime_ms", Projection::Real("$.runtime_ms")),
    ("boundary_anchor", Projection::Text("$.boundary_anchor")),
    ("anchor_consumed", Projection::Flag("$.boundary_rules.start_anchor_consumed")),
    ("rule_start_ours", Projection::Count("$.boundary_rules.start_ours")),
    ("rule_tail_ours", Projection::Count("$.boundary_rules.tail_ours")),
    ("rule_start_tv", Projection::Count("$.boundary_rules.start_tv")),
    ("rule_end_tv", Projection::Count("$.boundary_rules.end_tv")),
    ("clipped_ours", Projection::Count("$.clipped_ours")),
    ("clipped_tv", Projection::Count("$.clipped_tv")),
    ("history_prefix_bars", Projection::Count("$.history_prefix_bars")),
    ("oracle_trimmed", Projection::Count("$.oracle_trimmed")),
    ("oracle_realtime", Projection::Count("$.oracle_realtime")),
    ("ts_entry_considered", Projection::Count("$.ts_shift.entry_considered")),
    ("ts_entry_shifted", Projection::Count("$.ts_shift.entry_shifted")),
    ("ts_entry_share_pct", Projection::Real("$.ts_shift.entry_share_pct")),
    ("ts_exit_considered", Projection::Count("$.ts_shift.exit_considered")),
    ("ts_exit_shifted", Projection::Count("$.ts_shift.exit_shifted")),
    ("ts_exit_share_pct", Projection::Real("$.ts_shift.exit_share_pct")),
    ("window_sensitive", Projection::Array("$.window_sensitive")),
    ("dynamic_builtin_calls", Projection::Count("$.dynamic_builtin_calls")),
];

/// The `disposition` DDL under `name` - `disposition` itself, or the v5
/// migration's staging table, which must be the same definition.
pub(super) fn disposition_ddl(name: &str) -> String {
    let projections: Vec<String> = PROJECTIONS
        .iter()
        .map(|(col, p)| {
            format!("    {col} {} GENERATED ALWAYS AS ({}) VIRTUAL", p.sql_type(), p.sql())
        })
        .collect();
    format!(
        "CREATE TABLE IF NOT EXISTS {name} (
    run_id        INTEGER NOT NULL REFERENCES run(run_id),
    probe         TEXT NOT NULL,
    disposition   TEXT NOT NULL,
    expected      TEXT,
    gate_ok       INTEGER NOT NULL,
    raw_source    TEXT NOT NULL,
    raw_json      TEXT NOT NULL,
{},
    PRIMARY KEY (run_id, probe)
);
CREATE INDEX IF NOT EXISTS idx_{name}_probe ON {name}(probe, run_id);",
        projections.join(",\n")
    )
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS run (
    run_id            INTEGER PRIMARY KEY,
    started_at        TEXT NOT NULL,
    selector          TEXT NOT NULL,
    gated             INTEGER NOT NULL,
    result            TEXT NOT NULL,
    fail_reason       TEXT,
    harness_exit_code INTEGER,
    probe_count       INTEGER NOT NULL,
    harness_stderr    TEXT,
    wall_ms           REAL,
    commit_sha        TEXT,
    dirty             INTEGER
);
CREATE INDEX IF NOT EXISTS idx_run_started ON run(started_at);

CREATE TABLE IF NOT EXISTS trade_diff (
    run_id            INTEGER NOT NULL REFERENCES run(run_id),
    probe             TEXT NOT NULL,
    our_index         INTEGER NOT NULL,
    tv_index          INTEGER NOT NULL,
    our_entry_ts      INTEGER NOT NULL,
    our_exit_ts       INTEGER NOT NULL,
    our_entry_price   REAL NOT NULL,
    our_exit_price    REAL NOT NULL,
    our_qty           REAL NOT NULL,
    our_pnl           REAL NOT NULL,
    entry_ts_delta    INTEGER,
    exit_ts_delta     INTEGER,
    entry_price_delta REAL,
    exit_price_delta  REAL,
    our_entry_bar     INTEGER,
    our_exit_bar      INTEGER,
    our_side          TEXT,
    our_entry_id      TEXT,
    our_exit_id       TEXT,
    tv_entry_ts       INTEGER,
    tv_exit_ts        INTEGER,
    tv_entry_price    REAL,
    tv_exit_price     REAL,
    tv_entry_qty      REAL,
    tv_pnl            REAL,
    tv_entry_signal   TEXT,
    tv_exit_signal    TEXT,
    PRIMARY KEY (run_id, probe, our_index, tv_index)
);

CREATE TABLE IF NOT EXISTS gate_miss (
    run_id   INTEGER NOT NULL REFERENCES run(run_id),
    probe    TEXT NOT NULL,
    expected TEXT,
    actual   TEXT,
    PRIMARY KEY (run_id, probe)
);

CREATE TABLE IF NOT EXISTS dense_na_site (
    id        INTEGER PRIMARY KEY,
    run_id    INTEGER NOT NULL REFERENCES run(run_id),
    probe     TEXT NOT NULL,
    name      TEXT NOT NULL,
    call_site TEXT NOT NULL,
    na_count  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_dense_na_run ON dense_na_site(run_id);
";

impl CorpusDb {
    /// Open (or create) the database read-write at `path`, creating the parent
    /// directory, enabling WAL, running migrations, and applying the schema.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate::run_migrations(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&disposition_ddl("disposition"))?;
        conn.pragma_update(None, "user_version", migrate::SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    /// Open the database read-only for queries. The read-only flag is the
    /// load-bearing guard behind the `--where`/`--sql` raw-SQL paths: SQLite
    /// rejects any write regardless of what the interpolated SQL asks for.
    ///
    /// A store written by an older brokkr is brought to the current schema
    /// first, through a short-lived read-write [`Self::open`]: the read paths
    /// (the runtime ceiling's `wall_ms`, the `boundary_*` columns) name columns
    /// only a migrated store has, and a read-only connection cannot migrate -
    /// so without this step an old `runs.db` failed every query and blocked
    /// every corpus run short of `--force`. The migrations are column adds
    /// plus the v5 `disposition` rebuild; the read-only connection the caller
    /// gets is opened afterwards.
    pub fn open_readonly(path: &Path) -> Result<Self, DevError> {
        let conn = Self::readonly_conn(path)?;
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current >= migrate::SCHEMA_VERSION {
            return Ok(Self { conn });
        }
        drop(conn);
        drop(Self::open(path)?);
        Ok(Self {
            conn: Self::readonly_conn(path)?,
        })
    }

    fn readonly_conn(path: &Path) -> Result<rusqlite::Connection, DevError> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        // Belt-and-suspenders; read-only open already blocks writes.
        conn.pragma_update(None, "query_only", "ON").ok();
        Ok(conn)
    }

    /// Borrow the underlying connection (crate-internal, for sibling modules).
    pub(super) fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// In-memory database for tests - applies the schema, skips WAL/migrate
    /// (no file, nothing to migrate). Keeps test data out of `/tmp`.
    #[cfg(test)]
    pub(super) fn open_in_memory() -> Result<Self, DevError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&disposition_ddl("disposition"))?;
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn readonly_open_migrates_an_older_store_first() {
        // A pre-v4 store on disk: `run` has no `wall_ms`, and `disposition`
        // is the v1 shape. The ceiling's query names `wall_ms`.
        let dir = crate::test_scratch::scratch("piners_corpus_db_schema", "old_store");
        let path = dir.join("runs.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE run (run_id INTEGER PRIMARY KEY, started_at TEXT NOT NULL, \
                     selector TEXT NOT NULL, gated INTEGER NOT NULL, result TEXT NOT NULL, \
                     fail_reason TEXT, harness_exit_code INTEGER, probe_count INTEGER NOT NULL, \
                     harness_stderr TEXT);
                 CREATE TABLE disposition (run_id INTEGER NOT NULL, probe TEXT NOT NULL, \
                     outcome TEXT NOT NULL, disposition TEXT NOT NULL, expected TEXT, \
                     gate_ok INTEGER NOT NULL, matched INTEGER NOT NULL, \
                     ours_only INTEGER NOT NULL, tv_only INTEGER NOT NULL, count_tier TEXT, \
                     acc_tier TEXT, acc_profile TEXT, acc_failing TEXT NOT NULL DEFAULT '[]', \
                     p90_entry REAL, p90_exit REAL, p90_pnl REAL, sig_domain TEXT, \
                     sig_leg TEXT, sig_dimension TEXT, sig_detail TEXT, sig_breaches INTEGER, \
                     error TEXT, PRIMARY KEY (run_id, probe));
                 INSERT INTO run VALUES (1, 'then', '{}', 1, 'pass', NULL, 0, 1, '');
                 INSERT INTO disposition (run_id, probe, outcome, disposition, gate_ok, \
                     matched, ours_only, tv_only) VALUES (1, 'p', 'parity', 'accepted', 1, 3, 0, 0);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let db = CorpusDb::open_readonly(&path).unwrap();
        assert_eq!(db.estimated_wall_ms(&["a".to_owned()], true).unwrap(), None);
        // The v1 row survived the v5 rebuild and answers the typed queries.
        let d = db.disposition_for_probe(1, "p").unwrap().unwrap();
        assert_eq!((d.matched, d.disposition.as_str()), (3, "accepted"));
        let version: i64 = db
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, migrate::SCHEMA_VERSION);
    }
}
