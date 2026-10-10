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
    /// Full `HEAD` hash at run start; `None` before v6 or outside git.
    pub commit_sha: Option<String>,
    pub dirty: Option<bool>,
    /// The isolation pass's note (v7), attached after the run was recorded.
    pub diagnosis: Option<String>,
    /// The harness's `run_error` record as stored JSON (v8).
    pub run_error: Option<String>,
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
    /// `false` for a row migrated from the typed schema: its diagnostics
    /// below were never kept, so NULL there means "not retained".
    pub from_harness: bool,
    pub boundary_anchor: Option<String>,
    /// Whether the armed anchor granted any discount (an armed anchor that
    /// granted nothing is decorative).
    pub anchor_consumed: Option<bool>,
    pub ts_entry: ShiftCensus,
    pub ts_exit: ShiftCensus,
}

/// One side of the timestamp-shift census: shifted of considered, and the
/// harness's share, which it omits when nothing was comparable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShiftCensus {
    pub considered: Option<i64>,
    pub shifted: Option<i64>,
    pub share_pct: Option<f64>,
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
        commit_sha: row.get("commit_sha")?,
        dirty: row.get::<_, Option<i64>>("dirty")?.map(|v| v != 0),
        diagnosis: row.get("diagnosis")?,
        run_error: row.get("run_error")?,
    })
}

const RUN_COLS: &str = "\
run_id, started_at, selector, gated, result, fail_reason, harness_exit_code, probe_count, \
commit_sha, dirty, diagnosis, run_error";

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

/// Every queryable `disposition` column (`run_id` excluded, as for
/// [`TRADE_DIFF_COLUMNS`]): brokkr's annotations, the harness projections
/// `schema.rs` generates from the stored record, and the record itself. The
/// `--dispositions` allow-list; a test holds it to the table's real columns.
pub const DISPOSITION_COLUMNS: &[&str] = &[
    "probe",
    "disposition",
    "expected",
    "gate_ok",
    "raw_source",
    "outcome",
    "matched",
    "ours_only",
    "tv_only",
    "boundary_ours",
    "boundary_tv",
    "count_tier",
    "acc_tier",
    "acc_profile",
    "acc_failing",
    "p90_entry",
    "p90_exit",
    "p90_pnl",
    "sig_domain",
    "sig_leg",
    "sig_dimension",
    "sig_detail",
    "sig_breaches",
    "error",
    "runtime_ms",
    "boundary_anchor",
    "anchor_consumed",
    "rule_start_ours",
    "rule_tail_ours",
    "rule_start_tv",
    "rule_end_tv",
    "clipped_ours",
    "clipped_tv",
    "history_prefix_bars",
    "oracle_trimmed",
    "oracle_realtime",
    "ts_entry_considered",
    "ts_entry_shifted",
    "ts_entry_share_pct",
    "ts_exit_considered",
    "ts_exit_shifted",
    "ts_exit_share_pct",
    "window_sensitive",
    "dynamic_builtin_calls",
    "raw_json",
];

/// The curated default projection for `--dispositions`: the window-edge
/// diagnostics the run-detail view does not show. The anchor rides with
/// `anchor_consumed` because an armed anchor that granted nothing is
/// decorative, and each timestamp-shift share rides with its counts because
/// 1/1 and 100/100 are both 100 percent. `raw_source` says whether a NULL means
/// "the harness did not report it" or "this row predates storing it".
pub const DEFAULT_DISPOSITION_COLUMNS: &[&str] = &[
    "probe",
    "disposition",
    "gate_ok",
    "boundary_ours",
    "boundary_tv",
    "boundary_anchor",
    "anchor_consumed",
    "clipped_ours",
    "clipped_tv",
    "ts_entry_shifted",
    "ts_entry_considered",
    "ts_entry_share_pct",
    "ts_exit_shifted",
    "ts_exit_considered",
    "ts_exit_share_pct",
    "raw_source",
];

/// A table `corpus-results` can shape with `--columns`/`--where`.
#[derive(Clone, Copy, Debug)]
pub enum Shaped {
    /// `--diffs`: `trade_diff`, ordered by probe then trade.
    Diffs,
    /// `--dispositions`: `disposition`, ordered by probe.
    Dispositions,
}

