//! Implementation of the `results` command - query the results DB and render
//! rows as a compact table (or as a detailed block when a UUID prefix
//! resolves to a single row).

use std::path::Path;

use crate::config::{self, DevConfig};
use crate::db;
use crate::db::DatasetMatcher;
use crate::db::paths::{results_db_path, sidecar_db_path};
use crate::error::DevError;
use crate::hotpath_fmt;
use crate::output;
use crate::request::{ResultsQuery, ResultsView};

/// Render a detail-style result view. When the result set is exactly
/// one row, use the new labelled-block layout via
/// `db::format_single_result` - no compact table header, multi-line
/// cli_args, brokkr_args surfaced, sidecar hint folded in as a field.
/// When the result set has multiple rows (a UUID prefix that matched
/// many), fall back to `format_table` + per-row `format_details`.
#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
fn render_single_or_multi(
    rows: &[db::StoredRow],
    sidecar_ids: &std::collections::BTreeSet<String>,
    top: usize,
    matcher: &DatasetMatcher,
    force_table: bool,
) {
    let has_sidecar = |uuid: &str| sidecar_ids.contains(uuid);

    if rows.len() == 1 && !force_table {
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

    // One library measurement's group (`brokkr man library`, Measuring a
    // scenario): its rows share one uuid, and each is shown whole, the
    // scenario's first, iterations and provenance included.
    if !force_table && rows.len() > 1 && rows.iter().all(|r| r.uuid == rows[0].uuid) {
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                println!();
            }
            println!("{}", db::format_single_result(row, has_sidecar(&row.uuid)));
        }
        // A library against CLI measurement's comparison, never folded into
        // `--compare`.
        if let Some(lines) = crate::pbfhogg::library::cli_comparison(rows) {
            println!();
            for line in lines {
                println!("{line}");
            }
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
    let with_sidecar: Vec<String> = rows
        .iter()
        .filter(|r| has_sidecar(&r.uuid))
        .map(|r| db::short_uuid(&r.uuid))
        .collect();
    if !with_sidecar.is_empty() {
        output::sidecar_msg(&format!(
            "sidecar data for {}: use `brokkr sidecar <id>` ({})",
            with_sidecar.join(", "),
            crate::sidecar_cmd::view_selector_hint(),
        ));
    }
}

#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
pub(crate) fn cmd_results(
    dev_config: &DevConfig,
    project_root: &Path,
    q: &ResultsQuery,
) -> Result<(), DevError> {
    let db_path = results_db_path(project_root);
    let sidecar_path = sidecar_db_path(project_root);
    let matcher = DatasetMatcher::new(config::all_dataset_keys(dev_config));

    let store = db::RunStore::open_read_only(&db_path, &sidecar_path)?;
    if !store.has_results_db() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    match &q.view {
        ResultsView::Uuid { prefix } => {
            show_uuid(&store, prefix, &q.filter, q.top, &matcher)?;
        }
        ResultsView::Compare {
            commits: (commit_a, commit_b),
        } => {
            let results_db = store
                .results_db()
                .ok_or_else(|| DevError::Config("results database is not available".into()))?;
            let (rows_a, rows_b) = results_db.query_compare(commit_a, commit_b, &q.filter)?;
            // The canonical plans and contexts library rows name, so an
            // unpaired one can be explained down to the field.
            let digests: std::collections::BTreeSet<String> = rows_a
                .iter()
                .chain(&rows_b)
                .flat_map(db::library_rows::digests)
                .collect();
            let documents = results_db.library_documents(&digests)?;
            let table = db::format_compare_with_limit(
                &db::Sides {
                    commit_a,
                    rows_a: &rows_a,
                    commit_b,
                    rows_b: &rows_b,
                    documents: &documents,
                },
                q.top,
                q.filter.limit,
                &matcher,
            );
            println!("{table}");
        }
        ResultsView::Table => {
            let results_db = store
                .results_db()
                .ok_or_else(|| DevError::Config("results database is not available".into()))?;
            let rows = results_db.query(&q.filter)?;
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

/// Results keeps the historical multi-row display policy, while resolution
/// itself is shared with sidecar and invalidate.
fn show_uuid(
    store: &db::RunStore,
    prefix: &str,
    filter: &db::QueryFilter,
    top: usize,
    matcher: &DatasetMatcher,
) -> Result<(), DevError> {
    let ids = match store.resolve(db::RunRef::Uuid(prefix))? {
        db::Resolved::One(id) => vec![id],
        db::Resolved::Ambiguous(ids) => ids,
        db::Resolved::Missing => Vec::new(),
    };
    let (rows, omitted) = store.query_results(&ids, filter)?;
    let mut has_result_candidate = false;
    let mut sidecar_ids = std::collections::BTreeSet::new();
    let mut sidecar_only_ids = Vec::new();
    for id in &ids {
        let has_result = store.has_result(id)?;
        has_result_candidate |= has_result;
        if store.has_sidecar_data(id)? {
            sidecar_ids.insert(id.as_str().to_owned());
            if !has_result {
                sidecar_only_ids.push(id.as_str().to_owned());
            }
        }
    }
    if rows.is_empty() {
        if omitted > 0 {
            output::result_msg(&format!(
                "{omitted} matching result row(s) not shown - raise --limit",
            ));
            if !sidecar_only_ids.is_empty() {
                report_sidecar_only(prefix, &sidecar_only_ids, ids.len() == 1);
            }
        } else if !sidecar_only_ids.is_empty() {
            if has_result_candidate {
                output::result_msg("no matching results; sidecar-only run(s) also matched");
            } else {
                output::result_msg("sidecar-only run (no results DB entry - dirty tree or failed)");
            }
            report_sidecar_only(prefix, &sidecar_only_ids, ids.len() == 1);
        } else {
            output::result_msg("no matching results");
        }
    } else {
        render_single_or_multi(&rows, &sidecar_ids, top, matcher, ids.len() > 1);
        if omitted > 0 {
            output::result_msg(&format!(
                "{omitted} more matching result row(s) not shown - raise --limit",
            ));
        }
        if !sidecar_only_ids.is_empty() {
            output::sidecar_msg(&format!(
                "sidecar-only run(s) also matched: {}",
                sidecar_only_ids.join(", "),
            ));
            output::sidecar_msg("query one by its full UUID with `brokkr sidecar <uuid>`");
        }
    }
    Ok(())
}

fn report_sidecar_only(prefix: &str, ids: &[String], prefix_is_unique: bool) {
    if ids.len() == 1 && prefix_is_unique {
        output::result_msg(&format!(
            "use `brokkr sidecar {prefix}` (default phase summary; {})",
            crate::sidecar_cmd::view_selector_hint(),
        ));
    } else {
        output::sidecar_msg(&format!("sidecar-only UUIDs: {}", ids.join(", ")));
        output::sidecar_msg("query one by its full UUID with `brokkr sidecar <uuid>`");
    }
}
