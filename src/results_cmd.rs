//! Implementation of the `results` command - query the results DB and render
//! rows as a compact table (or as a detailed block when a UUID prefix
//! resolves to a single row).

use std::path::Path;

use crate::bench::results::{BenchFilter, BenchReader, Lookup};
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

    // One stored group (a retired library measurement's): its rows share one
    // uuid, and each is shown whole, the scenario's first, iterations and
    // provenance included.
    if !force_table && rows.len() > 1 && rows.iter().all(|r| r.uuid == rows[0].uuid) {
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                println!();
            }
            println!("{}", db::format_single_result(row, has_sidecar(&row.uuid)));
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

    // The per-user bench store holds this project's script-bench runs,
    // locally or on the configured results server. One that exists but cannot
    // be read, or a server that cannot be reached, fails the command like an
    // unreadable results.db would: an unanswerable query is not an empty
    // answer.
    let bench = BenchReader::open(&q.project, dev_config.results_server.as_ref())?;
    let bench_filter = bench_filter(q);
    // litehtml's `.brokkr/results.db` is its mechanical database, another
    // schema: it has no typed results, only script-bench runs.
    let store = if q.project == crate::project::Project::Litehtml.name() {
        db::RunStore::absent()
    } else {
        db::RunStore::open_read_only(&db_path, &sidecar_path)?
    };
    let bench_has_runs = match &bench {
        Some(bench) => bench.has_runs()?,
        None => false,
    };
    if !store.has_results_db() && !bench_has_runs {
        // Nothing of this project's, but a uuid may still name another
        // project's script-bench run, which is worth saying.
        if let (ResultsView::Uuid { prefix }, Some(bench)) = (&q.view, &bench)
            && !prefix.is_empty()
        {
            let lookup = bench.lookup(prefix, &bench_filter)?;
            if !lookup.elsewhere.is_empty() {
                show_bench_uuid(&lookup, false, 0, q.top);
                return Ok(());
            }
        }
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }
    // `--param` names script-bench params, which no typed row has.
    let typed = store.has_results_db() && q.params.is_empty();

    match &q.view {
        ResultsView::Uuid { prefix } => {
            let lookup = match &bench {
                Some(bench) => Some(bench.lookup(prefix, &bench_filter)?),
                None => None,
            };
            let (bench_candidates, bench_found) = lookup.as_ref().map_or((0, false), |l| {
                (l.candidates, !l.runs.is_empty() || l.omitted > 0)
            });
            let typed_candidates = if typed {
                show_uuid(
                    &store,
                    prefix,
                    &q.filter,
                    q.top,
                    &matcher,
                    // Bench runs the filters excluded leave the typed view
                    // exactly as it was without them.
                    TypedUuid {
                        quiet_empty: bench_found,
                        other_candidates: if bench_found { bench_candidates } else { 0 },
                    },
                )?
            } else {
                0
            };
            if let Some(lookup) = lookup {
                show_bench_uuid(&lookup, typed, typed_candidates, q.top);
            } else if !typed {
                output::result_msg("no matching results");
            }
        }
        ResultsView::Compare {
            commits: (commit_a, commit_b),
        } => {
            let comparison = match &bench {
                Some(bench) => bench.compare_sides(commit_a, commit_b, &bench_filter)?,
                None => crate::bench::results::Comparison::default(),
            };
            let bench_empty = comparison.a.is_empty() && comparison.b.is_empty();
            let typed_shown =
                typed && show_typed_compare(&store, commit_a, commit_b, q, &matcher, bench_empty)?;
            if !bench_empty {
                if typed_shown {
                    println!();
                }
                println!(
                    "{}",
                    crate::bench::results::format_compare(
                        commit_a,
                        commit_b,
                        &comparison,
                        q.top,
                        q.filter.limit,
                    )
                );
            } else if !typed {
                println!("(no results)");
            }
        }
        ResultsView::Table => {
            let bench_rows = match &bench {
                Some(bench) => bench.table(&bench_filter)?,
                None => crate::bench::results::Listing::default(),
            };
            show_table(&store, typed, &q.filter, &matcher, &bench_rows)?;
        }
    }

    Ok(())
}

/// The table view: the typed table, then the script-bench table, each only
/// when it has rows.
#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
fn show_table(
    store: &db::RunStore,
    typed: bool,
    filter: &db::QueryFilter,
    matcher: &DatasetMatcher,
    bench: &crate::bench::results::Listing,
) -> Result<(), DevError> {
    let bench_rows = &bench.runs;
    let typed_rows = if typed {
        let results_db = store
            .results_db()
            .ok_or_else(|| DevError::Config("results database is not available".into()))?;
        results_db.query(filter)?
    } else {
        Vec::new()
    };
    if !typed_rows.is_empty() {
        println!("{}", db::format_table(&typed_rows, matcher));
    }
    if !bench_rows.is_empty() {
        if !typed_rows.is_empty() {
            println!();
        }
        println!(
            "{}",
            crate::bench::results::format_table(bench_rows, &bench.marks)
        );
    }
    if typed_rows.is_empty() && bench_rows.is_empty() {
        output::result_msg("no matching results");
    }
    Ok(())
}

