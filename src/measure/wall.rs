//! Timing conversions, ranking and distribution summaries.

use super::BenchResult;
use crate::db::{Distribution, Wall};
use crate::sidecar::linear_percentile as percentile;
use std::time::Duration;

/// The stored wall for a harness result: exact when the path measured
/// microseconds, otherwise the millisecond-only reading.
pub(super) fn result_wall(elapsed_ms: i64, elapsed_us: Option<i64>) -> Wall {
    Wall::from_columns(elapsed_ms, elapsed_us)
}

/// Convert a `Duration` to milliseconds as `i64`, rounded to nearest.
///
/// Nearest, not floored, for the reason [`us_to_ms`] gives: flooring reads
/// every run as up to a whole millisecond faster than it was. Saturates at
/// `i64::MAX`.
pub fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_micros().saturating_add(500) / 1000).unwrap_or(i64::MAX)
}

/// Convert a `Duration` to whole microseconds as `i64`. Saturates at
/// `i64::MAX`.
///
/// The externally timed paths measure an exact `Duration`; this is what
/// they record as `elapsed_us`, so best-of-N compares microseconds rather
/// than millisecond ties.
pub fn elapsed_to_us(duration: &Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

/// Summarise distribution samples, given in microseconds in any order, as
/// min/p50/p95/max. Returns the summary and the fastest sample, or `None`
/// when there are no samples.
///
/// The percentiles are taken over the microseconds and kept exact; the
/// millisecond column is derived from them by [`Wall::ms`]'s nearest
/// rounding, so the two columns of one row can never disagree.
pub(super) fn summarize_distribution(samples_us: &[i64]) -> Option<(Distribution, i64)> {
    if samples_us.is_empty() {
        return None;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    let min = percentile(&sorted, 0);
    let dist = Distribution {
        samples: i64::try_from(sorted.len()).unwrap_or(i64::MAX),
        min: Wall::from_micros(min),
        p50: Wall::from_micros(percentile(&sorted, 50)),
        p95: Wall::from_micros(percentile(&sorted, 95)),
        max: Wall::from_micros(percentile(&sorted, 100)),
    };
    Some((dist, min))
}

/// Keep the first minimum, comparing exact walls where available.
pub(super) fn winning_index(
    current: Option<(usize, &BenchResult)>,
    index: usize,
    candidate: &BenchResult,
) -> usize {
    match current {
        Some((winner, best)) if bench_ordering_key(best) <= bench_ordering_key(candidate) => winner,
        _ => index,
    }
}

/// Comparable wall for best-of-N, in microseconds.
pub(super) fn bench_ordering_key(result: &BenchResult) -> i64 {
    result
        .elapsed_us
        .unwrap_or_else(|| result.elapsed_ms.saturating_mul(1000))
}

/// Round microseconds to the nearest millisecond.
///
/// Nearest, not truncating: a 6.847 ms region is 7 ms, not 6. `elapsed_ms`
/// is a display and back-compat value once `elapsed_us` exists, so being off
/// by a whole millisecond in the direction of "faster than it was" is the
/// one error worth avoiding.
pub(super) fn us_to_ms(us: i64) -> i64 {
    // Saturating: `elapsed_to_us` saturates at `i64::MAX`, and the plain
    // addition would then overflow.
    us.saturating_add(500) / 1000
}

/// Per-iteration microsecond walls, kept only when every iteration had one.
///
/// A list with gaps cannot stay index-aligned with the millisecond walls, and
/// the alignment - execution order - is the only reason the list exists, so a
/// single missing reading drops the whole list rather than part of it.
pub(super) fn all_or_none(walls_us: Vec<Option<i64>>) -> Vec<i64> {
    walls_us
        .into_iter()
        .collect::<Option<Vec<i64>>>()
        .unwrap_or_default()
}

/// A fixed timing policy for every iteration in one measurement.
#[derive(Clone, Copy)]
pub enum WallSource {
    External,
    SelfReported,
    Closure,
    /// All iterations must have the requested span. Missing markers fail the
    /// measurement rather than comparing a span against a whole-process wall.
    MarkerSpan {
        start: &'static str,
        end: &'static str,
    },
}

/// Last start and first later end, in time order (arrival order breaking
/// ties), preserving exact marker microseconds. Only the process stream
/// counts: a call's START paired with another call's END would be a span
/// nothing ran.
pub(crate) fn marker_span_us(
    markers: &[crate::sidecar::Marker],
    start: &str,
    end: &str,
) -> Option<i64> {
    let mut process: Vec<&crate::sidecar::Marker> =
        markers.iter().filter(|marker| marker.call.is_none()).collect();
    process.sort_by_key(|marker| (marker.timestamp_us, marker.marker_idx));
    let begin = process.iter().rfind(|marker| marker.name == start)?;
    let finish = process
        .iter()
        .find(|marker| marker.name == end && marker.timestamp_us > begin.timestamp_us)?;
    finish.timestamp_us.checked_sub(begin.timestamp_us)
}
