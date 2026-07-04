//! Read paths for the corpus database.
//!
//! Canned queries are `?N`-parameterized exactly like
//! `src/db/query.rs::build_query_sql`. The `--where` and `--sql` paths
//! interpolate trusted local SQL instead - the feature's purpose is ad-hoc
//! exploration of the user's own DB, and you cannot `?N`-bind an arbitrary
//! boolean expression. Safety rests on the read-only connection
//! ([`super::CorpusDb::open_readonly`]); the caller adds a SELECT-only UX
//! guard before reaching here.

use rusqlite::Row;

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
    /// Window-boundary-artifact discount: `ours_only`/`tv_only` stay raw, these
    /// explain the gap to the effective divergence the label was scored on.
    pub boundary_ours: i64,
    pub boundary_tv: i64,
    pub count_tier: Option<String>,
    pub p90_entry: Option<f64>,
    pub p90_exit: Option<f64>,
    pub p90_pnl: Option<f64>,
    pub sig_domain: Option<String>,
    pub sig_dimension: Option<String>,
    pub error: Option<String>,
}

/// One per-trade drill-down row (the rendered subset). `our_qty`/`tv_entry_qty`
/// carry the size axis the curated view historically dropped - the field the
/// pyramiding investigations turned on.
pub struct TradeDiffRow {
    pub our_index: i64,
    pub tv_index: i64,
    pub our_side: Option<String>,
    pub entry_ts_delta: Option<i64>,
    pub exit_ts_delta: Option<i64>,
    pub entry_price_delta: Option<f64>,
    pub exit_price_delta: Option<f64>,
    pub our_qty: f64,
    pub tv_entry_qty: Option<f64>,
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
    pub boundary_ours: i64,
    pub boundary_tv: i64,
    pub p90_exit: Option<f64>,
}

/// One probe's most-recent runtime, for the `--runtimes` view.
pub struct RuntimeRow {
    pub probe: String,
    pub runtime_ms: f64,
    pub run_id: i64,
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
        boundary_ours: row.get("boundary_ours")?,
        boundary_tv: row.get("boundary_tv")?,
        count_tier: row.get("count_tier")?,
        p90_entry: row.get("p90_entry")?,
        p90_exit: row.get("p90_exit")?,
        p90_pnl: row.get("p90_pnl")?,
        sig_domain: row.get("sig_domain")?,
        sig_dimension: row.get("sig_dimension")?,
        error: row.get("error")?,
    })
}

fn runtime_row(row: &Row<'_>) -> rusqlite::Result<RuntimeRow> {
    Ok(RuntimeRow {
        probe: row.get("probe")?,
        runtime_ms: row.get("runtime_ms")?,
        run_id: row.get("run_id")?,
    })
}

const DISPOSITION_COLS: &str = "\
probe, outcome, disposition, expected, gate_ok, matched, ours_only, tv_only, \
boundary_ours, boundary_tv, count_tier, p90_entry, p90_exit, p90_pnl, \
sig_domain, sig_dimension, error";

/// Every queryable `trade_diff` column - the 26 harness fields (`run_id` is
/// excluded; the run is already fixed by the query). This is the allow-list
/// behind `--columns`: only an identifier appearing here is ever interpolated
/// into the projection's SELECT, so a typo can't become SQL injection. Listed
/// in the schema's column order so `--columns all` reads naturally.
pub const TRADE_DIFF_COLUMNS: &[&str] = &[
    "probe",
    "our_index",
    "tv_index",
    "our_entry_ts",
    "our_exit_ts",
    "our_entry_price",
    "our_exit_price",
    "our_qty",
    "our_pnl",
    "entry_ts_delta",
    "exit_ts_delta",
    "entry_price_delta",
    "exit_price_delta",
    "our_entry_bar",
    "our_exit_bar",
    "our_side",
    "our_entry_id",
    "our_exit_id",
    "tv_entry_ts",
    "tv_exit_ts",
    "tv_entry_price",
    "tv_exit_price",
    "tv_entry_qty",
    "tv_pnl",
    "tv_entry_signal",
    "tv_exit_signal",
];

