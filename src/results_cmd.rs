//! Implementation of the `results` command - query the results DB and render
//! rows as a compact table (or as a detailed block when a UUID prefix
//! resolves to a single row).

use std::path::Path;

use crate::config::{self, DevConfig};
use crate::db;
use crate::db::DatasetMatcher;
use crate::error::DevError;
use crate::hotpath_fmt;
use crate::output;
use crate::request::{ResultsQuery, ResultsView};
use crate::resolve::{results_db_path, sidecar_db_path};

fn open_sidecar_db(project_root: &Path) -> Option<db::sidecar::SidecarDb> {
    let path = sidecar_db_path(project_root);
    if path.exists() {
        db::sidecar::SidecarDb::open(&path).ok()
    } else {
        None
    }
}

/// Render a detail-style result view. When the result set is exactly
/// one row, use the new labelled-block layout via
/// `db::format_single_result` - no compact table header, multi-line
/// cli_args, brokkr_args surfaced, sidecar hint folded in as a field.
/// When the result set has multiple rows (a UUID prefix that matched
/// many), fall back to `format_table` + per-row `format_details`.
fn render_single_or_multi(
    rows: &[db::StoredRow],
    sidecar_db: Option<&db::sidecar::SidecarDb>,
    top: usize,
    matcher: &DatasetMatcher,
) {
    let has_sidecar = |uuid: &str| sidecar_db.is_some_and(|sdb| sdb.has_data(uuid));

    if rows.len() == 1 {
        let row = &rows[0];
        let block = db::format_single_result(row, has_sidecar(&row.uuid));
        println!("{block}");
        if let Some(ref hotpath) = row.hotpath
            && let Some(report) = hotpath_fmt::format_hotpath_report(hotpath, top)
        {
            println!("\n{report}");
        }
        return;
    }

    // Multi-row result set - keep the compact table + per-row details.
    let table = db::format_table(rows, matcher);
    println!("{table}");
    for row in rows {
        let details = db::format_details(row);
        if !details.is_empty() {
            println!("\n{details}");
        }
        if let Some(ref hotpath) = row.hotpath
            && let Some(report) = hotpath_fmt::format_hotpath_report(hotpath, top)
        {
            println!("\n{report}");
        }
    }
    // One hint for the whole set, naming the rows it applies to.
    let with_sidecar: Vec<&str> = rows
        .iter()
        .filter(|r| has_sidecar(&r.uuid))
        .map(|r| &r.uuid[..8.min(r.uuid.len())])
        .collect();
    if !with_sidecar.is_empty() {
        output::sidecar_msg(&format!(
            "sidecar data for {}: use `brokkr sidecar <id>` (add --samples/--markers/--durations/--counters for raw views)",
            with_sidecar.join(", "),
        ));
    }
}

pub(crate) fn cmd_results(
    dev_config: &DevConfig,
    project_root: &Path,
    q: &ResultsQuery,
) -> Result<(), DevError> {
    let db_path = results_db_path(project_root);
    let matcher = DatasetMatcher::new(config::all_dataset_keys(dev_config));

    if !db_path.exists() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    let results_db = db::ResultsDb::open(&db_path)?;
    let sidecar_db = open_sidecar_db(project_root);

    match q.view()? {
        ResultsView::Uuid {
            prefix,
            filter,
            top,
        } => {
            // Resolve sidecar latest-keys (e.g. "dirty") so the results DB
            // lookup uses the actual UUID, not the alias.
            let resolved_prefix = sidecar_db
                .as_ref()
                .map(|sdb| sdb.resolve_latest(&prefix))
                .unwrap_or_else(|| prefix.clone());
            let rows = results_db.query_by_uuid_with_filter(&resolved_prefix, &filter)?;
            if rows.is_empty() {
                // A supplied row filter may have excluded a real result row. Only
                // call it sidecar-only when the UUID has no results row at all.
                if !results_db.has_uuid_prefix(&resolved_prefix)? {
                    if let Some(ref sdb) = sidecar_db
                        && sdb.has_data(&prefix)
                    {
                        output::result_msg(
                            "sidecar-only run (no results DB entry - dirty tree or failed)",
                        );
                        output::result_msg(&format!(
                            "use `brokkr sidecar {prefix}` (default phase summary; add --samples/--markers/--durations/--counters/--stat for raw views)",
                        ));
                    } else {
                        output::result_msg("no matching results");
                    }
                } else {
                    output::result_msg("no matching results");
                }
            } else {
                render_single_or_multi(&rows, sidecar_db.as_ref(), top, &matcher);
            }
        }
        ResultsView::Compare {
            commits: (commit_a, commit_b),
            filter,
            top,
        } => {
            let (rows_a, rows_b) = results_db.query_compare(&commit_a, &commit_b, &filter)?;
            let table = db::format_compare_with_limit(
                &commit_a,
                &rows_a,
                &commit_b,
                &rows_b,
                top,
                filter.limit,
                &matcher,
            );
            println!("{table}");
        }
        ResultsView::Table { filter } => {
            let rows = results_db.query(&filter)?;
            if rows.is_empty() {
                output::result_msg("no matching results");
            } else {
                let table = db::format_table(&rows, &matcher);
                println!("{table}");
            }
        }
    }

    Ok(())
}
