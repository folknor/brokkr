//! Read paths for the corpus database.
//!
//! Canned queries are `?N`-parameterized exactly like
//! `src/db/query.rs::build_query_sql`. The `--where` and `--sql` paths
//! interpolate trusted local SQL instead - the feature's purpose is ad-hoc
//! exploration of the user's own DB, and you cannot `?N`-bind an arbitrary
//! boolean expression. Safety rests on the read-only connection
//! ([`super::CorpusDb::open_readonly`]); the caller adds a SELECT-only UX
//! guard before reaching here.

use rusqlite::{OptionalExtension, Row};

use super::CorpusDb;
use crate::error::DevError;

/// One row of the recent-runs table.
pub struct RunRow {
    pub run_id: i64,
    pub started_at: String,
    pub selector: String,
    pub gated: bool,
    pub result: String,
    pub fail_reason: Option<String>,
    pub harness_exit_code: Option<i64>,
    pub probe_count: i64,
}

/// One per-probe disposition row (the rendered subset of the column set).
#[derive(Clone)]
pub struct DispositionRow {
    pub probe: String,
    pub outcome: String,
    pub disposition: String,
    pub expected: Option<String>,
    pub gate_ok: bool,
    pub matched: i64,
    pub ours_only: i64,
    pub tv_only: i64,
    pub count_tier: Option<String>,
    pub p90_entry: Option<f64>,
    pub p90_exit: Option<f64>,
    pub p90_pnl: Option<f64>,
    pub sig_domain: Option<String>,
    pub sig_dimension: Option<String>,
    pub error: Option<String>,
}

/// One per-trade drill-down row (the rendered subset).
pub struct TradeDiffRow {
    pub our_index: i64,
    pub tv_index: i64,
    pub our_side: Option<String>,
    pub entry_ts_delta: Option<i64>,
    pub exit_ts_delta: Option<i64>,
    pub entry_price_delta: Option<f64>,
    pub exit_price_delta: Option<f64>,
    pub our_pnl: f64,
    pub tv_pnl: Option<f64>,
}

/// One row of a probe's cross-run trend.
pub struct TrendRow {
    pub run_id: i64,
    pub started_at: String,
    pub disposition: String,
    pub count_tier: Option<String>,
    pub gate_ok: bool,
    pub matched: i64,
    pub ours_only: i64,
    pub tv_only: i64,
    pub p90_exit: Option<f64>,
}

/// A selected probe that produced no disposition line.
pub struct GateMissRow {
    pub probe: String,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

/// A generic stringified result set, for the `--where`/`--sql` raw paths.
pub struct RawTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Clamp a `usize` limit to `i64` for binding (limits never approach i64::MAX).
fn clamp(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

fn run_row(row: &Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run_id: row.get("run_id")?,
        started_at: row.get("started_at")?,
        selector: row.get("selector")?,
        gated: row.get::<_, i64>("gated")? != 0,
        result: row.get("result")?,
        fail_reason: row.get("fail_reason")?,
        harness_exit_code: row.get("harness_exit_code")?,
        probe_count: row.get("probe_count")?,
    })
}

fn disposition_row(row: &Row<'_>) -> rusqlite::Result<DispositionRow> {
    Ok(DispositionRow {
        probe: row.get("probe")?,
        outcome: row.get("outcome")?,
        disposition: row.get("disposition")?,
        expected: row.get("expected")?,
        gate_ok: row.get::<_, i64>("gate_ok")? != 0,
        matched: row.get("matched")?,
        ours_only: row.get("ours_only")?,
        tv_only: row.get("tv_only")?,
        count_tier: row.get("count_tier")?,
        p90_entry: row.get("p90_entry")?,
        p90_exit: row.get("p90_exit")?,
        p90_pnl: row.get("p90_pnl")?,
        sig_domain: row.get("sig_domain")?,
        sig_dimension: row.get("sig_dimension")?,
        error: row.get("error")?,
    })
}

const DISPOSITION_COLS: &str = "\
probe, outcome, disposition, expected, gate_ok, matched, ours_only, tv_only, \
count_tier, p90_entry, p90_exit, p90_pnl, sig_domain, sig_dimension, error";