/// `results`' row filters as they apply to script-bench runs: a filter no
/// bench run can answer (`--mode`, `--dataset`, `--meta`) excludes them all,
/// as `--param` excludes every typed row.
fn bench_filter(q: &ResultsQuery) -> BenchFilter {
    let f = &q.filter;
    BenchFilter {
        commit: f.commit.clone(),
        bench: f.command.clone(),
        env: f.env.clone(),
        params: q.params.clone(),
        grep: f.grep.clone(),
        grep_v: f.grep_v.clone(),
        excluded: f.mode.is_some() || f.dataset.is_some() || !f.meta.is_empty(),
        hidden: q.hidden,
        limit: f.limit,
    }
}

/// The script-bench half of a uuid lookup: one run's block when the prefix
/// named exactly one run in either store, the table otherwise.
#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
fn show_bench_uuid(lookup: &Lookup, typed: bool, typed_candidates: usize, top: usize) {
    if !lookup.runs.is_empty() {
        if typed_candidates > 0 {
            println!();
        }
        if lookup.runs.len() == 1 && lookup.candidates + typed_candidates == 1 {
            println!(
                "{}",
                crate::bench::results::format_single(&lookup.runs[0], &lookup.marks, top)
            );
        } else {
            println!(
                "{}",
                crate::bench::results::format_table(&lookup.runs, &lookup.marks)
            );
        }
    }
    if lookup.omitted > 0 {
        let more = if lookup.runs.is_empty() { "" } else { " more" };
        output::result_msg(&format!(
            "{}{more} matching script-bench run(s) not shown - raise --limit",
            lookup.omitted
        ));
    }
    let found = !lookup.runs.is_empty() || lookup.omitted > 0;
    if !found && !typed && lookup.elsewhere.is_empty() {
        output::result_msg("no matching results");
    }
    for (uuid, project) in &lookup.elsewhere {
        output::result_msg(&format!(
            "script-bench run {} belongs to project {project}; `brokkr results` shows the current project's runs",
            db::short_uuid(uuid)
        ));
    }
}

/// The typed comparison, printed exactly as before script benches existed,
/// except that its empty answer is left out when the bench comparison has
/// pairs to show. Returns whether it printed anything.
#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
fn show_typed_compare(
    store: &db::RunStore,
    commit_a: &str,
    commit_b: &str,
    q: &ResultsQuery,
    matcher: &DatasetMatcher,
    bench_empty: bool,
) -> Result<bool, DevError> {
    let results_db = store
        .results_db()
        .ok_or_else(|| DevError::Config("results database is not available".into()))?;
    let (rows_a, rows_b) = results_db.query_compare(commit_a, commit_b, &q.filter)?;
    if rows_a.is_empty() && rows_b.is_empty() && !bench_empty {
        return Ok(false);
    }
    let table = db::format_compare_with_limit(
        &db::Sides {
            commit_a,
            rows_a: &rows_a,
            commit_b,
            rows_b: &rows_b,
        },
        q.top,
        q.filter.limit,
        matcher,
    );
    println!("{table}");
    Ok(true)
}

/// How the typed half of a uuid lookup defers to the script-bench half.
#[derive(Clone, Copy)]
struct TypedUuid {
    /// The bench half has runs to show, so "no matching results" would
    /// contradict it; every other notice still prints.
    quiet_empty: bool,
    /// Bench runs the prefix also named, which make it ambiguous.
    other_candidates: usize,
}

/// Results keeps the historical multi-row display policy, while resolution
/// itself is shared with sidecar and invalidate. Returns how many typed runs
/// the prefix named.
fn show_uuid(
    store: &db::RunStore,
    prefix: &str,
    filter: &db::QueryFilter,
    top: usize,
    matcher: &DatasetMatcher,
    bench: TypedUuid,
) -> Result<usize, DevError> {
    let ids = match store.resolve(db::RunRef::Uuid(prefix))? {
        db::Resolved::One(id) => vec![id],
        db::Resolved::Ambiguous(ids) => ids,
        db::Resolved::Missing => Vec::new(),
    };
    let unique = ids.len() + bench.other_candidates == 1;
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
                report_sidecar_only(prefix, &sidecar_only_ids, unique);
            }
        } else if !sidecar_only_ids.is_empty() {
            if has_result_candidate {
                output::result_msg("no matching results; sidecar-only run(s) also matched");
            } else {
                output::result_msg("sidecar-only run (no results DB entry - dirty tree or failed)");
            }
            report_sidecar_only(prefix, &sidecar_only_ids, unique);
        } else if !bench.quiet_empty {
            output::result_msg("no matching results");
        }
    } else {
        render_single_or_multi(&rows, &sidecar_ids, top, matcher, !unique);
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
    Ok(ids.len())
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
