//! `brokkr lint-results` - query the lint corpus run store (`runs.db`).
//!
//! The query sibling of `brokkr lint-corpus`, mirroring `brokkr
//! corpus-results` but over the much smaller lint schema. Bare lists recent
//! runs; a run id (positional or `--run`) shows that run's per-probe
//! dispositions, deviations-only unless `--full`.

use std::path::Path;

use crate::error::DevError;
use crate::piners::lint::db::LintDb;
use crate::resolve::lint_runs_db_path;

/// Flags lifted off the `LintResults` CLI command.
#[derive(Debug, Default)]
pub struct LintQuery {
    /// Positional run id (the run-detail view).
    pub run_id: Option<i64>,
    /// `--run <N>` (same as the positional id).
    pub run: Option<i64>,
    /// `-n` cap on the recent-runs table.
    pub limit: usize,
    /// `--full`: show every probe in the run-detail view, not just deviations.
    pub full: bool,
}

/// Entry point for `brokkr lint-results`.
pub fn cmd(project_root: &Path, q: &LintQuery) -> Result<(), DevError> {
    let db_path = lint_runs_db_path(project_root);
    if !db_path.exists() {
        crate::output::lint_msg(
            "no lint runs recorded yet (run `brokkr lint-corpus --keyword <k>` first)",
        );
        return Ok(());
    }
    let db = LintDb::open_readonly(&db_path)?;

    // A specific run (positional id or --run), else the latest when neither is
    // given but a detail view is implied by --full.
    let target = q.run_id.or(q.run);
    if let Some(id) = target {
        println!("{}", db.run_dispositions(id, q.full)?);
        return Ok(());
    }
    if q.full {
        match db.latest_run_id()? {
            Some(id) => println!("{}", db.run_dispositions(id, true)?),
            None => crate::output::lint_msg("no lint runs recorded yet"),
        }
        return Ok(());
    }

    let limit = if q.limit == 0 { 20 } else { q.limit };
    println!("{}", db.recent_runs(limit)?);
    Ok(())
}
