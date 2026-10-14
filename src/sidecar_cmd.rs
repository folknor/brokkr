//! Implementation of the `sidecar` command - per-phase summary, raw
//! sample/marker export, START/END durations, counters, single-field
//! stats, and two-result phase compare. Each invocation picks exactly
//! one view; clap enforces the mutual exclusion.

use std::path::Path;
use std::sync::OnceLock;

use crate::db;
use crate::error::DevError;
use crate::output;
use crate::request::{SidecarQuery, SidecarView};
use crate::resolve::{results_db_path, sidecar_db_path};
use crate::sidecar_fmt::{
    apply_timeline_filter, parse_time_range, print_compare_timeline, print_counters,
    print_field_stat, print_marker_durations, print_phase_summary, print_run_info,
    resolve_phase_range, sidecar_marker_json, sidecar_sample_json_projected,
    validate_timeline_args,
};

/// A compact, current list of sidecar view selectors for results output.
/// Selectors are the options whose clap conflict set contains the other view
/// choices; command modifiers such as `--run`, `--phase` and `--human` stay out
/// of the hint automatically.
pub(crate) fn view_selector_hint() -> &'static str {
    static HINT: OnceLock<String> = OnceLock::new();
    HINT.get_or_init(|| {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(|| {
                use clap::CommandFactory;

                let mut cli = crate::cli::Cli::command();
                let Some(command) = cli.find_subcommand_mut("sidecar") else {
                    return "see `brokkr sidecar --help` for available views".to_owned();
                };
                let selectors = command
                    .get_arguments()
                    .filter(|arg| arg.get_long().is_some())
                    .filter(|arg| {
                        command
                            .get_arg_conflicts_with(arg)
                            .iter()
                            .filter(|other| other.get_long().is_some())
                            .count()
                            >= 3
                    })
                    .filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
                    .collect::<Vec<_>>();
                if selectors.is_empty() {
                    "see `brokkr sidecar --help` for available views".to_owned()
                } else {
                    format!("views: {}", selectors.join(", "))
                }
            })
            .ok()
            .and_then(|worker| worker.join().ok())
            .unwrap_or_else(|| "see `brokkr sidecar --help` for available views".to_owned())
    })
}

/// Resolve `--run`: `"all"` → None (all runs), N → Some(N), absent → best run.
/// Also logs which run(s) will be shown when the result has multiple runs.
fn resolve_run_filter(
    store: &db::RunStore,
    id: &db::RunId,
    run_arg: Option<&str>,
) -> Result<Option<usize>, DevError> {
    let (best_idx, total) = store.query_sidecar_meta(id)?;
    let run_filter = match run_arg {
        Some("all") => None,
        Some(n) => Some(n.parse::<usize>().map_err(|_| {
            DevError::Usage(format!("--run: expected a number or 'all', got '{n}'"))
        })?),
        None => Some(best_idx),
    };
    warn_if_capture_truncated(store, id, run_filter)?;
    if total > 1 {
        let showing = match run_filter {
            Some(idx) => format!("run {idx}/{total}"),
            None => format!("all {total} runs"),
        };
        output::sidecar_msg(&format!("showing {showing} (use --run to override)"));
    }
    Ok(run_filter)
}

fn warn_if_capture_truncated(
    store: &db::RunStore,
    id: &db::RunId,
    run_idx: Option<usize>,
) -> Result<(), DevError> {
    if store.query_sidecar_capture_truncated(id, run_idx)? {
        output::warn("stored sidecar trajectory is incomplete because a capture limit was reached");
    }
    Ok(())
}

pub(crate) fn cmd_sidecar(project_root: &Path, q: &SidecarQuery) -> Result<(), DevError> {
    // Malformed filters fail before anything is printed: `--where`/`--fields`
    // used to fail open (a bad condition printed every sample, an unknown
    // field silently vanished), which reads as a real answer.
    validate_timeline_args(q)?;
    let store = db::RunStore::open_read_only(
        &results_db_path(project_root),
        &sidecar_db_path(project_root),
    )?;
    if !store.has_sidecar_db() {
        output::result_msg("no sidecar.db found");
        return Ok(());
    }
    match &q.view {
        SidecarView::Compare { uuids: (a, b) } => run_compare(&store, a, b, q.human),
        SidecarView::Summary { uuid } => {
            with_resolved(&store, uuid, |id| run_phase_summary(&store, id, q))
        }
        SidecarView::Samples { uuid } => {
            with_resolved(&store, uuid, |id| run_samples(&store, id, q))
        }
        SidecarView::Markers { uuid } => {
            with_resolved(&store, uuid, |id| run_markers(&store, id, q))
        }
        SidecarView::Durations { uuid } => {
            with_resolved(&store, uuid, |id| run_durations(&store, id, q))
        }
        SidecarView::Counters { uuid } => {
            with_resolved(&store, uuid, |id| run_counters(&store, id, q))
        }
        SidecarView::Stalls { uuid } => with_resolved(&store, uuid, |id| run_stalls(&store, id, q)),
        SidecarView::Stat { uuid } => {
            let field = q
                .stat
                .as_deref()
                .ok_or_else(|| DevError::Usage("sidecar stat view requires a field".into()))?;
            with_resolved(&store, uuid, |id| run_stat(&store, id, field, q))
        }
    }
}

