//! `brokkr results`, project-gated to piners.
//!
//! piners records no benchmarks, so its `results.db` is always empty and the
//! `results` command is repurposed to query the corpus run store
//! (`.brokkr/piners/corpus/runs.db`, written by `brokkr corpus`). The command
//! is bimodal: the benchmark filters (`--commit`/`--compare`/`--command`/
//! `--mode`/`--dataset`/`--meta`/`--env`/`--grep`) don't apply here and are
//! rejected with a clear error; the piners flags below do.
//!
//! Flag -> view:
//! - bare                  -> table of recent runs
//! - `<id>` / `--run N`    -> that run's per-probe dispositions (+ gate misses)
//! - `--probe X`           -> X's disposition + its `trade_diff` rows (combo)
//! - `--diffs [--probe …]  -> `trade_diff` table across the run, optionally
//!    [--columns …]            narrowed to a probe set, projected onto columns
//!    [--where E]`             (`all` => every column, vertical), and/or filtered
//! - `--runtimes [--over S]` -> per-probe most-recent runtime, slowest first
//! - `--trend X`           -> X's disposition/tier/p90 over recent runs
//! - `--sql Q`             -> read-only `SELECT`/`WITH` escape hatch
//!
//! The canned views are `?N`-parameterized; `--columns` interpolates only
//! allow-listed identifiers (see [`super::corpus_db::query::resolve_diff_columns`]),
//! while `--where`/`--sql` interpolate trusted local SQL - all guarded by the
//! read-only DB open plus a SELECT-only UX check (see [`super::corpus_db`]).

use std::path::Path;

use crate::error::DevError;
use crate::output;
use crate::piners::corpus_db::query::DispositionRow;
use crate::piners::corpus_db::{self, CorpusDb};
use crate::request::ResultsQuery;
use crate::resolve::corpus_runs_db_path;

