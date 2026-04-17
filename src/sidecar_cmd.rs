//! Implementation of the `sidecar` command — per-phase summary, raw
//! sample/marker export, START/END durations, counters, single-field
//! stats, and two-result phase compare. Each invocation picks exactly
//! one view; clap enforces the mutual exclusion.

use std::path::Path;

use crate::db;
use crate::error::DevError;
use crate::output;
use crate::request::SidecarQuery;
use crate::resolve::sidecar_db_path;
use crate::sidecar_fmt::{
    apply_timeline_filter, parse_time_range, print_compare_timeline, print_counters,
    print_field_stat, print_marker_durations, print_phase_summary, print_run_info,
    resolve_phase_range, sidecar_marker_json, sidecar_sample_json_projected,
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

pub(crate) fn cmd_sidecar(project_root: &Path, q: &SidecarQuery) -> Result<(), DevError> {
    let Some(sdb) = open_sidecar_db(project_root) else {
        output::result_msg("no sidecar.db found");
        return Ok(());
    };

    // --compare A B takes a different path: no single UUID, two full timelines.
    if let Some(ref uuids) = q.compare {
        return run_compare(&sdb, uuids, q.human);
    }

    // Clap's `required_unless_present = "compare"` guarantees query is set
    // whenever we get here.
    let uuid_prefix = q
        .query
        .clone()
        .expect("clap required_unless_present guarantees query is set");

    if q.samples {
        return run_samples(&sdb, &uuid_prefix, q);
    }
    if q.markers {
        return run_markers(&sdb, &uuid_prefix, q);
    }
    if q.durations {
        return run_durations(&sdb, &uuid_prefix, q);
    }
    if q.counters {
        return run_counters(&sdb, &uuid_prefix, q);
    }
    if q.stalls {
        return run_stalls(&sdb, &uuid_prefix, q);
    }
    if let Some(ref field) = q.stat {
        return run_stat(&sdb, &uuid_prefix, field, q);
    }

    // Default view: per-phase summary (JSONL, or a fixed-width table with --human).
    run_phase_summary(&sdb, &uuid_prefix, q)
}

fn run_phase_summary(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_run_info(sdb, uuid_prefix);
    let run_filter = resolve_run_filter(sdb, uuid_prefix, q.run.as_deref())?;
    let samples = sdb.query_samples(uuid_prefix, run_filter)?;
    if samples.is_empty() {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    }
    let markers = sdb.query_markers(uuid_prefix, run_filter)?;
    print_phase_summary(&samples, &markers, q.human);
    Ok(())
}

fn run_samples(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_run_info(sdb, uuid_prefix);
    let run_filter = resolve_run_filter(sdb, uuid_prefix, q.run.as_deref())?;
    let mut samples = sdb.query_samples(uuid_prefix, run_filter)?;
    if samples.is_empty() {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    }

    if let Some(ref phase_name) = q.phase {
        let markers = sdb.query_markers(uuid_prefix, run_filter)?;
        let (start_us, end_us) = resolve_phase_range(phase_name, &markers, &samples)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }
    if let Some(ref range_str) = q.range {
        let (start_us, end_us) = parse_time_range(range_str)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }

    let filtered = apply_timeline_filter(&samples, q);
    let fields = if q.fields.is_empty() {
        None
    } else {
        Some(&q.fields)
    };
    for s in &filtered {
        println!("{}", sidecar_sample_json_projected(s, fields));
    }
    Ok(())
}

fn run_markers(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_run_info(sdb, uuid_prefix);
    let run_filter = resolve_run_filter(sdb, uuid_prefix, q.run.as_deref())?;
    let markers = sdb.query_markers(uuid_prefix, run_filter)?;
    if markers.is_empty() {
        output::result_msg("no sidecar markers for this result");
        return Ok(());
    }
    for m in &markers {
        println!("{}", sidecar_marker_json(m));
    }
    Ok(())
}

fn run_durations(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_run_info(sdb, uuid_prefix);
    let run_filter = resolve_run_filter(sdb, uuid_prefix, q.run.as_deref())?;
    let markers = sdb.query_markers(uuid_prefix, run_filter)?;
    if markers.is_empty() {
        output::result_msg("no sidecar markers for this result");
        return Ok(());
    }
    print_marker_durations(&markers, q.human);
    Ok(())
}

fn run_stalls(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_run_info(sdb, uuid_prefix);
    let run_filter = resolve_run_filter(sdb, uuid_prefix, q.run.as_deref())?;
    let markers = sdb.query_markers(uuid_prefix, run_filter)?;
    if markers.is_empty() {
        output::result_msg("no sidecar markers for this result");
        return Ok(());
    }
    let samples = sdb.query_samples(uuid_prefix, run_filter)?;
    let wall_us = samples
        .first()
        .zip(samples.last())
        .map_or(0, |(a, b)| b.timestamp_us - a.timestamp_us);
    crate::sidecar_fmt::print_stalls(&markers, wall_us, q.human);
    Ok(())
}

fn run_counters(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_run_info(sdb, uuid_prefix);
    let run_filter = resolve_run_filter(sdb, uuid_prefix, q.run.as_deref())?;
    let mut counters = sdb.query_counters(uuid_prefix, run_filter)?;
    if counters.is_empty() {
        output::result_msg("no counters for this result");
        return Ok(());
    }
    if let Some(ref phase_name) = q.phase {
        let markers = sdb.query_markers(uuid_prefix, run_filter)?;
        let samples = sdb.query_samples(uuid_prefix, run_filter)?;
        let (start_us, end_us) =
            crate::sidecar_fmt::resolve_phase_range(phase_name, &markers, &samples)?;
        counters.retain(|c| c.timestamp_us >= start_us && c.timestamp_us < end_us);
    }
    print_counters(&counters, q.human);
    Ok(())
}

fn run_stat(
    sdb: &db::sidecar::SidecarDb,
    uuid_prefix: &str,
    field: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_run_info(sdb, uuid_prefix);
    let run_filter = resolve_run_filter(sdb, uuid_prefix, q.run.as_deref())?;
    let mut samples = sdb.query_samples(uuid_prefix, run_filter)?;
    if samples.is_empty() {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    }
    if let Some(ref phase_name) = q.phase {
        let markers = sdb.query_markers(uuid_prefix, run_filter)?;
        let (start_us, end_us) = resolve_phase_range(phase_name, &markers, &samples)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }
    if let Some(ref range_str) = q.range {
        let (start_us, end_us) = parse_time_range(range_str)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }
    let filtered = apply_timeline_filter(&samples, q);
    print_field_stat(&filtered, field)
}

fn run_compare(
    sdb: &db::sidecar::SidecarDb,
    uuids: &[String],
    human: bool,
) -> Result<(), DevError> {
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
        uuid_a, &samples_a, &markers_a, uuid_b, &samples_b, &markers_b, human,
    );
    Ok(())
}