/// The curated default projection for `--diffs`: the four axes a trade pair can
/// diverge on - time, price, size, pnl - at a glance. `our_qty`/`tv_entry_qty`
/// are the size axis the old hard-coded view dropped. `--columns all` widens to
/// every column (rendered vertically); `--columns a,b,c` picks a subset.
pub const DEFAULT_DIFF_COLUMNS: &[&str] = &[
    "probe",
    "our_index",
    "tv_index",
    "our_side",
    "entry_ts_delta",
    "exit_ts_delta",
    "entry_price_delta",
    "exit_price_delta",
    "our_qty",
    "tv_entry_qty",
    "our_pnl",
    "tv_pnl",
];

/// Resolve a `--columns` request into a validated SELECT list. Empty -> the
/// curated [`DEFAULT_DIFF_COLUMNS`]; the lone token `all` -> every column;
/// otherwise each name must be a known [`TRADE_DIFF_COLUMNS`] entry. An unknown
/// name errors with the full valid set - that error *is* the column-discovery
/// path, which is why there is no separate `--list-columns`.
pub fn resolve_diff_columns(requested: &[String]) -> Result<Vec<String>, DevError> {
    let owned = |cols: &[&str]| cols.iter().map(|s| (*s).to_owned()).collect();
    if requested.is_empty() {
        return Ok(owned(DEFAULT_DIFF_COLUMNS));
    }
    if requested.len() == 1 && requested[0] == "all" {
        return Ok(owned(TRADE_DIFF_COLUMNS));
    }
    let mut out = Vec::with_capacity(requested.len());
    for c in requested {
        if c == "all" {
            return Err(DevError::Config(
                "results --columns: 'all' selects every column and must stand alone, \
                 not be mixed with named columns"
                    .to_owned(),
            ));
        }
        if !TRADE_DIFF_COLUMNS.contains(&c.as_str()) {
            return Err(DevError::Config(format!(
                "results --columns: unknown trade_diff column '{c}'. Valid columns:\n  {}",
                TRADE_DIFF_COLUMNS.join(", ")
            )));
        }
        out.push(c.clone());
    }
    Ok(out)
}

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
                    entry_price_delta, exit_price_delta, our_qty, tv_entry_qty, our_pnl, tv_pnl \
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
                our_qty: r.get("our_qty")?,
                tv_entry_qty: r.get("tv_entry_qty")?,
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
                    d.ours_only AS ours_only, d.tv_only AS tv_only, \
                    d.boundary_ours AS boundary_ours, d.boundary_tv AS boundary_tv, \
                    d.p90_exit AS p90_exit \
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
                boundary_ours: r.get("boundary_ours")?,
                boundary_tv: r.get("boundary_tv")?,
                p90_exit: r.get("p90_exit")?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `trade_diff` rows for a run, narrowed to a `probes` set (empty = all
    /// probes in the run), projected onto `columns`, optionally further filtered
    /// by a raw boolean expression. `columns` must come from
    /// [`resolve_diff_columns`] - it is interpolated, so the allow-list is the
    /// only thing standing between projection and SQL injection; the probe set
    /// and `run_id` are bound, and `where_expr` is trusted local input against a
    /// read-only connection. Ordered (probe, our_index).
    pub fn diffs(
        &self,
        run_id: i64,
        probes: &[String],
        columns: &[String],
        where_expr: Option<&str>,
    ) -> Result<RawTable, DevError> {
        let select = columns.join(", ");
        let mut sql = format!("SELECT {select} FROM trade_diff WHERE run_id = ?1");
        if !probes.is_empty() {
            // probe IN (?2, ?3, ...) - the ids are bound, never interpolated.
            let placeholders: Vec<String> =
                (0..probes.len()).map(|i| format!("?{}", i + 2)).collect();
            sql.push_str(&format!(" AND probe IN ({})", placeholders.join(", ")));
        }
        if let Some(expr) = where_expr {
            sql.push_str(&format!(" AND ({expr})"));
        }
        sql.push_str(" ORDER BY probe, our_index");
        let mut stmt = self.conn().prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + probes.len());
        params.push(&run_id);
        for p in probes {
            params.push(p);
        }
        read_raw(&mut stmt, params.as_slice())
    }

    /// Each probe's most recent recorded runtime (the latest run carrying a
    /// non-null `runtime_ms`), slowest first. A *diagnostic* view - "which probe
    /// is heavy" - not the ceiling's basis: the harness overlaps probes, so the
    /// sum of these per-probe figures runs several times the real wall (see
    /// [`Self::estimated_wall_ms`], which the ceiling uses instead). `over_ms`,
    /// when set, keeps only probes above it.
    pub fn runtimes(&self, over_ms: Option<f64>) -> Result<Vec<RuntimeRow>, DevError> {
        let mut sql = String::from(
            "SELECT probe, runtime_ms, run_id FROM disposition d \
             WHERE runtime_ms IS NOT NULL \
               AND run_id = (SELECT MAX(run_id) FROM disposition d2 \
                             WHERE d2.probe = d.probe AND d2.runtime_ms IS NOT NULL)",
        );
        if over_ms.is_some() {
            sql.push_str(" AND runtime_ms > ?1");
        }
        sql.push_str(" ORDER BY runtime_ms DESC");
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = match over_ms {
            Some(ms) => stmt.query_map([ms], runtime_row)?.collect::<Result<Vec<_>, _>>()?,
            None => stmt.query_map([], runtime_row)?.collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    /// Estimated whole-run wall for a `selection`, in milliseconds: the measured
    /// `wall_ms` of the most recent run whose own selection was a **superset** of
    /// (or equal to) `selection`. Since dropping probes can only shorten a run,
    /// `wall(subset) <= wall(superset)`, so a covering run's real wall is a valid
    /// upper bound - and any `--all` run covers everything, so one full run bounds
    /// every selection. Returns `None` when no recorded run covers `selection`
    /// (a fresh DB, or a selection no prior run is a superset of); the caller
    /// treats that as "no measured basis, don't refuse".
    ///
    /// This replaces the old sum-of-per-probe-`runtime_ms` estimate, which
    /// assumed serial probes and so overshot the real (probe-overlapping) wall
    /// several-fold. Coverage is read off the stored `selector` JSON `ids`.
    pub fn estimated_wall_ms(&self, selection: &[String]) -> Result<Option<f64>, DevError> {
        let want: std::collections::HashSet<&str> =
            selection.iter().map(String::as_str).collect();
        // Newest first; stop at the first run whose id-set covers the selection.
        // The most recent run is often `--all` (covers everything), so this
        // typically returns on the first row.
        let mut stmt = self.conn().prepare(
            "SELECT selector, wall_ms FROM run \
             WHERE wall_ms IS NOT NULL ORDER BY run_id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        for row in rows {
            let (selector, wall_ms) = row?;
            if selection_covered(&selector, &want) {
                return Ok(Some(wall_ms));
            }
        }
        Ok(None)
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

/// Does the run whose stored `selector` JSON is `selector` cover every id in
/// `want`? Coverage is set-inclusion of the selector's `ids` array over `want`.
/// A selector that fails to parse, or carries no `ids`, covers nothing (returns
/// `false`) - a malformed row is skipped, never treated as a universal bound.
fn selection_covered(selector: &str, want: &std::collections::HashSet<&str>) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(selector) else {
        return false;
    };
    let Some(ids) = value.get("ids").and_then(|v| v.as_array()) else {
        return false;
    };
    let have: std::collections::HashSet<&str> =
        ids.iter().filter_map(serde_json::Value::as_str).collect();
    want.iter().all(|id| have.contains(id))
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

    /// Record one run from an NDJSON literal, no gate context, no wall.
    fn record(db: &CorpusDb, result: &str, nd: &[u8]) {
        record_full(db, "{}", None, result, nd);
    }

    /// Record a run with an explicit `selector` JSON and measured `wall_ms` -
    /// the inputs the superset-wall estimator reads.
    fn record_full(db: &CorpusDb, selector: &str, wall_ms: Option<f64>, result: &str, nd: &[u8]) {
        let report = parse(nd);
        let run = crate::piners::corpus_db::RunRecord {
            selector,
            gated: true,
            result,
            fail_reason: None,
            harness_exit_code: Some(0),
            stderr: "",
            wall_ms,
        };
        db.record_run(&run, &report, &BTreeMap::new(), &[]).unwrap();
    }

    /// Body NDJSON is irrelevant to the wall estimator (it reads the run
    /// envelope's selector + wall_ms), so the wall tests use a one-probe line.
    const ONE_LINE: &[u8] = br#"{"probe":"x","outcome":"parity"}
"#;

    #[test]
    fn estimated_wall_uses_the_most_recent_superset_runs_measured_wall() {
        let db = CorpusDb::open_in_memory().unwrap();
        // Run 1: an `--all`-style full run over [a,b,c], wall 60s.
        record_full(&db, r#"{"ids":["a","b","c"]}"#, Some(60_000.0), "pass", ONE_LINE);
        // Run 2: a smaller slice [a], wall 5s.
        record_full(&db, r#"{"ids":["a"]}"#, Some(5_000.0), "pass", ONE_LINE);

        // Selecting {a,b}: run 2 ([a]) does NOT cover it; run 1 ([a,b,c]) does,
        // so the estimate is run 1's real 60s wall - the valid upper bound.
        let est = db
            .estimated_wall_ms(&["a".to_owned(), "b".to_owned()])
            .unwrap();
        assert_eq!(est, Some(60_000.0));

        // Selecting {a}: the newest covering run wins - run 2's 5s, not run 1's.
        assert_eq!(db.estimated_wall_ms(&["a".to_owned()]).unwrap(), Some(5_000.0));
    }

    #[test]
    fn estimated_wall_is_none_when_no_run_covers_the_selection() {
        let db = CorpusDb::open_in_memory().unwrap();
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(10_000.0), "pass", ONE_LINE);

        // `z` is not in any recorded run's selection -> no covering run -> None
        // (the caller reads this as "no measured basis, don't refuse").
        assert_eq!(db.estimated_wall_ms(&["z".to_owned()]).unwrap(), None);
        // A partial overlap still isn't coverage: {a,z} needs BOTH in one run.
        assert_eq!(
            db.estimated_wall_ms(&["a".to_owned(), "z".to_owned()]).unwrap(),
            None
        );
    }

    #[test]
    fn estimated_wall_skips_runs_with_no_measured_wall() {
        let db = CorpusDb::open_in_memory().unwrap();
        // Newest covering run has NULL wall (e.g. a spawn failure) -> skipped;
        // the older covering run with a real wall is used.
        record_full(&db, r#"{"ids":["a"]}"#, Some(8_000.0), "pass", ONE_LINE);
        record_full(&db, r#"{"ids":["a"]}"#, None, "fail", ONE_LINE);
        assert_eq!(db.estimated_wall_ms(&["a".to_owned()]).unwrap(), Some(8_000.0));
    }

    fn owned(cols: &[&str]) -> Vec<String> {
        cols.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn resolve_columns_empty_is_the_curated_default_with_qty() {
        let cols = resolve_diff_columns(&[]).unwrap();
        assert_eq!(cols, owned(DEFAULT_DIFF_COLUMNS));
        // The whole point of the change: qty is in the default.
        assert!(cols.iter().any(|c| c == "our_qty"));
        assert!(cols.iter().any(|c| c == "tv_entry_qty"));
    }

    #[test]
    fn resolve_columns_all_is_every_column() {
        assert_eq!(resolve_diff_columns(&owned(&["all"])).unwrap(), owned(TRADE_DIFF_COLUMNS));
    }

    #[test]
    fn resolve_columns_validates_and_preserves_order() {
        let req = owned(&["our_qty", "tv_entry_qty", "our_pnl"]);
        assert_eq!(resolve_diff_columns(&req).unwrap(), req);
        // Unknown name errors (and the message lists the valid set - the
        // discovery path that stands in for --list-columns).
        let err = resolve_diff_columns(&owned(&["our_qty", "bogus"])).unwrap_err();
        assert!(err.to_string().contains("bogus"));
        assert!(err.to_string().contains("our_qty"));
        // `all` mixed with names is rejected (it means "everything", alone).
        assert!(resolve_diff_columns(&owned(&["all", "our_qty"])).is_err());
    }

    #[test]
    fn runtimes_lists_latest_per_probe_slowest_first() {
        let db = CorpusDb::open_in_memory().unwrap();
        record(
            &db,
            "pass",
            br#"{"probe":"slow","outcome":"parity","runtime_ms":300000}
{"probe":"fast","outcome":"parity","runtime_ms":100}
"#,
        );
        // `slow` re-runs faster; the latest value must win (not the max).
        record(
            &db,
            "pass",
            br#"{"probe":"slow","outcome":"parity","runtime_ms":5000}
"#,
        );

        let rows = db.runtimes(None).unwrap();
        // Slowest first: slow(5000) then fast(100).
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].probe, "slow");
        assert_eq!(rows[0].runtime_ms, 5000.0);
        assert_eq!(rows[0].run_id, 2);
        assert_eq!(rows[1].probe, "fast");

        // --over filters in seconds: > 1s keeps only `slow` (5000ms).
        let over = db.runtimes(Some(1000.0)).unwrap();
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].probe, "slow");
    }
}