impl Shaped {
    fn table(self) -> &'static str {
        match self {
            Shaped::Diffs => "trade_diff",
            Shaped::Dispositions => "disposition",
        }
    }

    fn columns(self) -> &'static [&'static str] {
        match self {
            Shaped::Diffs => TRADE_DIFF_COLUMNS,
            Shaped::Dispositions => DISPOSITION_COLUMNS,
        }
    }

    fn defaults(self) -> &'static [&'static str] {
        match self {
            Shaped::Diffs => DEFAULT_DIFF_COLUMNS,
            Shaped::Dispositions => DEFAULT_DISPOSITION_COLUMNS,
        }
    }

    fn order(self) -> &'static str {
        match self {
            Shaped::Diffs => "probe, our_index",
            Shaped::Dispositions => "probe",
        }
    }
}

/// Resolve a `--columns` request for `table` into a validated SELECT list.
/// Empty -> the table's curated default; the lone token `all` -> every
/// column; otherwise each name must be a known column of that table. An
/// unknown name errors with the full valid set - that error *is* the
/// column-discovery path, which is why there is no separate `--list-columns`.
pub fn resolve_columns(table: Shaped, requested: &[String]) -> Result<Vec<String>, DevError> {
    let owned = |cols: &[&str]| cols.iter().map(|s| (*s).to_owned()).collect();
    if requested.is_empty() {
        return Ok(owned(table.defaults()));
    }
    if requested.len() == 1 && requested[0] == "all" {
        return Ok(owned(table.columns()));
    }
    let mut out = Vec::with_capacity(requested.len());
    for c in requested {
        if c == "all" {
            return Err(DevError::Config(
                "corpus-results --columns: 'all' selects every column and must stand alone, \
                 not be mixed with named columns"
                    .to_owned(),
            ));
        }
        if !table.columns().contains(&c.as_str()) {
            return Err(DevError::Config(format!(
                "corpus-results --columns: unknown {} column '{c}'.{}\nValid columns:\n  {}",
                table.table(),
                column_hint(table, c),
                table.columns().join(", ")
            )));
        }
        out.push(c.clone());
    }
    Ok(out)
}