fn print_resolved_run_info(store: &db::RunStore, id: &db::RunId) -> Result<(), DevError> {
    print_run_info(store, id)
}

fn with_resolved(
    store: &db::RunStore,
    prefix: &str,
    run: impl FnOnce(&db::RunId) -> Result<(), DevError>,
) -> Result<(), DevError> {
    match resolve_id(store, prefix)? {
        Some(id) => run(&id),
        None => {
            output::result_msg("no sidecar data for this result");
            Ok(())
        }
    }
}

fn resolve_id(store: &db::RunStore, prefix: &str) -> Result<Option<db::RunId>, DevError> {
    match store.resolve(db::RunRef::Uuid(prefix))? {
        db::Resolved::One(id) => Ok(Some(id)),
        db::Resolved::Missing => Ok(None),
        db::Resolved::Ambiguous(ids) => {
            const SHOWN: usize = 10;
            let listed = ids
                .iter()
                .take(SHOWN)
                .map(db::RunId::as_str)
                .collect::<Vec<_>>();
            let more = ids.len().saturating_sub(SHOWN);
            let tail = if more > 0 {
                format!(", and {more} more")
            } else {
                String::new()
            };
            Err(DevError::Config(format!(
                "uuid prefix '{prefix}' is ambiguous - it matches {} runs: {}{tail}",
                ids.len(),
                listed.join(", "),
            )))
        }
    }
}

fn run_phase_summary(
    store: &db::RunStore,
    id: &db::RunId,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_resolved_run_info(store, id)?;
    let run_filter = resolve_run_filter(store, id, q.run.as_deref())?;
    let samples = store.query_sidecar_samples(id, run_filter)?;
    if samples.is_empty() {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    }
    let markers = store.query_sidecar_markers(id, run_filter)?;
    print_phase_summary(&samples, &markers, q.human);
    Ok(())
}

