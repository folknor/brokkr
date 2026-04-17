//! Implementation of the `results` command — query the results DB and render
//! rows as a compact table (or as a detailed block when a UUID prefix
//! resolves to a single row).

use std::path::Path;

use crate::config::{self, DevConfig};
use crate::db;
use crate::db::DatasetMatcher;
use crate::error::DevError;
use crate::hotpath_fmt;
use crate::output;
use crate::request::ResultsQuery;
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
/// `db::format_single_result` — no compact table header, multi-line
/// cli_args, brokkr_args surfaced, sidecar hint folded in as a field.
/// When the result set has multiple rows (a UUID prefix that matched
/// many), fall back to `format_table` + per-row `format_details`.
fn render_single_or_multi(
    rows: &[db::StoredRow],
    sidecar_db: Option<&db::sidecar::SidecarDb>,
    top: usize,
    matcher: &DatasetMatcher,
) {
    let has_sidecar = |uuid: &str| {
        sidecar_db.is_some_and(|sdb| sdb.has_data(uuid))
    };

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

    // Multi-row result set — keep the compact table + per-row details.
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
        if has_sidecar(&row.uuid) {
            output::sidecar_msg(&format!(
                "use `brokkr sidecar {}` (add --samples/--markers/--durations/--counters for raw views)",
                &row.uuid[..8.min(row.uuid.len())],
            ));
        }
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

    if let Some(ref uuid_prefix) = q.query {
        // Resolve sidecar latest-keys (e.g. "dirty") so the results DB
        // lookup uses the actual UUID, not the alias.
        let resolved_prefix = sidecar_db
            .as_ref()
            .map(|sdb| sdb.resolve_latest(uuid_prefix))
            .unwrap_or_else(|| uuid_prefix.to_owned());
        let rows = results_db.query_by_uuid(&resolved_prefix)?;
        if rows.is_empty() {
            // No results DB entry — check if sidecar data exists (dirty/failed run).
            if let Some(ref sdb) = sidecar_db
                && sdb.has_data(uuid_prefix)
            {
                output::result_msg("sidecar-only run (no results DB entry — dirty tree or failed)");
                output::result_msg(&format!(
                    "use `brokkr sidecar {uuid_prefix}` (default phase summary; add --samples/--markers/--durations/--counters/--stat for raw views)",
                ));
            } else {
                output::result_msg("no matching results");
            }
        } else {
            render_single_or_multi(&rows, sidecar_db.as_ref(), q.top, &matcher);
        }
        return Ok(());
    }

    if let Some(ref commits) = q.compare {
        let commit_a = commits.first().map_or("", String::as_str);
        let commit_b = commits.get(1).map_or("", String::as_str);
        let (rows_a, rows_b) = results_db.query_compare(
            commit_a,
            commit_b,
            q.command.as_deref(),
            q.mode.as_deref(),
            q.dataset.as_deref(),
        )?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b, q.top, &matcher);
        println!("{table}");
        return Ok(());
    }


    // Parse --meta KEY=VALUE strings into (key, value) pairs. The CLI
    // validator already guarantees each entry contains '=', so split_once
    // can't fail here, but we still defensively pattern-match.
    let meta_pairs: Vec<(String, String)> = q
        .meta
        .iter()
        .filter_map(|s| s.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect();
    let env_pairs: Vec<(String, String)> = q
        .env
        .iter()
        .filter_map(|s| s.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect();

    let filter = db::QueryFilter {
        commit: q.commit.clone(),
        command: q.command.clone(),
        mode: q.mode.clone(),
        dataset: q.dataset.clone(),
        meta: meta_pairs,
        env: env_pairs,
        grep: q.grep.clone(),
        limit: q.limit,
    };
    let rows = results_db.query(&filter)?;
    if rows.is_empty() {
        output::result_msg("no matching results");
    } else {
        let table = db::format_table(&rows, &matcher);
        println!("{table}");
    }

    Ok(())
}
