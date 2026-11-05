//! The single-run views, written once over a [`Source`]: for a typed run each
//! reads, warns and prints in the order it always did.

use super::source::Source;
use crate::error::DevError;
use crate::output;
use crate::request::SidecarQuery;
use crate::sidecar::Sample;
use crate::sidecar_fmt::{
    CallStreams, parse_time_range, print_call_timelines, print_counters, print_field_stat,
    print_marker_durations, print_phase_summary, resolve_phase_range, sidecar_marker_json,
    sidecar_sample_json_projected, stream_markers, timeline_filter_indices,
};

/// A view whose answer is one process timeline: concatenated runs would make
/// one timeline, and one wall, out of several processes.
fn one_run(run_filter: Option<usize>, what: &str) -> Result<usize, DevError> {
    run_filter.ok_or_else(|| {
        DevError::Usage(format!(
            "{what} reads one run; pass --run N instead of --run all"
        ))
    })
}

pub(super) fn phase_summary(src: &Source<'_>, q: &SidecarQuery) -> Result<(), DevError> {
    src.print_info()?;
    let run_filter = src.run_filter(q.run.as_deref())?;
    let run = one_run(run_filter, "the per-phase summary")?;
    src.check_call(run_filter, q.call.as_deref())?;
    let markers = src.markers(run_filter)?;
    let calls = src.calls(run_filter)?;
    let sink_ends = src.sink_ends(run_filter)?;
    let process = src.process(run);
    if let Some(call) = q.call.as_deref() {
        let streams = CallStreams {
            markers: &stream_markers(&markers, Some(call)),
            calls: &calls
                .iter()
                .filter(|s| s.call == call)
                .cloned()
                .collect::<Vec<_>>(),
            sink_ends: &sink_ends
                .iter()
                .filter(|s| s.call == call)
                .cloned()
                .collect::<Vec<_>>(),
        };
        print_call_timelines(&streams, process, q.human);
        return Ok(());
    }
    // Samples gate a typed run's process resource table only; a call's
    // marker intervals stand without them. A bench child's interval is
    // known, so its phases stand without samples too.
    let (samples, _) = src.samples(run_filter)?;
    if samples.is_empty() && process.wall_us().is_none() {
        output::result_msg("no sidecar samples for this result");
    } else {
        print_phase_summary(&samples, &markers, process, q.human);
    }
    print_call_timelines(
        &CallStreams {
            markers: &markers,
            calls: &calls,
            sink_ends: &sink_ends,
        },
        process,
        q.human,
    );
    Ok(())
}

/// `--phase` for the sample and counter views, in the selected stream of one
/// run. `--call` without `--phase` cannot select samples: no call window is
/// known.
fn phase_window(
    src: &Source<'_>,
    run_filter: Option<usize>,
    q: &SidecarQuery,
    samples: &[Sample],
) -> Result<Option<(i64, i64)>, DevError> {
    let Some(ref phase_name) = q.phase else {
        return Ok(None);
    };
    let run = one_run(run_filter, "--phase")?;
    let markers = src.markers(run_filter)?;
    let calls = src.calls(run_filter)?;
    let span = q
        .call
        .as_deref()
        .and_then(|call| calls.iter().find(|s| s.call == call));
    resolve_phase_range(
        phase_name,
        &markers,
        samples,
        q.call.as_deref(),
        span,
        src.process(run),
    )
    .map(Some)
}

fn sample_call_needs_phase(q: &SidecarQuery) -> Result<(), DevError> {
    if q.call.is_some() && q.phase.is_none() {
        return Err(DevError::Usage(
            "--call with --samples or --stat needs --phase: samples are process-wide and \
             no call window is known"
                .into(),
        ));
    }
    Ok(())
}

/// The samples of the selected runs with each one's run, narrowed to the
/// `--phase` window and the `--range`; the phase is validated before any
/// empty result can return. `None` when there are no samples at all.
#[allow(clippy::type_complexity)]
fn windowed_samples(
    src: &Source<'_>,
    run_filter: Option<usize>,
    q: &SidecarQuery,
) -> Result<Option<(Vec<Sample>, Vec<Option<usize>>)>, DevError> {
    let (samples, runs) = src.samples(run_filter)?;
    let window = phase_window(src, run_filter, q, &samples)?;
    if samples.is_empty() {
        return Ok(None);
    }
    let range = q.range.as_deref().map(parse_time_range).transpose()?;
    let inside = |s: &Sample, (start_us, end_us): (i64, i64)| {
        s.timestamp_us >= start_us && s.timestamp_us < end_us
    };
    let (samples, runs) = samples
        .into_iter()
        .zip(runs)
        .filter(|(s, _)| window.is_none_or(|w| inside(s, w)))
        .filter(|(s, _)| range.is_none_or(|r| inside(s, r)))
        .unzip();
    Ok(Some((samples, runs)))
}

