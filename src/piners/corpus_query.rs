//! `brokkr corpus-results`, project-gated to piners.
//!
//! Queries the corpus run store (`.brokkr/piners/corpus/runs.db`, written by
//! `brokkr corpus`). This is its own command rather than a mode of `brokkr
//! results`: piners now records hotpath/alloc benchmarks into the shared
//! `results.db` like every other project, so `results` keeps its benchmark
//! meaning and the corpus store gets a dedicated surface (no overloaded
//! struct, no benchmark filters to reject).
//!
//! Flag -> view:
//! - bare                  -> table of recent runs
//! - `<id>` / `--run N`    -> that run's per-probe dispositions (+ gate misses)
//! - `--probe X`           -> X's disposition + its `trade_diff` rows (combo)
//! - `--diffs [--probe …]  -> `trade_diff` table across the run, optionally
//!    [--columns …]            narrowed to a probe set, projected onto columns
//!    [--where E]`             (`all` => every column, vertical), and/or filtered
//! - `--dispositions …`    -> the same shaping over `disposition`, including
//!   the harness diagnostics the run-detail view omits
//! - `--runtimes [--over S]` -> per-probe most-recent runtime, slowest first
//! - `--trend X`           -> X's disposition/count_tier/p90, anchor and shift
//!   census over recent runs
//! - `--compare A B`       -> per-probe count/tier/disposition moves between
//!   two runs (see [`super::corpus_db::compare`])
//! - `--sql Q`             -> read-only `SELECT`/`WITH` escape hatch
//!
//! The canned views are `?N`-parameterized; `--columns` interpolates only
//! allow-listed identifiers (see [`super::corpus_db::query::resolve_columns`]),
//! while `--where`/`--sql` interpolate trusted local SQL - all guarded by the
//! read-only DB open plus a SELECT-only UX check (see [`super::corpus_db`]).

use std::path::Path;

use crate::error::DevError;
use crate::output;
use crate::piners::corpus_db::query::DispositionRow;
use crate::piners::corpus_db::{self, CorpusDb, Shaped};
use crate::request::CorpusQuery;
use crate::resolve::corpus_runs_db_path;

