//! Implementation of the `results` command — query and render the results DB
//! with optional sidecar timeline / marker views.

use std::path::Path;

use crate::db;
use crate::error::DevError;
use crate::hotpath_fmt;
use crate::output;
use crate::request::ResultsQuery;
use crate::resolve::{results_db_path, sidecar_db_path};
use crate::sidecar_fmt::{
    apply_timeline_filter, parse_time_range, print_compare_timeline, print_counters,
    print_field_stat, print_marker_durations, print_marker_phases_with_counters,
    print_phase_summary, print_run_info, resolve_phase_range, sidecar_marker_json,
    sidecar_sample_json_projected,
};

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
pub(crate) fn render_single_or_multi(
    rows: &[db::StoredRow],
    sidecar_db: Option<&db::sidecar::SidecarDb>,
    top: usize,
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
    let table = db::format_table(rows);
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
            output::sidecar_msg("--timeline/--markers");
        }
    }
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(crate) fn cmd_results(project_root: &Path, q: &ResultsQuery) -> Result<(), DevError> {
    let db_path = results_db_path(project_root);

    if !db_path.exists() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    let results_db = db::ResultsDb::open(&db_path)?;
    let sidecar_db = open_sidecar_db(project_root);

    // --compare-timeline <uuid_a> <uuid_b>
    if let Some(ref uuids) = q.compare_timeline {
        let Some(ref sdb) = sidecar_db else {
            output::result_msg("no sidecar.db found");
            return Ok(());
        };
        let uuid_a = &uuids[0];
        let uuid_b = &uuids[1];
        let (best_a, _) = sdb.query_meta(uuid_a);
        let (best_b, _) = sdb.query_meta(uuid_b);
        let samples_a = sdb.query_samples(uuid_a, Some(best_a))?;
        let samples_b = sdb.query_samples(uuid_b, Some(best_b))?;
        if samples_a.is_empty() || samples_b.is_empty() {
            output::result_msg("one or both results have no sidecar data");
            return Ok(());
        }
        let markers_a = sdb.query_markers(uuid_a, Some(best_a))?;
        let markers_b = sdb.query_markers(uuid_b, Some(best_b))?;
        print_compare_timeline(
            uuid_a, &samples_a, &markers_a, uuid_b, &samples_b, &markers_b, q.human,
        );
        return Ok(());
    }

    // Resolve effective UUID: explicit query arg, or last result from DB.
    let effective_uuid: Option<String> = if let Some(ref prefix) = q.query {
        Some(prefix.clone())
    } else if q.timeline || q.markers {
        let filter = db::QueryFilter {
            limit: 1,
            ..Default::default()
        };
        let rows = results_db.query(&filter)?;
        if let Some(row) = rows.first() {
            Some(row.uuid.clone())
        } else {
            output::result_msg("no results yet");
            return Ok(());
        }
    } else {
        None
    };

    if let Some(ref uuid_prefix) = effective_uuid {
        // Sidecar output modes.
        if q.timeline {
            let Some(ref sdb) = sidecar_db else {
                output::result_msg("no sidecar.db found");
                return Ok(());
            };
            print_run_info(sdb, uuid_prefix);

            // Resolve --run: "all" → None (all runs), N → Some(N), absent → best run.
            let (best_idx, total) = sdb.query_meta(uuid_prefix);
            let run_filter = match q.run.as_deref() {
                Some("all") => None,
                Some(n) => Some(n.parse::<usize>().map_err(|_| {
                    DevError::Config(format!("--run: expected a number or 'all', got '{n}'"))
                })?),
                None => Some(best_idx),
            };
            if total > 1 {
                let showing = match run_filter {
                    Some(idx) => format!("run {idx}/{total}"),
                    None => format!("all {total} runs"),
                };
                output::sidecar_msg(&format!("showing {showing} (use --run to override)"));
            }

            let mut samples = sdb.query_samples(uuid_prefix, run_filter)?;
            if samples.is_empty() {
                output::result_msg("no sidecar data for this result");
            } else if q.summary {
                let markers = sdb.query_markers(uuid_prefix, run_filter)?;
                print_phase_summary(&samples, &markers, q.human);
            } else {
                if let Some(ref phase_name) = q.phase {
                    let markers = sdb.query_markers(uuid_prefix, run_filter)?;
                    let (start_us, end_us) = resolve_phase_range(phase_name, &markers, &samples)?;
                    samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
                }

                if let Some(ref range_str) = q.range {
                    let (start_us, end_us) = parse_time_range(range_str)?;
                    samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
                }

                if let Some(ref field) = q.stat {
                    let filtered = apply_timeline_filter(&samples, q);
                    print_field_stat(&filtered, field)?;
                } else {
                    let filtered = apply_timeline_filter(&samples, q);
                    let fields = if q.fields.is_empty() {
                        None
                    } else {
                        Some(&q.fields)
                    };
                    for s in &filtered {
                        println!("{}", sidecar_sample_json_projected(s, fields));
                    }
                }
            }
            return Ok(());
        }
        if q.markers {
            let Some(ref sdb) = sidecar_db else {
                output::result_msg("no sidecar.db found");
                return Ok(());
            };
            print_run_info(sdb, uuid_prefix);
            let (best_idx, total) = sdb.query_meta(uuid_prefix);
            let run_filter = match q.run.as_deref() {
                Some("all") => None,
                Some(n) => Some(n.parse::<usize>().map_err(|_| {
                    DevError::Config(format!("--run: expected a number or 'all', got '{n}'"))
                })?),
                None => Some(best_idx),
            };
            if total > 1 {
                let showing = match run_filter {
                    Some(idx) => format!("run {idx}/{total}"),
                    None => format!("all {total} runs"),
                };
                output::sidecar_msg(&format!("showing {showing} (use --run to override)"));
            }
            let markers = sdb.query_markers(uuid_prefix, run_filter)?;
            if q.counters {
                let counters = sdb.query_counters(uuid_prefix, run_filter)?;
                if counters.is_empty() {
                    output::result_msg("no counters for this result");
                } else {
                    print_counters(&counters);
                }
                return Ok(());
            }
            if markers.is_empty() {
                output::result_msg("no sidecar markers for this result");
            } else if q.phases {
                let samples = sdb.query_samples(uuid_prefix, run_filter)?;
                let counters = sdb.query_counters(uuid_prefix, run_filter)?;
                print_marker_phases_with_counters(&markers, &samples, &counters);
            } else if q.durations {
                print_marker_durations(&markers);
            } else {
                for m in &markers {
                    println!("{}", sidecar_marker_json(m));
                }
            }
            return Ok(());
        }

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
                print_run_info(sdb, uuid_prefix);
                output::result_msg("sidecar-only run (no results DB entry — dirty tree or failed)");
                output::result_msg("use --timeline, --markers, or --markers --phases for data");
            } else {
                output::result_msg("no matching results");
            }
        } else {
            render_single_or_multi(&rows, sidecar_db.as_ref(), q.top);
        }
    } else if let Some(ref commits) = q.compare {
        let commit_a = commits.first().map_or("", String::as_str);
        let commit_b = commits.get(1).map_or("", String::as_str);
        let (rows_a, rows_b) = results_db.query_compare(
            commit_a,
            commit_b,
            q.command.as_deref(),
            q.mode.as_deref(),
            q.dataset.as_deref(),
        )?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b, q.top);
        println!("{table}");
    } else if q.compare_last {
        match results_db.query_compare_last(
            q.command.as_deref(),
            q.mode.as_deref(),
            q.dataset.as_deref(),
        )? {
            Some((commit_a, rows_a, commit_b, rows_b)) => {
                let table = db::format_compare(&commit_a, &rows_a, &commit_b, &rows_b, q.top);
                println!("{table}");
            }
            None => {
                output::result_msg("need at least two distinct commits to compare");
            }
        }
    } else {
        // Parse --meta KEY=VALUE strings into (key, value) pairs. The CLI
        // validator already guarantees each entry contains '=', so split_once
        // can't fail here, but we still defensively pattern-match.
        let meta_pairs: Vec<(String, String)> = q
            .meta
            .iter()
            .filter_map(|s| s.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
            .collect();

        let filter = db::QueryFilter {
            commit: q.commit.clone(),
            command: q.command.clone(),
            mode: q.mode.clone(),
            dataset: q.dataset.clone(),
            meta: meta_pairs,
            grep: q.grep.clone(),
            limit: q.limit,
        };
        let rows = results_db.query(&filter)?;
        if rows.is_empty() {
            output::result_msg("no matching results");
        } else {
            let table = db::format_table(&rows);
            println!("{table}");
        }
    }

    Ok(())
}