#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
pub(super) fn samples(src: &Source<'_>, q: &SidecarQuery) -> Result<(), DevError> {
    src.print_info()?;
    sample_call_needs_phase(q)?;
    let run_filter = src.run_filter(q.run.as_deref())?;
    src.check_call(run_filter, q.call.as_deref())?;
    let Some((samples, runs)) = windowed_samples(src, run_filter, q)? else {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    };
    let fields = if q.fields.is_empty() {
        None
    } else {
        Some(&q.fields)
    };
    for i in timeline_filter_indices(&samples, q)? {
        println!(
            "{}",
            sidecar_sample_json_projected(&samples[i], fields, runs[i])
        );
    }
    Ok(())
}

/// The selected runs' markers, narrowed to one call when `--call` is given.
fn call_markers(
    src: &Source<'_>,
    run_filter: Option<usize>,
    call: Option<&str>,
) -> Result<Vec<crate::sidecar::Marker>, DevError> {
    let mut markers = src.markers(run_filter)?;
    if let Some(call) = call {
        markers.retain(|m| m.call.as_deref() == Some(call));
    }
    Ok(markers)
}

#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
pub(super) fn markers(src: &Source<'_>, q: &SidecarQuery) -> Result<(), DevError> {
    src.print_info()?;
    let run_filter = src.run_filter(q.run.as_deref())?;
    src.check_call(run_filter, q.call.as_deref())?;
    let markers = call_markers(src, run_filter, q.call.as_deref())?;
    if markers.is_empty() {
        output::result_msg("no sidecar markers for this result");
        return Ok(());
    }
    for m in &markers {
        println!("{}", sidecar_marker_json(m));
    }
    Ok(())
}

/// START/END spans per stream. A library call's stored boundaries are not
/// read here, deliberately: a span names its own two ends, so it needs no
/// return to close it, and one that crosses the return is what the harness
/// marked (`docs/commands/measure.md`, Marker and counter rules).
pub(super) fn durations(src: &Source<'_>, q: &SidecarQuery) -> Result<(), DevError> {
    src.print_info()?;
    let run_filter = src.run_filter(q.run.as_deref())?;
    src.check_call(run_filter, q.call.as_deref())?;
    let markers = call_markers(src, run_filter, q.call.as_deref())?;
    if markers.is_empty() {
        output::result_msg("no sidecar markers for this result");
        return Ok(());
    }
    // Pairing and collapsing run per (run, call) stream.
    print_marker_durations(&markers, q.human);
    Ok(())
}

pub(super) fn stalls(src: &Source<'_>, q: &SidecarQuery) -> Result<(), DevError> {
    src.print_info()?;
    let run_filter = src.run_filter(q.run.as_deref())?;
    let run = one_run(run_filter, "--stalls")?;
    src.check_call(run_filter, q.call.as_deref())?;
    // Stalls roll up cumulative `*_wait_ns` counters (not markers - markers are
    // reserved for phase boundaries). print_stalls handles the empty case.
    let mut counters = src.counters(run_filter)?;
    if let Some(call) = q.call.as_deref() {
        counters.retain(|c| c.call.as_deref() == Some(call));
    }
    let (samples, _) = src.samples(run_filter)?;
    let wall_us = src.stall_wall_us(run, &samples);
    let calls = src.calls(run_filter)?;
    crate::sidecar_fmt::print_stalls(&counters, wall_us, &calls, q.human);
    Ok(())
}

pub(super) fn counters(src: &Source<'_>, q: &SidecarQuery) -> Result<(), DevError> {
    src.print_info()?;
    let run_filter = src.run_filter(q.run.as_deref())?;
    src.check_call(run_filter, q.call.as_deref())?;
    // The phase is validated before any empty result can return.
    let window = if q.phase.is_some() {
        let (samples, _) = src.samples(run_filter)?;
        phase_window(src, run_filter, q, &samples)?
    } else {
        None
    };
    let mut counters = src.counters(run_filter)?;
    if let Some(call) = q.call.as_deref() {
        counters.retain(|c| c.call.as_deref() == Some(call));
        if counters.is_empty() {
            output::result_msg(&format!("call '{call}' emitted no counters"));
            return Ok(());
        }
    }
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
    if let Some((start_us, end_us)) = window {
        // The window belongs to the stream it resolved in; another call's
        // counters inside the same interval are not that phase's.
        let stream = q.call.as_deref();
        counters.retain(|c| {
            c.call.as_deref() == stream && c.timestamp_us >= start_us && c.timestamp_us < end_us
        });
    }
    print_counters(&counters, q.human);
    Ok(())
}

pub(super) fn stat(src: &Source<'_>, field: &str, q: &SidecarQuery) -> Result<(), DevError> {
    src.print_info()?;
    sample_call_needs_phase(q)?;
    let run_filter = src.run_filter(q.run.as_deref())?;
    src.check_call(run_filter, q.call.as_deref())?;
    let Some((samples, _)) = windowed_samples(src, run_filter, q)? else {
        output::result_msg("no sidecar data for this result");
        return Ok(());
    };
    let filtered: Vec<&Sample> = timeline_filter_indices(&samples, q)?
        .into_iter()
        .map(|i| &samples[i])
        .collect();
    print_field_stat(&filtered, field)
}
