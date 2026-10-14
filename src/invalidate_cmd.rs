//! Implementation of the `invalidate` command - hard-delete benchmark results
//! and their sidecar profile data by UUID prefix or commit prefix.

use std::path::Path;

use crate::db::{self, RunId, RunSelector, RunStore};
use crate::error::DevError;
use crate::output;
use crate::resolve::{results_db_path, sidecar_db_path};

pub(crate) fn cmd_invalidate(
    project_root: &Path,
    uuid_prefix: Option<&str>,
    commit_prefix: Option<&str>,
    force: bool,
) -> Result<(), DevError> {
    // An empty prefix is `LIKE '%'` - every row in both databases, which
    // `brokkr invalidate "" -f` used to delete. Refused before anything opens.
    if let Some(uuid) = uuid_prefix {
        db::like::require_prefix(uuid, "uuid")?;
    }
    if let Some(commit) = commit_prefix {
        db::like::require_prefix(commit, "commit")?;
    }

    let store = RunStore::open(
        &results_db_path(project_root),
        &sidecar_db_path(project_root),
    )?;
    if !store.has_results_db() && !store.has_sidecar_db() {
        output::result_msg("no results.db or sidecar.db found - nothing to invalidate");
        return Ok(());
    }

    let (selector, descriptor) = match (uuid_prefix, commit_prefix) {
        (Some(uuid), _) => (RunSelector::Uuid(uuid), format!("uuid prefix '{uuid}'")),
        (None, Some(commit)) => (
            RunSelector::Commit(commit),
            format!("commit prefix '{commit}'"),
        ),
        // Clap's required_unless_present guarantees one of the two is set.
        (None, None) => {
            return Err(DevError::Config(
                "invalidate: supply a UUID prefix or --commit <hash>".into(),
            ));
        }
    };

    let targets = store.select_runs(selector)?;
    if targets.is_empty() {
        output::result_msg(&format!("no matching rows for {descriptor}"));
        return Ok(());
    }

    preview(&targets, &store, &descriptor)?;
    if !force {
        output::result_msg("dry-run - re-run with -f / --force to delete");
        return Ok(());
    }

    let mut results_removed = 0usize;
    let mut sidecar_sessions_removed = 0usize;
    for id in &targets {
        let deleted = store.delete(id)?;
        results_removed += deleted.results_rows;
        sidecar_sessions_removed += deleted.sidecar_sessions;
    }
    output::result_msg(&format!(
        "deleted {results_removed} results row(s), {sidecar_sessions_removed} sidecar session(s)",
    ));
    Ok(())
}

fn preview(ids: &[RunId], store: &RunStore, descriptor: &str) -> Result<(), DevError> {
    output::result_msg(&format!("{} UUID(s) match {descriptor}:", ids.len()));
    for id in ids {
        let short = &id.as_str()[..8.min(id.as_str().len())];
        let results_row = store.result_preview(id)?;
        let sidecar_present = store.has_sidecar_data(id)?;
        let detail = match results_row {
            Some(row) => format!(
                "{} {} {} (elapsed {}ms)",
                row.commit.chars().take(8).collect::<String>(),
                row.command,
                if row.mode.is_empty() {
                    "-"
                } else {
                    row.mode.as_str()
                },
                row.elapsed_ms,
            ),
            None => String::from("(sidecar-only, no results row)"),
        };
        let sc_tag = if sidecar_present { " [+sidecar]" } else { "" };
        output::result_msg(&format!("  {short}  {detail}{sc_tag}"));
    }
    Ok(())
}