pub fn cmd(project_root: &Path, q: &ResultsQuery) -> Result<(), DevError> {
    reject_bench_flags(q)?;

    let db_path = corpus_runs_db_path(project_root);
    if !db_path.exists() {
        output::result_msg("no corpus runs yet (run `brokkr corpus ...` first)");
        return Ok(());
    }
    let db = CorpusDb::open_readonly(&db_path)?;

    // --sql escape hatch (read-only, SELECT/WITH only).
    if let Some(sql) = &q.sql {
        guard_sql(sql, "--sql", true)?;
        let table = db.raw_sql(sql)?;
        println!("{}", corpus_db::raw_table(&table));
        return Ok(());
    }

    // --runtimes [--over SECS]: per-probe most-recent runtime, slowest first.
    // Calls the same per-probe estimate the pre-run ceiling sums, so the view
    // can never disagree with the wall.
    if q.runtimes {
        let over_ms = q.over.map(|s| s * 1000.0);
        let rows = db.runtimes(over_ms)?;
        println!(
            "{}",
            corpus_db::runtimes_table(&rows, super::cmd::RUNTIME_CEILING_MS)
        );
        return Ok(());
    }

    // --trend <probe> over recent runs.
    if let Some(probe) = &q.trend {
        let rows = db.trend_for_probe(probe, q.limit)?;
        println!("{}", corpus_db::trend_table(&rows));
        return Ok(());
    }

    // `--columns` only shapes the `--diffs` table; reject it elsewhere rather
    // than silently ignore it.
    if !q.columns.is_empty() && !q.diffs {
        return Err(DevError::Config(
            "results --columns shapes the --diffs table - add --diffs \
             (e.g. `--diffs --probe <id> --columns our_qty,tv_entry_qty`)"
                .to_owned(),
        ));
    }

    // --diffs [--probe ... ] [--columns ...] [--where E]: the shapeable diff
    // table across the selected/latest run. `--probe` here is an IN-list
    // filter, not the combo view.
    if q.diffs {
        let Some(run_id) = resolve_run(&db, q)? else {
            output::result_msg("no corpus runs to filter");
            return Ok(());
        };
        let columns = corpus_db::resolve_diff_columns(&q.columns)?;
        let vertical = q.columns.len() == 1 && q.columns[0] == "all";
        let where_expr = match &q.where_expr {
            Some(e) => {
                guard_sql(e, "--where", false)?;
                Some(e.as_str())
            }
            None => None,
        };
        let table = db.diffs(run_id, &q.probe, &columns, where_expr)?;
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
                "results: multiple --probe needs --diffs (the multi-probe diff table); \
                 a bare --probe shows one probe's full combo view"
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

    // Bare `brokkr results`: the recent-runs table.
    let rows = db.recent_runs(q.limit)?;
    println!("{}", corpus_db::runs_table(&rows));
    Ok(())
}

/// The run id named explicitly via `--run` or the bare positional argument.
/// A non-numeric positional is a clear error (it's not a probe selector).
fn explicit_run_id(q: &ResultsQuery) -> Result<Option<i64>, DevError> {
    if let Some(run) = q.run {
        return Ok(Some(run));
    }
    if let Some(s) = &q.query {
        return s.parse::<i64>().map(Some).map_err(|_| {
            DevError::Config(format!(
                "results (piners): '{s}' is not a run id - pass a numeric run id, \
                 or use `--probe <id>` to look up a probe"
            ))
        });
    }
    Ok(None)
}

/// The explicit run id if given, else the latest recorded run.
fn resolve_run(db: &CorpusDb, q: &ResultsQuery) -> Result<Option<i64>, DevError> {
    if let Some(id) = explicit_run_id(q)? {
        return Ok(Some(id));
    }
    db.latest_run_id()
}

fn render_probe(db: &CorpusDb, run_id: i64, probe: &str) -> Result<(), DevError> {
    match db.disposition_for_probe(run_id, probe)? {
        Some(d) => {
            println!("run {run_id}");
            println!("{}", corpus_db::dispositions_table(std::slice::from_ref(&d)));
            let diffs = db.trade_diffs_for_probe(run_id, probe)?;
            if diffs.is_empty() {
                output::result_msg("(no trade_diff rows - probe was byte-exact or divergence-free)");
            } else {
                println!("\ntrade_diff:");
                println!("{}", corpus_db::trade_diffs_table(&diffs));
            }
        }
        None => output::result_msg(&format!("probe '{probe}' not found in run {run_id}")),
    }
    Ok(())
}

fn render_run_detail(db: &CorpusDb, run_id: i64, full: bool) -> Result<(), DevError> {
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
            disps.iter().filter(|d| !d.gate_ok).cloned().collect()
        };
        let hidden = total - shown.len();
        println!("run {run_id}");
        if shown.is_empty() {
            output::result_msg(&format!(
                "all {total} probe(s) match their pin - pass --full to show"
            ));
        } else {
            println!("{}", corpus_db::dispositions_table(&shown));
            if hidden > 0 {
                output::result_msg(&format!(
                    "{hidden} probe(s) match their pin (hidden) - pass --full to show"
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

/// Reject the benchmark filters - they have no meaning against the corpus
/// store, and silently ignoring them would mislead.
fn reject_bench_flags(q: &ResultsQuery) -> Result<(), DevError> {
    let any_bench = q.commit.is_some()
        || q.compare.is_some()
        || q.command.is_some()
        || q.mode.is_some()
        || q.dataset.is_some()
        || !q.meta.is_empty()
        || !q.env.is_empty()
        || !q.grep.is_empty();
    if any_bench {
        return Err(DevError::Config(
            "results (piners): --commit/--compare/--command/--mode/--dataset/--meta/--env/--grep \
             are benchmark filters and don't apply to the corpus run store. Use \
             --probe/--diffs/--trend/--run/--where/--sql (or a bare run id)."
                .to_owned(),
        ));
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
            "results {flag}: ';' (statement stacking) is not allowed"
        )));
    }
    if require_select {
        let first = input.split_whitespace().next().unwrap_or("");
        if !first.eq_ignore_ascii_case("select") && !first.eq_ignore_ascii_case("with") {
            return Err(DevError::Config(format!(
                "results {flag}: only read-only SELECT/WITH queries are allowed"
            )));
        }
    }
    Ok(())
}