/// The near misses for an unknown `--columns` name: the valid columns that
/// contain it. `tier` is the case this exists for - the acceptance tier the
/// gate compares is stored as `disposition`, and `count_tier`/`acc_tier`
/// both contain the word, so the guess lands on none of them.
fn column_hint(table: Shaped, unknown: &str) -> String {
    let tier_guess = matches!(table, Shaped::Dispositions) && unknown == "tier";
    let near: Vec<&str> = tier_guess
        .then_some("disposition")
        .into_iter()
        .chain(
            table
                .columns()
                .iter()
                .copied()
                .filter(|col| !unknown.is_empty() && col.contains(unknown)),
        )
        .collect();
    if near.is_empty() {
        return String::new();
    }
    let mut hint = format!(" Did you mean: {}?", near.join(", "));
    if tier_guess {
        hint.push_str(
            " (`disposition` is the acceptance tier the gate compares; `count_tier` is \
             the diagnostic exact/near/drift count tier)",
        );
    }
    hint
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
        let sql = format!("SELECT {RUN_COLS} FROM run ORDER BY run_id DESC LIMIT ?1");
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([clamp(limit)], run_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One run's envelope, `None` when no run has that id.
    pub fn run(&self, run_id: i64) -> Result<Option<RunRow>, DevError> {
        let sql = format!("SELECT {RUN_COLS} FROM run WHERE run_id = ?1");
        let mut stmt = self.conn().prepare(&sql)?;
        let mut rows = stmt.query_map([run_id], run_row)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
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
                    d.p90_exit AS p90_exit, d.raw_source AS raw_source, \
                    d.boundary_anchor AS boundary_anchor, d.anchor_consumed AS anchor_consumed, \
                    d.ts_entry_considered, d.ts_entry_shifted, d.ts_entry_share_pct, \
                    d.ts_exit_considered, d.ts_exit_shifted, d.ts_exit_share_pct \
             FROM disposition d JOIN run r ON r.run_id = d.run_id \
             WHERE d.probe = ?1 ORDER BY d.run_id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![probe, clamp(limit)], |r| {
            let census = |side: &str| -> rusqlite::Result<ShiftCensus> {
                Ok(ShiftCensus {
                    considered: r.get(format!("ts_{side}_considered").as_str())?,
                    shifted: r.get(format!("ts_{side}_shifted").as_str())?,
                    share_pct: r.get(format!("ts_{side}_share_pct").as_str())?,
                })
            };
            Ok(TrendRow {
                from_harness: r.get::<_, String>("raw_source")? == "harness",
                boundary_anchor: r.get("boundary_anchor")?,
                anchor_consumed: r.get::<_, Option<i64>>("anchor_consumed")?.map(|v| v != 0),
                ts_entry: census("entry")?,
                ts_exit: census("exit")?,
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

    /// `table`'s rows for a run, narrowed to a `probes` set (empty = all
    /// probes in the run), projected onto `columns`, optionally further filtered
    /// by a raw boolean expression. `columns` must come from
    /// [`resolve_columns`] for the same table - it is interpolated, so the
    /// allow-list is the only thing standing between projection and SQL
    /// injection; the probe set and `run_id` are bound, and `where_expr` is
    /// trusted local input against a read-only connection.
    pub fn shaped(
        &self,
        table: Shaped,
        run_id: i64,
        probes: &[String],
        columns: &[String],
        where_expr: Option<&str>,
    ) -> Result<RawTable, DevError> {
        let select = columns.join(", ");
        let mut sql = format!("SELECT {select} FROM {} WHERE run_id = ?1", table.table());
        if !probes.is_empty() {
            // probe IN (?2, ?3, ...) - the ids are bound, never interpolated.
            let placeholders: Vec<String> =
                (0..probes.len()).map(|i| format!("?{}", i + 2)).collect();
            sql.push_str(&format!(" AND probe IN ({})", placeholders.join(", ")));
        }
        if let Some(expr) = where_expr {
            sql.push_str(&format!(" AND ({expr})"));
        }
        sql.push_str(&format!(" ORDER BY {}", table.order()));
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
        // One pass with a window, not a correlated subquery per row:
        // `runtime_ms` is projected from the stored record, so every
        // evaluation parses JSON, and the store grows without bound.
        let mut sql = String::from(
            "SELECT probe, runtime_ms, run_id FROM ( \
                 SELECT probe, runtime_ms, run_id, ROW_NUMBER() OVER \
                     (PARTITION BY probe ORDER BY run_id DESC) AS newest \
                 FROM disposition WHERE runtime_ms IS NOT NULL) \
             WHERE newest = 1",
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
    ///
    /// Only a run that actually did the work it was asked to is a valid bound,
    /// so a covering run must also be **comparable**: the harness completed
    /// (exit 0, or 1 - a break is a finished probe, not an abort), it broke no
    /// report or stream integrity (`protocol_violations` 0, which also counts
    /// a truncated stdout; a row predating v7 has none recorded and is judged
    /// on coverage alone), **every id it selected has a valid stored
    /// disposition** (a row whose stored outcome and tier pass the shared
    /// validator `report::valid_label` and agree with its label; and none is
    /// a `harness_abort`, a valid gate disposition but no timing sample -
    /// exact id coverage read from the disposition rows, so report-only extras
    /// count for nothing and a line count can never stand in for a missing
    /// probe), it ran with no forwarded harness flags, and it was built in the
    /// same profile (`debug`; a row predating the recorded profile counts as
    /// debug, the parity default). Without these, one fast-failing `--all` run
    /// (a harness error at startup, say) would bound every later selection
    /// at a second or two and silently disable the ceiling.
    pub fn estimated_wall_ms(
        &self,
        selection: &[String],
        debug: bool,
    ) -> Result<Option<f64>, DevError> {
        let want: std::collections::HashSet<&str> =
            selection.iter().map(String::as_str).collect();
        // Newest first; stop at the first comparable run whose id-set covers
        // the selection. The most recent run is often `--all` (covers
        // everything), so this typically returns on the first rows.
        let mut stmt = self.conn().prepare(
            "SELECT run_id, selector, wall_ms FROM run \
             WHERE wall_ms IS NOT NULL AND harness_exit_code IN (0, 1) \
             AND COALESCE(protocol_violations, 0) = 0 \
             ORDER BY run_id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })?;
        for row in rows {
            let (run_id, selector, wall_ms) = row?;
            if selection_covered(&selector, &want)
                && comparable_run(&selector, debug)
                && self.fully_reported(run_id, &selector)?
            {
                return Ok(Some(wall_ms));
            }
        }
        Ok(None)
    }

    /// Is the run a usable timing sample over its selection? Every id it
    /// selected (its `selector` `ids`) has a stored disposition whose label is
    /// one the gate can use, read from the disposition rows (so an extra line
    /// for an unselected probe covers nothing), and no row is a
    /// `harness_abort` - a valid gate disposition, but a crashed probe makes
    /// the wall describe a different workload.
    fn fully_reported(&self, run_id: i64, selector: &str) -> Result<bool, DevError> {
        let Some(asked) = selector_ids(selector) else {
            return Ok(false);
        };
        // The stored outcome and tier must form a valid disposition under the
        // shared validator (`report::valid_label`), and agree with the label
        // stored beside them.
        let mut stmt = self.conn().prepare(
            "SELECT probe, disposition, outcome, acc_tier, raw_json FROM disposition \
             WHERE run_id = ?1",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut valid: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in rows {
            let (probe, disposition, outcome, tier, raw_json) = row?;
            // The stored record is re-validated with the record-level
            // validator: a harness `disposition` field that contradicts the
            // derived label makes the record no evidence, even on a row
            // stored before brokkr checked it. A missing field is fine (and
            // a row rebuilt by the v5 migration has none).
            let declared = serde_json::from_str::<serde_json::Value>(&raw_json)
                .ok()
                .and_then(|v| v.get("disposition").cloned());
            // A valid gate disposition is not necessarily a usable timing
            // sample: a run in which a probe process died ran a different
            // workload than one in which it finished.
            if disposition == crate::piners::report::HARNESS_ABORT {
                return Ok(false);
            }
            let label = outcome.as_deref().and_then(|o| {
                crate::piners::report::record_label(o, tier.as_deref(), declared.as_ref()).ok()
            });
            if label == Some(disposition.as_str()) {
                valid.insert(probe);
            }
        }
        Ok(asked.iter().all(|id| valid.contains(id)))
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
    let Some(have) = selector_ids(selector) else {
        return false;
    };
    let have: std::collections::HashSet<&str> = have.iter().map(String::as_str).collect();
    want.iter().all(|id| have.contains(id))
}

/// The resolved ids a stored `selector` JSON names, `None` when it does not
/// parse or carries no `ids` array.
fn selector_ids(selector: &str) -> Option<Vec<String>> {
    let value = serde_json::from_str::<serde_json::Value>(selector).ok()?;
    let ids = value.get("ids")?.as_array()?;
    Some(ids.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect())
}

/// Is the run whose stored `selector` JSON is `selector` a valid wall basis
/// for a run built in profile `debug`, as far as its selector says? See
/// [`CorpusDb::estimated_wall_ms`]: no forwarded harness flags and the same
/// profile (absent = debug). Coverage of its selection is checked separately
/// against the stored dispositions. Unparsable = not comparable.
fn comparable_run(selector: &str, debug: bool) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(selector) else {
        return false;
    };
    let perturbed = value
        .get("harness_args")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let run_debug = value
        .get("debug")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    !perturbed && run_debug == debug
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
        record_exit(db, selector, wall_ms, Some(0), result, nd);
    }

    /// [`record_full`] with an explicit harness exit code.
    fn record_exit(
        db: &CorpusDb,
        selector: &str,
        wall_ms: Option<f64>,
        exit: Option<i32>,
        result: &str,
        nd: &[u8],
    ) {
        record_protocol(db, selector, wall_ms, exit, result, nd, Some(0));
    }

    /// [`record_exit`] with an explicit protocol-violation count (`None` =
    /// a row predating v7).
    fn record_protocol(
        db: &CorpusDb,
        selector: &str,
        wall_ms: Option<f64>,
        exit: Option<i32>,
        result: &str,
        nd: &[u8],
        protocol_violations: Option<i64>,
    ) {
        let report = parse(nd);
        let run = crate::piners::corpus_db::RunRecord {
            run_id: None,
            started_at: None,
            commit_sha: None,
            dirty: None,
            selector,
            gated: true,
            result,
            fail_reason: None,
            harness_exit_code: exit,
            stderr: "",
            wall_ms,
            protocol_violations,
            run_error: None,
        };
        db.record_run(&run, &report, &BTreeMap::new(), &[]).unwrap();
    }

    /// One valid disposition line per id - a run that finished every probe it
    /// was given, which is what makes it a valid wall basis.
    fn lines(ids: &[&str]) -> Vec<u8> {
        ids.iter()
            .map(|id| {
                format!(
                    "{{\"probe\":\"{id}\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}}}}\n"
                )
            })
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn estimated_wall_needs_exact_valid_coverage_and_a_clean_protocol() {
        let db = CorpusDb::open_in_memory().unwrap();
        let sel = ["a".to_owned(), "b".to_owned()];
        // The one real basis.
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(50_000.0), "pass", &lines(&["a", "b"]));
        // Two lines, enough for a line count - but `b` is an extra under a
        // different selection's id, and the selected `b` never reported.
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(1_000.0), "fail", &lines(&["a", "zz"]));
        // A line for `b` with no usable label is not a disposition.
        let mut half_valid = lines(&["a"]);
        half_valid.extend_from_slice(b"{\"probe\":\"b\",\"outcome\":\"parity\"}\n");
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(1_100.0), "fail", &half_valid);
        // Nor is a tier posing as an outcome, though its stored label reads
        // like a gate label.
        let mut posing = lines(&["a"]);
        posing.extend_from_slice(b"{\"probe\":\"b\",\"outcome\":\"accepted\"}\n");
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(1_150.0), "fail", &posing);
        // A record whose own disposition field contradicts its outcome/tier,
        // stored with a clean count (as a row predating the check would be):
        // re-validated from raw_json, it is no evidence.
        let mut contradicted = lines(&["a"]);
        contradicted.extend_from_slice(
            b"{\"probe\":\"b\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"},\"disposition\":\"byte_exact\"}\n",
        );
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(1_175.0), "pass", &contradicted);
        // Complete, but it broke protocol (an extra line, say).
        record_protocol(
            &db,
            r#"{"ids":["a","b"]}"#,
            Some(1_200.0),
            Some(0),
            "fail",
            &lines(&["a", "b", "zz"]),
            Some(1),
        );
        assert_eq!(db.estimated_wall_ms(&sel, true).unwrap(), Some(50_000.0));
        // A clean, complete exit-1 run with a pinned crash: valid gate
        // dispositions, but no timing sample.
        let mut crashed = lines(&["a"]);
        crashed.extend_from_slice(b"{\"probe\":\"b\",\"outcome\":\"harness_abort\",\"error\":\"signal 11\"}\n");
        record_exit(&db, r#"{"ids":["a","b"]}"#, Some(1_250.0), Some(1), "pass", &crashed);
        assert_eq!(db.estimated_wall_ms(&sel, true).unwrap(), Some(50_000.0));
        // A pre-v7 row (no protocol count) with full coverage still counts,
        // and its extra line does not hurt it.
        record_protocol(
            &db,
            r#"{"ids":["a","b"]}"#,
            Some(40_000.0),
            Some(0),
            "pass",
            &lines(&["a", "b", "zz"]),
            None,
        );
        assert_eq!(db.estimated_wall_ms(&sel, true).unwrap(), Some(40_000.0));
    }

    #[test]
    fn estimated_wall_uses_the_most_recent_superset_runs_measured_wall() {
        let db = CorpusDb::open_in_memory().unwrap();
        // Run 1: an `--all`-style full run over [a,b,c], wall 60s.
        record_full(&db, r#"{"ids":["a","b","c"]}"#, Some(60_000.0), "pass", &lines(&["a", "b", "c"]));
        // Run 2: a smaller slice [a], wall 5s.
        record_full(&db, r#"{"ids":["a"]}"#, Some(5_000.0), "pass", &lines(&["a"]));

        // Selecting {a,b}: run 2 ([a]) does NOT cover it; run 1 ([a,b,c]) does,
        // so the estimate is run 1's real 60s wall - the valid upper bound.
        let est = db
            .estimated_wall_ms(&["a".to_owned(), "b".to_owned()], true)
            .unwrap();
        assert_eq!(est, Some(60_000.0));

        // Selecting {a}: the newest covering run wins - run 2's 5s, not run 1's.
        assert_eq!(db.estimated_wall_ms(&["a".to_owned()], true).unwrap(), Some(5_000.0));
    }

    #[test]
    fn estimated_wall_is_none_when_no_run_covers_the_selection() {
        let db = CorpusDb::open_in_memory().unwrap();
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(10_000.0), "pass", &lines(&["a", "b"]));

        // `z` is not in any recorded run's selection -> no covering run -> None
        // (the caller reads this as "no measured basis, don't refuse").
        assert_eq!(db.estimated_wall_ms(&["z".to_owned()], true).unwrap(), None);
        // A partial overlap still isn't coverage: {a,z} needs BOTH in one run.
        assert_eq!(
            db.estimated_wall_ms(&["a".to_owned(), "z".to_owned()], true).unwrap(),
            None
        );
    }

    #[test]
    fn estimated_wall_skips_runs_with_no_measured_wall() {
        let db = CorpusDb::open_in_memory().unwrap();
        // Newest covering run has NULL wall (e.g. a spawn failure) -> skipped;
        // the older covering run with a real wall is used.
        record_full(&db, r#"{"ids":["a"]}"#, Some(8_000.0), "pass", &lines(&["a"]));
        record_full(&db, r#"{"ids":["a"]}"#, None, "fail", &lines(&["a"]));
        assert_eq!(db.estimated_wall_ms(&["a".to_owned()], true).unwrap(), Some(8_000.0));
    }

    #[test]
    fn estimated_wall_skips_runs_that_are_not_comparable() {
        let db = CorpusDb::open_in_memory().unwrap();
        // The one real basis: a complete debug run, 90s.
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(90_000.0), "pass", &lines(&["a", "b"]));
        // Newer, all covering, all fast, none a valid bound:
        // a harness error at startup (exit 2, no lines),
        record_exit(&db, r#"{"ids":["a","b"]}"#, Some(900.0), Some(2), "fail", b"");
        // a run that stopped emitting after one probe,
        record_full(&db, r#"{"ids":["a","b"]}"#, Some(1_000.0), "fail", &lines(&["a"]));
        // a run perturbed by forwarded harness flags,
        record_full(
            &db,
            r#"{"ids":["a","b"],"harness_args":["--fast"]}"#,
            Some(1_100.0),
            "pass",
            &lines(&["a", "b"]),
        );
        // and a release build.
        record_full(&db, r#"{"ids":["a","b"],"debug":false}"#, Some(1_200.0), "pass", &lines(&["a", "b"]));

        let sel = ["a".to_owned(), "b".to_owned()];
        assert_eq!(db.estimated_wall_ms(&sel, true).unwrap(), Some(90_000.0));
        // A release selection is bounded by the release run only; the
        // profile-less legacy row reads as debug.
        assert_eq!(db.estimated_wall_ms(&sel, false).unwrap(), Some(1_200.0));
        // Exit 1 (breaks) is a finished run and still counts.
        record_exit(&db, r#"{"ids":["a","b"]}"#, Some(70_000.0), Some(1), "fail", &lines(&["a", "b"]));
        assert_eq!(db.estimated_wall_ms(&sel, true).unwrap(), Some(70_000.0));
    }

    fn owned(cols: &[&str]) -> Vec<String> {
        cols.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn resolve_columns_empty_is_the_curated_default_with_qty() {
        let cols = resolve_columns(Shaped::Diffs, &[]).unwrap();
        assert_eq!(cols, owned(DEFAULT_DIFF_COLUMNS));
        // The whole point of the change: qty is in the default.
        assert!(cols.iter().any(|c| c == "our_qty"));
        assert!(cols.iter().any(|c| c == "tv_entry_qty"));
    }

    #[test]
    fn resolve_columns_all_is_every_column() {
        assert_eq!(resolve_columns(Shaped::Diffs, &owned(&["all"])).unwrap(), owned(TRADE_DIFF_COLUMNS));
    }

    #[test]
    fn resolve_columns_validates_and_preserves_order() {
        let req = owned(&["our_qty", "tv_entry_qty", "our_pnl"]);
        assert_eq!(resolve_columns(Shaped::Diffs, &req).unwrap(), req);
        // Unknown name errors (and the message lists the valid set - the
        // discovery path that stands in for --list-columns).
        let err = resolve_columns(Shaped::Diffs, &owned(&["our_qty", "bogus"])).unwrap_err();
        assert!(err.to_string().contains("bogus"));
        assert!(err.to_string().contains("our_qty"));
        // The hint names the command that owns the flag.
        assert!(err.to_string().contains("corpus-results --columns"));
        // `all` mixed with names is rejected (it means "everything", alone).
        assert!(resolve_columns(Shaped::Diffs, &owned(&["all", "our_qty"])).is_err());
    }

    #[test]
    fn the_disposition_allow_list_is_exactly_the_tables_columns() {
        // Generated columns are hidden from table_info; table_xinfo lists
        // them. A projection added to the schema without an allow-list entry
        // (or the reverse) fails here, not at a user's --columns.
        let db = CorpusDb::open_in_memory().unwrap();
        let t = db
            .raw_sql("SELECT name FROM pragma_table_xinfo('disposition') WHERE name <> 'run_id'")
            .unwrap();
        let mut table: Vec<String> = t.rows.into_iter().map(|r| r[0].clone()).collect();
        let mut listed = owned(DISPOSITION_COLUMNS);
        table.sort();
        listed.sort();
        assert_eq!(table, listed);
        for c in DEFAULT_DISPOSITION_COLUMNS {
            assert!(DISPOSITION_COLUMNS.contains(c), "{c}");
        }
    }

    #[test]
    fn dispositions_table_projects_and_filters_the_diagnostics() {
        let db = CorpusDb::open_in_memory().unwrap();
        record(
            &db,
            "pass",
            br#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"},"boundary_anchor":"carry_tv","ts_shift":{"entry_considered":4,"entry_shifted":3,"entry_share_pct":75.0,"exit_considered":4,"exit_shifted":0,"exit_share_pct":0.0}}
{"probe":"b","outcome":"parity","acceptance":{"tier":"byte_exact"},"ts_shift":{"entry_considered":9,"entry_shifted":0,"entry_share_pct":0.0,"exit_considered":9,"exit_shifted":0,"exit_share_pct":0.0}}
"#,
        );
        let cols = resolve_columns(Shaped::Dispositions, &owned(&["probe", "boundary_anchor"]))
            .unwrap();
        let t = db
            .shaped(Shaped::Dispositions, 1, &[], &cols, Some("ts_entry_share_pct >= 50"))
            .unwrap();
        assert_eq!(t.rows, vec![vec!["a".to_owned(), "carry_tv".to_owned()]]);
        let err = resolve_columns(Shaped::Dispositions, &owned(&["our_qty"])).unwrap_err();
        assert!(err.to_string().contains("unknown disposition column 'our_qty'"));
        // The canned views' old `tier` header sent agents here first.
        let err = resolve_columns(Shaped::Dispositions, &owned(&["tier"])).unwrap_err();
        assert!(
            err.to_string().contains("Did you mean: disposition, count_tier, acc_tier?"),
            "{err}"
        );
    }

    #[test]
    fn trend_carries_the_anchor_and_shift_census() {
        let db = CorpusDb::open_in_memory().unwrap();
        record(
            &db,
            "pass",
            br#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"},"boundary_anchor":"carry_tv","boundary_rules":{"start_ours":0,"tail_ours":0,"start_tv":1,"end_tv":0,"start_anchor_consumed":true},"ts_shift":{"entry_considered":2,"entry_shifted":1,"entry_share_pct":50.0,"exit_considered":0,"exit_shifted":0}}
"#,
        );
        let rows = db.trend_for_probe("a", 5).unwrap();
        assert_eq!(rows.len(), 1);
        let t = &rows[0];
        assert!(t.from_harness);
        assert_eq!(t.boundary_anchor.as_deref(), Some("carry_tv"));
        assert_eq!(t.anchor_consumed, Some(true));
        assert_eq!(
            t.ts_entry,
            ShiftCensus {
                considered: Some(2),
                shifted: Some(1),
                share_pct: Some(50.0)
            }
        );
        // Nothing comparable: the harness sends no share, and none is invented.
        assert_eq!(t.ts_exit.share_pct, None);
        let table = crate::piners::corpus_db::trend_table(&rows);
        assert!(table.contains("carry_tv+"), "{table}");
        assert!(table.contains("1/2 50%"), "{table}");
        assert!(table.contains("0/0"), "{table}");
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
