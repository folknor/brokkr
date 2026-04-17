//! Implementation of the `sidecar` command — timeline / marker / phase views
//! over sidecar data captured by `.brokkr/sidecar.db`.

use std::path::Path;

use crate::db;
use crate::error::DevError;
use crate::output;
use crate::request::SidecarQuery;
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

/// Resolve `--run`: `"all"` → None (all runs), N → Some(N), absent → best run.
/// Also logs which run(s) will be shown when the result has multiple runs.
fn resolve_run_filter(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    run_arg: Option<&str>,
) -> Result<Option<usize>, DevError> {
    let (best_idx, total) = sdb.query_meta(uuid_prefix);
    let run_filter = match run_arg {
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
    Ok(run_filter)
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(crate) fn cmd_sidecar(project_root: &Path, q: &SidecarQuery) -> Result<(), DevError> {
    let Some(sdb) = open_sidecar_db(project_root) else {
        output::result_msg("no sidecar.db found");
        return Ok(());
    };

    // --compare-timeline <uuid_a> <uuid_b>
    if let Some(ref uuids) = q.compare_timeline {
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

    // Default view when no selector is given: the per-phase timeline summary
    // (JSONL). Equivalent to `--timeline --summary` — the overview that
    // wants to exist anyway, and the only view where "nothing specified"
    // has an obvious meaning.
    let timeline = q.timeline || !q.markers;
    let summary = q.summary || (!q.timeline && !q.markers);

    // Resolve UUID: explicit query arg, or last result from DB.
    let uuid_prefix: String = if let Some(ref prefix) = q.query {
        prefix.clone()
    } else {
        let db_path = results_db_path(project_root);
        if !db_path.exists() {
            output::result_msg("no results yet (run a benchmark first)");
            return Ok(());
        }
        let results_db = db::ResultsDb::open(&db_path)?;
        let filter = db::QueryFilter {
            limit: 1,
            ..Default::default()
        };
        let rows = results_db.query(&filter)?;
        let Some(row) = rows.first() else {
            output::result_msg("no results yet");
            return Ok(());
        };
        row.uuid.clone()
    };

    if timeline {
        print_run_info(&sdb, &uuid_prefix);
        let run_filter = resolve_run_filter(&sdb, &uuid_prefix, q.run.as_deref())?;

        let mut samples = sdb.query_samples(&uuid_prefix, run_filter)?;
        if samples.is_empty() {
            output::result_msg("no sidecar data for this result");
            return Ok(());
        }
        if summary {
            let markers = sdb.query_markers(&uuid_prefix, run_filter)?;
            print_phase_summary(&samples, &markers, q.human);
            return Ok(());
        }
        if let Some(ref phase_name) = q.phase {
            let markers = sdb.query_markers(&uuid_prefix, run_filter)?;
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
        return Ok(());
    }

    // q.markers
    print_run_info(&sdb, &uuid_prefix);
    let run_filter = resolve_run_filter(&sdb, &uuid_prefix, q.run.as_deref())?;
    let markers = sdb.query_markers(&uuid_prefix, run_filter)?;

    if q.counters {
        let counters = sdb.query_counters(&uuid_prefix, run_filter)?;
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
        let samples = sdb.query_samples(&uuid_prefix, run_filter)?;
        let counters = sdb.query_counters(&uuid_prefix, run_filter)?;
        print_marker_phases_with_counters(&markers, &samples, &counters);
    } else if q.durations {
        print_marker_durations(&markers);
    } else {
        for m in &markers {
            println!("{}", sidecar_marker_json(m));
        }
    }
    Ok(())
}