pub fn cmd(project_root: &Path, q: &CorpusQuery) -> Result<(), DevError> {
    validate_query(q)?;

    let db_path = corpus_runs_db_path(project_root);
    if !db_path.exists() {
        // A view naming a run (compare, `<id>`, `--run`) asked for one that
        // cannot exist; only the "latest"/listing views have nothing to show.
        if let Some(id) = q.compare.map(|(a, _)| a).or(q.run).or(q.run_id) {
            return Err(no_run(id));
        }
        output::result_msg("no corpus runs yet (run `brokkr corpus ...` first)");
        return Ok(());
    }
    let db = CorpusDb::open_readonly(&db_path)?;

    // --compare A B: per-probe count moves, B read against A. clap refuses
    // every other view flag beside it.
    if let Some((a, b)) = q.compare {
        let comparison = db.compare_runs(a, b)?;
        println!("{}", corpus_db::compare_report(&comparison, q.full));
        return Ok(());
    }

    // --sql escape hatch (read-only, SELECT/WITH only).
    if let Some(sql) = &q.sql {
        guard_sql(sql, "--sql", true)?;
        let table = db.raw_sql(sql)?;
        println!("{}", corpus_db::raw_table(&table));
        return Ok(());
    }

    // --runtimes [--over SECS]: per-probe most-recent runtime, slowest first.
    // A diagnostic (which probe is heavy) - probes overlap in the harness, so
    // the per-probe sum is not the run wall.
    if q.runtimes {
        let over_ms = q.over.map(|s| s * 1000.0);
        let rows = db.runtimes(over_ms)?;
        println!("{}", corpus_db::runtimes_table(&rows));
        return Ok(());
    }

    // --trend <probe> over recent runs.
    if let Some(probe) = &q.trend {
        let rows = db.trend_for_probe(probe, q.limit)?;
        println!("{}", corpus_db::trend_table(&rows));
        return Ok(());
    }

    let shaped = if q.diffs {
        Some(Shaped::Diffs)
    } else if q.dispositions {
        Some(Shaped::Dispositions)
    } else {
        None
    };

    // --diffs / --dispositions [--probe ...] [--columns ...] [--where E]: a
    // shapeable table across the selected/latest run. `--probe` here is an
    // IN-list filter, not the combo view.
    if let Some(shape) = shaped {
        let Some(run_id) = resolve_run(&db, q)? else {
            output::result_msg("no corpus runs to filter");
            return Ok(());
        };
        let columns = corpus_db::resolve_columns(shape, &q.columns)?;
        let vertical = q.columns.len() == 1 && q.columns[0] == "all";
        let where_expr = match &q.where_expr {
            Some(e) => {
                guard_sql(e, "--where", false)?;
                Some(e.as_str())
            }
            None => None,
        };
        let table = db.shaped(shape, run_id, &q.probe, &columns, where_expr)?;
        println!("run {run_id}");
        if vertical {
            println!("{}", corpus_db::raw_records(&table));
        } else {
            println!("{}", corpus_db::raw_table(&table));
        }
        return Ok(());
    }

    // --probe <id>: one probe's combo view (disposition + its trade_diff rows).
    // Multiple probes only make sense as the diff-table filter above.
    if !q.probe.is_empty() {
        if q.probe.len() > 1 {
            return Err(DevError::Config(
                "corpus-results: multiple --probe needs --diffs or --dispositions (the \
                 multi-probe tables); a bare --probe shows one probe's full combo view"
                    .to_owned(),
            ));
        }
        let Some(run_id) = resolve_run(&db, q)? else {
            output::result_msg("no corpus runs recorded");
            return Ok(());
        };
        return render_probe(&db, run_id, &q.probe[0]);
    }

    // Bare positional run id or --run N: that run's per-probe detail.
    if let Some(run_id) = explicit_run_id(q)? {
        return render_run_detail(&db, run_id, q.full);
    }

    // Bare `brokkr corpus-results`: the recent-runs table.
    let rows = db.recent_runs(q.limit)?;
    println!("{}", corpus_db::runs_table(&rows));
    Ok(())
}

/// The run id named explicitly via `--run` or the bare positional argument.
/// A non-numeric positional is a clear error (it's not a probe selector).
fn explicit_run_id(q: &CorpusQuery) -> Result<Option<i64>, DevError> {
    // `--run` and the bare positional are equivalent; prefer the flag. clap
    // already rejects a non-numeric positional, so there is no parse path here.
    Ok(q.run.or(q.run_id))
}