fn run_samples(store: &db::RunStore, id: &db::RunId, q: &SidecarQuery) -> Result<(), DevError> {
    print_resolved_run_info(store, id)?;
    let run_filter = resolve_run_filter(store, id, q.run.as_deref())?;
    let mut samples = store.query_sidecar_samples(id, run_filter)?;
    if samples.is_empty() {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    }

    if let Some(ref phase_name) = q.phase {
        let markers = store.query_sidecar_markers(id, run_filter)?;
        let (start_us, end_us) = resolve_phase_range(phase_name, &markers, &samples)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }
    if let Some(ref range_str) = q.range {
        let (start_us, end_us) = parse_time_range(range_str)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }

    let filtered = apply_timeline_filter(&samples, q)?;
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

fn run_markers(store: &db::RunStore, id: &db::RunId, q: &SidecarQuery) -> Result<(), DevError> {
    print_resolved_run_info(store, id)?;
    let run_filter = resolve_run_filter(store, id, q.run.as_deref())?;
    let markers = store.query_sidecar_markers(id, run_filter)?;
    if markers.is_empty() {
        output::result_msg("no sidecar markers for this result");
        return Ok(());
    }
    for m in &markers {
        println!("{}", sidecar_marker_json(m));
    }
    Ok(())
}

fn run_durations(store: &db::RunStore, id: &db::RunId, q: &SidecarQuery) -> Result<(), DevError> {
    print_resolved_run_info(store, id)?;
    let run_filter = resolve_run_filter(store, id, q.run.as_deref())?;
    let markers = store.query_sidecar_markers(id, run_filter)?;
    if markers.is_empty() {
        output::result_msg("no sidecar markers for this result");
        return Ok(());
    }
    // Only collapse repeated names within a single run. `--run all` concatenates
    // every run's markers and `Marker` carries no run_idx, so a once-per-run
    // phase would look like a repeated span and get aggregated away.
    let collapse = run_filter.is_some();
    print_marker_durations(&markers, q.human, collapse);
    Ok(())
}

fn run_stalls(store: &db::RunStore, id: &db::RunId, q: &SidecarQuery) -> Result<(), DevError> {
    print_resolved_run_info(store, id)?;
    let run_filter = resolve_run_filter(store, id, q.run.as_deref())?;
    // Stalls roll up cumulative `*_wait_ns` counters (not markers - markers are
    // reserved for phase boundaries). print_stalls handles the empty case.
    let counters = store.query_sidecar_counters(id, run_filter)?;
    let samples = store.query_sidecar_samples(id, run_filter)?;
    let wall_us = samples
        .first()
        .zip(samples.last())
        .map_or(0, |(a, b)| b.timestamp_us - a.timestamp_us);
    crate::sidecar_fmt::print_stalls(&counters, wall_us, q.human);
    Ok(())
}

fn run_counters(store: &db::RunStore, id: &db::RunId, q: &SidecarQuery) -> Result<(), DevError> {
    print_resolved_run_info(store, id)?;
    let run_filter = resolve_run_filter(store, id, q.run.as_deref())?;
    let mut counters = store.query_sidecar_counters(id, run_filter)?;
    if counters.is_empty() {
        output::result_msg("no counters for this result");
        return Ok(());
    }
    if let Some(ref needle) = q.grep {
        counters.retain(|c| c.name.contains(needle.as_str()));
        if counters.is_empty() {
            output::result_msg(&format!("no counters matching '{needle}'"));
            return Ok(());
        }
    }
    if let Some(ref phase_name) = q.phase {
        let markers = store.query_sidecar_markers(id, run_filter)?;
        let samples = store.query_sidecar_samples(id, run_filter)?;
        let (start_us, end_us) =
            crate::sidecar_fmt::resolve_phase_range(phase_name, &markers, &samples)?;
        counters.retain(|c| c.timestamp_us >= start_us && c.timestamp_us < end_us);
    }
    print_counters(&counters, q.human);
    Ok(())
}

fn run_stat(
    store: &db::RunStore,
    id: &db::RunId,
    field: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    print_resolved_run_info(store, id)?;
    let run_filter = resolve_run_filter(store, id, q.run.as_deref())?;
    let mut samples = store.query_sidecar_samples(id, run_filter)?;
    if samples.is_empty() {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    }
    if let Some(ref phase_name) = q.phase {
        let markers = store.query_sidecar_markers(id, run_filter)?;
        let (start_us, end_us) = resolve_phase_range(phase_name, &markers, &samples)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }
    if let Some(ref range_str) = q.range {
        let (start_us, end_us) = parse_time_range(range_str)?;
        samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
    }
    let filtered = apply_timeline_filter(&samples, q)?;
    print_field_stat(&filtered, field)
}

fn run_compare(
    store: &db::RunStore,
    uuid_a: &str,
    uuid_b: &str,
    human: bool,
) -> Result<(), DevError> {
    // Each side must name exactly one run across both stores.
    let Some(id_a) = resolve_id(store, uuid_a)? else {
        output::result_msg("one or both results have no sidecar data");
        return Ok(());
    };
    let Some(id_b) = resolve_id(store, uuid_b)? else {
        output::result_msg("one or both results have no sidecar data");
        return Ok(());
    };
    let (best_a, _) = store.query_sidecar_meta(&id_a)?;
    let (best_b, _) = store.query_sidecar_meta(&id_b)?;
    warn_if_capture_truncated(store, &id_a, Some(best_a))?;
    warn_if_capture_truncated(store, &id_b, Some(best_b))?;
    let samples_a = store.query_sidecar_samples(&id_a, Some(best_a))?;
    let samples_b = store.query_sidecar_samples(&id_b, Some(best_b))?;
    if samples_a.is_empty() || samples_b.is_empty() {
        output::result_msg("one or both results have no sidecar data");
        return Ok(());
    }
    let markers_a = store.query_sidecar_markers(&id_a, Some(best_a))?;
    let markers_b = store.query_sidecar_markers(&id_b, Some(best_b))?;
    print_compare_timeline(
        &id_a, &samples_a, &markers_a, &id_b, &samples_b, &markers_b, human,
    );
    Ok(())
}
