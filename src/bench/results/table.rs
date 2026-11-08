//! The script-bench table: one line per run, newest first.

use super::super::store::{MarkView, RunOutcome, StoredRun};
use super::{
    bench_name, hidden_by, ms, params_summary, result_verdict, result_wall, short_commit,
    stored_as, timestamp, verdict,
};

/// Widest a params or env cell grows before it is cut with `…`.
const CELL_MAX: usize = 40;

fn cut(text: String) -> String {
    if text.chars().count() <= CELL_MAX {
        return text;
    }
    let mut out: String = text.chars().take(CELL_MAX - 1).collect();
    out.push('…');
    out
}

/// A run's result cell: the best wall, or the outcome of a run without one.
fn result_cell(run: &StoredRun) -> String {
    match (&run.doc.outcome, result_wall(run)) {
        (RunOutcome::Completed, Some(wall)) => ms(wall),
        (RunOutcome::Completed, None) => "completed".to_owned(),
        (RunOutcome::Failed { .. }, _) => "failed".to_owned(),
        (RunOutcome::Interrupted, _) => "interrupted".to_owned(),
    }
}

fn env_cell(run: &StoredRun) -> String {
    let env = &run.doc.conditions.captured_env;
    cut(env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(","))
}

/// Render runs as an aligned table, resolved against `marks`, the marks
/// they were selected under. The verdict, env and hidden columns appear only
/// when some row has a verdict other than nil, a captured variable, or a
/// mark hiding it.
pub(crate) fn format_table(runs: &[StoredRun], marks: &MarkView) -> String {
    let hidden = |run: &StoredRun| {
        hidden_by(marks, run)
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>()
            .join(",")
    };
    let with_hidden = runs.iter().any(|r| !hidden(r).is_empty());
    let with_verdict = runs
        .iter()
        .any(|r| result_verdict(r).is_some_and(|v| v.is_some()));
    let with_env = runs
        .iter()
        .any(|r| !r.doc.conditions.captured_env.is_empty());
    let mut header = vec!["uuid", "timestamp", "commit", "bench", "result", "params"];
    if with_verdict {
        header.push("verdict");
    }
    if with_env {
        header.push("env");
    }
    if with_hidden {
        header.push("hidden");
    }
    let mut rows: Vec<Vec<String>> = vec![header.iter().map(|h| (*h).to_owned()).collect()];
    for run in runs {
        let mut row = vec![
            run.doc.uuid.chars().take(8).collect(),
            timestamp(run.doc.started_unix_ms),
            short_commit(run),
            match stored_as(marks, run) {
                Some(stored) => format!("{} (as {stored})", bench_name(marks, run)),
                None => run.doc.bench.clone(),
            },
            result_cell(run),
            cut(params_summary(run)),
        ];
        if with_verdict {
            row.push(result_verdict(run).map_or_else(String::new, verdict));
        }
        if with_env {
            row.push(env_cell(run));
        }
        if with_hidden {
            row.push(hidden(run));
        }
        rows.push(row);
    }
    let columns = rows[0].len();
    let widths: Vec<usize> = (0..columns)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    // The result column is right-aligned, as walls are in every view.
    let result_column = 4;
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(c, cell)| {
                    let pad = widths[c].saturating_sub(cell.chars().count());
                    if c == result_column {
                        format!("{}{cell}", " ".repeat(pad))
                    } else if c + 1 == columns {
                        cell.clone()
                    } else {
                        format!("{cell}{}", " ".repeat(pad))
                    }
                })
                .collect();
            cells.join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