/// Reject options that would otherwise be accepted and silently ignored by a
/// higher-priority corpus-results view.
fn validate_query(q: &CorpusQuery) -> Result<(), DevError> {
    let shaped = q.diffs || q.dispositions;

    if q.over.is_some() && !q.runtimes {
        return Err(DevError::Config(
            "corpus-results --over requires --runtimes.".to_owned(),
        ));
    }
    // `--columns` and `--where` shape either table view. In particular,
    // `--columns` with --dispositions is supported even without --diffs.
    if !shaped && (!q.columns.is_empty() || q.where_expr.is_some()) {
        return Err(DevError::Config(
            "corpus-results --columns/--where shape the --diffs or --dispositions table - \
             add one (e.g. `--diffs --probe <id> --columns our_qty,tv_entry_qty`)"
                .to_owned(),
        ));
    }

    let sql_conflict = q.run_id.is_some()
        || q.run.is_some()
        || !q.probe.is_empty()
        || q.diffs
        || q.dispositions
        || !q.columns.is_empty()
        || q.runtimes
        || q.over.is_some()
        || q.trend.is_some()
        || q.where_expr.is_some()
        || q.full
        || q.compare.is_some();
    if q.sql.is_some() && sql_conflict {
        return Err(DevError::Config(
            "corpus-results --sql is a standalone view and cannot be combined with other view flags."
                .to_owned(),
        ));
    }

    let trend_conflict = q.run_id.is_some()
        || q.run.is_some()
        || !q.probe.is_empty()
        || q.diffs
        || q.dispositions
        || !q.columns.is_empty()
        || q.runtimes
        || q.over.is_some()
        || q.where_expr.is_some()
        || q.full
        || q.compare.is_some();
    if q.trend.is_some() && trend_conflict {
        return Err(DevError::Config(
            "corpus-results --trend is a standalone view and cannot be combined with other view flags."
                .to_owned(),
        ));
    }

    let runtimes_conflict = q.run_id.is_some()
        || q.run.is_some()
        || !q.probe.is_empty()
        || q.diffs
        || q.dispositions
        || !q.columns.is_empty()
        || q.where_expr.is_some()
        || q.full
        || q.compare.is_some();
    if q.runtimes && runtimes_conflict {
        return Err(DevError::Config(
            "corpus-results --runtimes cannot be combined with run-detail, probe, or table-view flags."
                .to_owned(),
        ));
    }

    if shaped && (q.full || q.compare.is_some()) {
        return Err(DevError::Config(
            "corpus-results --full/--compare cannot be combined with --diffs or --dispositions."
                .to_owned(),
        ));
    }
    if !q.probe.is_empty() && !shaped && q.full {
        return Err(DevError::Config(
            "corpus-results --full applies to run detail, not the single-probe combo view."
                .to_owned(),
        ));
    }
    if q.full && q.run_id.is_none() && q.run.is_none() && q.compare.is_none() {
        return Err(DevError::Config(
            "corpus-results --full requires a run id (or --compare).".to_owned(),
        ));
    }

    Ok(())
}

/// The explicit run id if given, else the latest recorded run.
fn resolve_run(db: &CorpusDb, q: &CorpusQuery) -> Result<Option<i64>, DevError> {
    if let Some(id) = explicit_run_id(q)? {
        return Ok(Some(id));
    }
    db.latest_run_id()
}

fn no_run(run_id: i64) -> DevError {
    DevError::Config(format!(
        "corpus-results: no run {run_id} in runs.db (bare `brokkr corpus-results` lists the runs)"
    ))
}

fn render_probe(db: &CorpusDb, run_id: i64, probe: &str) -> Result<(), DevError> {
    let run = db.run(run_id)?.ok_or_else(|| no_run(run_id))?;
    match db.disposition_for_probe(run_id, probe)? {
        Some(d) => {
            println!("{}", corpus_db::run_header(&run));
            println!(
                "{}",
                corpus_db::dispositions_table(std::slice::from_ref(&d))
            );
            let diffs = db.trade_diffs_for_probe(run_id, probe)?;
            if diffs.is_empty() {
                output::result_msg(
                    "(no trade_diff rows - probe was byte-exact or divergence-free)",
                );
            } else {
                println!("\ntrade_diff:");
                println!("{}", corpus_db::trade_diffs_table(&diffs));
            }
        }
        None => output::result_msg(&format!("probe '{probe}' not found in run {run_id}")),
    }
    Ok(())
}

/// Whether the run-detail view lists a row without `--full`: a deviation
/// from its pin, or a crash. A `harness_abort` stays visible even when pinned,
/// as in the live rendering, so a crash is never folded into "matching their
/// pin".
fn shown_by_default(d: &DispositionRow) -> bool {
    !d.gate_ok || d.disposition == crate::piners::report::HARNESS_ABORT
}