impl CorpusDb {
    /// The newest `run_id`, or `None` if the DB has no runs.
    pub fn latest_run_id(&self) -> Result<Option<i64>, DevError> {
        let id = self
            .conn()
            .query_row("SELECT MAX(run_id) FROM run", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?;
        Ok(id)
    }

    /// The most recent `limit` runs, newest first.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunRow>, DevError> {
        let mut stmt = self.conn().prepare(
            "SELECT run_id, started_at, selector, gated, result, fail_reason, \
                    harness_exit_code, probe_count \
             FROM run ORDER BY run_id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([clamp(limit)], run_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The captured stderr for a run (for the detail view of a failure).
    pub fn run_stderr(&self, run_id: i64) -> Result<Option<String>, DevError> {
        let res = self.conn().query_row(
            "SELECT harness_stderr FROM run WHERE run_id = ?1",
            [run_id],
            |r| r.get::<_, Option<String>>(0),
        );
        match res {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// All disposition rows for a run, by probe.
    pub fn dispositions_for_run(&self, run_id: i64) -> Result<Vec<DispositionRow>, DevError> {
        let sql = format!(
            "SELECT {DISPOSITION_COLS} FROM disposition WHERE run_id = ?1 ORDER BY probe"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([run_id], disposition_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One probe's disposition in a given run.
    pub fn disposition_for_probe(
        &self,
        run_id: i64,
        probe: &str,
    ) -> Result<Option<DispositionRow>, DevError> {
        let sql = format!(
            "SELECT {DISPOSITION_COLS} FROM disposition WHERE run_id = ?1 AND probe = ?2"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let mut rows = stmt.query_map(rusqlite::params![run_id, probe], disposition_row)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Selected probes that produced no disposition line, for a run.
    pub fn gate_misses_for_run(&self, run_id: i64) -> Result<Vec<GateMissRow>, DevError> {
        let mut stmt = self.conn().prepare(
            "SELECT probe, expected, actual FROM gate_miss WHERE run_id = ?1 ORDER BY probe",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(GateMissRow {
                probe: r.get("probe")?,
                expected: r.get("expected")?,
                actual: r.get("actual")?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One probe's `trade_diff` rows in a given run.
    pub fn trade_diffs_for_probe(
        &self,
        run_id: i64,
        probe: &str,
    ) -> Result<Vec<TradeDiffRow>, DevError> {
        let mut stmt = self.conn().prepare(
            "SELECT our_index, tv_index, our_side, entry_ts_delta, exit_ts_delta, \
                    entry_price_delta, exit_price_delta, our_pnl, tv_pnl \
             FROM trade_diff WHERE run_id = ?1 AND probe = ?2 ORDER BY our_index",
        )?;
        let rows = stmt.query_map(rusqlite::params![run_id, probe], |r| {
            Ok(TradeDiffRow {
                our_index: r.get("our_index")?,
                tv_index: r.get("tv_index")?,
                our_side: r.get("our_side")?,
                entry_ts_delta: r.get("entry_ts_delta")?,
                exit_ts_delta: r.get("exit_ts_delta")?,
                entry_price_delta: r.get("entry_price_delta")?,
                exit_price_delta: r.get("exit_price_delta")?,
                our_pnl: r.get("our_pnl")?,
                tv_pnl: r.get("tv_pnl")?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// A probe's disposition over the most recent `limit` runs it appears in,
    /// newest first.
    pub fn trend_for_probe(&self, probe: &str, limit: usize) -> Result<Vec<TrendRow>, DevError> {
        let mut stmt = self.conn().prepare(
            "SELECT d.run_id AS run_id, r.started_at AS started_at, d.disposition AS disposition, \
                    d.count_tier AS count_tier, d.gate_ok AS gate_ok, d.matched AS matched, \
                    d.ours_only AS ours_only, d.tv_only AS tv_only, d.p90_exit AS p90_exit \
             FROM disposition d JOIN run r ON r.run_id = d.run_id \
             WHERE d.probe = ?1 ORDER BY d.run_id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![probe, clamp(limit)], |r| {
            Ok(TrendRow {
                run_id: r.get("run_id")?,
                started_at: r.get("started_at")?,
                disposition: r.get("disposition")?,
                count_tier: r.get("count_tier")?,
                gate_ok: r.get::<_, i64>("gate_ok")? != 0,
                matched: r.get("matched")?,
                ours_only: r.get("ours_only")?,
                tv_only: r.get("tv_only")?,
                p90_exit: r.get("p90_exit")?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `trade_diff` rows for a run filtered by a raw boolean expression
    /// (trusted local SQL; the connection is read-only).
    pub fn diffs_where(&self, run_id: i64, where_expr: &str) -> Result<RawTable, DevError> {
        let sql = format!(
            "SELECT probe, our_index, tv_index, our_side, entry_price_delta, exit_price_delta, \
                    entry_ts_delta, exit_ts_delta, our_pnl, tv_pnl \
             FROM trade_diff WHERE run_id = ?1 AND ({where_expr}) ORDER BY probe, our_index"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        read_raw(&mut stmt, rusqlite::params![run_id])
    }

    /// Estimated wall-clock runtime for a selection, in milliseconds: the sum
    /// over `probes` of each probe's most recent recorded `runtime_ms` (the
    /// latest run in which it carries a non-null runtime). Probes never run, or
    /// run only on harness output predating `runtime_ms`, contribute 0. Drives
    /// the pre-run ceiling in `cmd.rs`.
    pub fn estimated_runtime_ms(&self, probes: &[String]) -> Result<f64, DevError> {
        let mut stmt = self.conn().prepare(
            "SELECT runtime_ms FROM disposition \
             WHERE probe = ?1 AND runtime_ms IS NOT NULL \
             ORDER BY run_id DESC LIMIT 1",
        )?;
        let mut total = 0.0;
        for probe in probes {
            if let Some(ms) = stmt.query_row([probe], |r| r.get::<_, f64>(0)).optional()? {
                total += ms;
            }
        }
        Ok(total)
    }

    /// Run an arbitrary read-only query (the `--sql` escape hatch).
    pub fn raw_sql(&self, sql: &str) -> Result<RawTable, DevError> {
        let mut stmt = self.conn().prepare(sql)?;
        read_raw(&mut stmt, [])
    }
}

/// Execute a prepared statement and stringify every cell, capturing the column
/// names. Shared by the `--where` and `--sql` raw paths.
fn read_raw(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<RawTable, DevError> {
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let n = columns.len();
    let rows = stmt.query_map(params, |row| {
        let mut cells = Vec::with_capacity(n);
        for i in 0..n {
            cells.push(value_to_string(row.get_ref(i)?));
        }
        Ok(cells)
    })?;
    Ok(RawTable {
        columns,
        rows: rows.collect::<Result<Vec<_>, _>>()?,
    })
}

fn value_to_string(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(_) => "<blob>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    use std::collections::BTreeMap;

    use super::*;
    use crate::piners::report::parse;

    /// Record one run from an NDJSON literal, no gate context.
    fn record(db: &CorpusDb, result: &str, nd: &[u8]) {
        let report = parse(nd);
        let run = crate::piners::corpus_db::RunRecord {
            selector: "{}",
            gated: true,
            result,
            fail_reason: None,
            harness_exit_code: Some(0),
            stderr: "",
        };
        db.record_run(&run, &report, &BTreeMap::new(), &[]).unwrap();
    }

    #[test]
    fn estimated_runtime_sums_latest_per_probe_and_treats_missing_as_zero() {
        let db = CorpusDb::open_in_memory().unwrap();
        // Run 1: p1=100ms, p2=200ms.
        record(
            &db,
            "pass",
            br#"{"probe":"p1","outcome":"parity","runtime_ms":100}
{"probe":"p2","outcome":"parity","runtime_ms":200}
"#,
        );
        // Run 2: only p1, now slower (150ms). p2 absent -> its run-1 value stands.
        record(
            &db,
            "pass",
            br#"{"probe":"p1","outcome":"parity","runtime_ms":150}
"#,
        );

        // p1 latest (150) + p2 latest (200) + p3 unseen (0) = 350.
        let est = db
            .estimated_runtime_ms(&["p1".to_owned(), "p2".to_owned(), "p3".to_owned()])
            .unwrap();
        assert_eq!(est, 350.0);

        // A selection of only never-run probes estimates 0.
        assert_eq!(db.estimated_runtime_ms(&["nope".to_owned()]).unwrap(), 0.0);
    }

    #[test]
    fn estimated_runtime_ignores_null_runtime_rows() {
        let db = CorpusDb::open_in_memory().unwrap();
        // A probe whose latest run carries no runtime_ms falls back to the most
        // recent run that does; with none at all it contributes 0.
        record(
            &db,
            "pass",
            br#"{"probe":"p1","outcome":"parity","runtime_ms":120}
"#,
        );
        record(
            &db,
            "pass",
            br#"{"probe":"p1","outcome":"parity"}
"#,
        );
        // Latest p1 row has NULL runtime; the 120ms run-1 row is used.
        assert_eq!(db.estimated_runtime_ms(&["p1".to_owned()]).unwrap(), 120.0);
    }
}