fn render_run_detail(db: &CorpusDb, run_id: i64, full: bool) -> Result<(), DevError> {
    let run = db.run(run_id)?.ok_or_else(|| no_run(run_id))?;
    println!("{}", corpus_db::run_header(&run));
    if let Some(reason) = &run.fail_reason {
        println!("reason: {reason}");
    }
    // The stored run_error, rendered with every locator it carries.
    if let Some(json) = run.run_error.as_deref() {
        match crate::piners::contract::RunErrorRecord::from_json(json) {
            Some(rec) => println!("harness {}", rec.render()),
            None => println!("harness run_error (unreadable record): {json}"),
        }
    }
    // Evidence from the isolation pass, kept beside (never in place of) the
    // reason. Diagnostic attempts are not stored as dispositions.
    if let Some(diagnosis) = run.diagnosis.as_deref().filter(|d| !d.is_empty()) {
        for line in diagnosis.lines() {
            println!("diagnosis: {line}");
        }
    }
    let disps = db.dispositions_for_run(run_id)?;
    if disps.is_empty() {
        output::result_msg(&format!("run {run_id}: no per-probe dispositions recorded"));
    } else {
        // Default to the deviations - the rows whose stored disposition does
        // not match their pin (`gate_ok = 0`, the `DEVIATES` rows). A run on
        // the full corpus is 200+ probes and most sit exactly on their pin;
        // showing all of them buries the handful that moved. `--full` opts
        // back into the complete table.
        let total = disps.len();
        let shown: Vec<DispositionRow> = if full {
            disps
        } else {
            disps
                .iter()
                .filter(|d| shown_by_default(d))
                .cloned()
                .collect()
        };
        let hidden = total - shown.len();
        if shown.is_empty() {
            output::result_msg(&format!(
                "all {} matching their pin - pass --full to show",
                output::count(total, "probe")
            ));
        } else {
            println!("{}", corpus_db::dispositions_table(&shown));
            if hidden > 0 {
                output::result_msg(&format!(
                    "{} matching their pin (hidden) - pass --full to show",
                    output::count(hidden, "probe")
                ));
            }
        }
    }

    let misses = db.gate_misses_for_run(run_id)?;
    if !misses.is_empty() {
        println!("\ngate misses (selected, no disposition emitted):");
        println!("{}", corpus_db::gate_misses_block(&misses));
    }

    if let Some(stderr) = db.run_stderr(run_id)?
        && !stderr.trim().is_empty()
    {
        println!("\nharness stderr:\n{stderr}");
    }
    Ok(())
}

/// UX guard for the raw-SQL paths. The read-only DB open is the load-bearing
/// safety; this just yields a clean error instead of a SQLite write failure.
/// `require_select` is true for `--sql` (a full query) and false for `--where`
/// (a boolean expression); both reject `;` to block statement stacking.
fn guard_sql(input: &str, flag: &str, require_select: bool) -> Result<(), DevError> {
    if input.contains(';') {
        return Err(DevError::Config(format!(
            "corpus-results {flag}: ';' (statement stacking) is not allowed"
        )));
    }
    if require_select {
        let first = input.split_whitespace().next().unwrap_or("");
        if !first.eq_ignore_ascii_case("select") && !first.eq_ignore_ascii_case("with") {
            return Err(DevError::Config(format!(
                "corpus-results {flag}: only read-only SELECT/WITH queries are allowed"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(disposition: &str, gate_ok: bool) -> DispositionRow {
        DispositionRow {
            probe: "p".to_owned(),
            outcome: disposition.to_owned(),
            disposition: disposition.to_owned(),
            expected: Some(disposition.to_owned()),
            gate_ok,
            matched: 0,
            ours_only: 0,
            tv_only: 0,
            boundary_ours: 0,
            boundary_tv: 0,
            count_tier: None,
            p90_entry: None,
            p90_exit: None,
            p90_pnl: None,
            sig_domain: None,
            sig_dimension: None,
            error: None,
        }
    }

    #[test]
    fn a_pinned_crash_stays_visible_in_the_stored_view() {
        assert!(shown_by_default(&row("harness_abort", true)));
        assert!(shown_by_default(&row("accepted", false)));
        assert!(!shown_by_default(&row("accepted", true)));
        assert!(!shown_by_default(&row("runtime_fail", true)));
    }
}
