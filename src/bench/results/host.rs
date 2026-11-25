//! How a run's host state reads: each iteration's trace, the gap from the
//! cell before it, and what differs between two result iterations.

use crate::bench::host_trace::{self, Gap, Window};
use crate::bench::store::{Conditions, HostTrace, Iteration, Reading};

fn mb(kb: i64) -> String {
    #[allow(clippy::cast_precision_loss)] // a display figure
    let mb = kb as f64 / 1024.0;
    format!("{mb:.1} MB")
}

fn opt_mb(kb: Option<i64>) -> String {
    kb.map_or_else(|| "?".to_owned(), mb)
}

fn seconds(us: i64) -> String {
    #[allow(clippy::cast_precision_loss)] // a display figure
    let s = us as f64 / 1_000_000.0;
    format!("{s:.1} s")
}

fn gauges(r: &Reading) -> String {
    format!(
        "Dirty {}, Writeback {}",
        opt_mb(r.dirty_kb),
        opt_mb(r.writeback_kb)
    )
}

fn device_part(w: &Window) -> Option<String> {
    let written = w.written_kb?;
    let mut text = format!("device wrote {}", mb(written));
    if let Some(discarded) = w.discarded_kb {
        text.push_str(&format!(", discarded {}", mb(discarded)));
    }
    if let Some(wait) = w.write_wait_ms {
        text.push_str(&format!(", mean write wait {wait:.2} ms per request"));
    }
    Some(text)
}

fn cell_name(cell: &crate::bench::store::Cell) -> String {
    format!(
        "{} {} iteration {} ({})",
        cell.project,
        cell.bench,
        cell.iteration,
        cell.run.chars().take(8).collect::<String>()
    )
}

/// The gap from the last recorded cell end, as a line.
pub(crate) fn gap_line(trace: &HostTrace) -> String {
    match host_trace::gap(trace) {
        Gap::None => "no earlier cell recorded on this host".to_owned(),
        Gap::Unended { cell } => format!(
            "after {}, which never recorded its end: the gap and what it did are unknown",
            cell_name(&cell)
        ),
        Gap::OtherBoot { cell } => format!(
            "after {}, in another boot or not shown to precede this one: no gap applies",
            cell_name(&cell)
        ),
        Gap::NoStart { cell } => format!(
            "after the end of {}; this iteration's start reading is missing, so the gap is unknown",
            cell_name(&cell)
        ),
        Gap::Ended {
            cell,
            elapsed_us,
            release_completed,
            device,
        } => {
            let mut text = format!(
                "{} after the end of {}",
                seconds(elapsed_us),
                cell_name(&cell)
            );
            if !release_completed {
                text.push_str(" (its release was deferred, cleanup may have followed)");
            }
            match device.as_ref().and_then(device_part) {
                Some(part) => text.push_str(&format!("; in the gap the {part}")),
                None => text.push_str("; device not shown to be the same instance, no delta"),
            }
            text
        }
    }
}

/// An iteration's host lines, for `brokkr results UUID`.
pub(crate) fn iteration_lines(iteration: &Iteration) -> Vec<String> {
    let Some(trace) = &iteration.host else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let device = trace.device.as_ref().map_or_else(
        || "no block device resolved".to_owned(),
        |d| {
            format!(
                "{} ({}, {}:{}{})",
                d.name,
                d.layer.name(),
                d.major,
                d.minor,
                d.diskseq
                    .map_or_else(String::new, |s| format!(", diskseq {s}"))
            )
        },
    );
    lines.push(format!("host      {device}"));
    lines.push(format!("  before  {}", gap_line(trace)));
    if let Some(start) = &trace.start {
        lines.push(format!("  start   {}", gauges(start)));
    }
    // The block's own readings when the periodic ones are loaded and the
    // block ran; otherwise the start and end readings, setup included.
    let block = trace
        .block_start_us
        .zip(iteration.wall_us)
        .filter(|_| !trace.readings.is_empty());
    let (label, window) = match block {
        Some((from, wall)) => (
            "block",
            host_trace::window(trace, from, from.saturating_add(wall)),
        ),
        None => (
            "around",
            host_trace::summarize(trace.start.iter().chain(&trace.end)),
        ),
    };
    let mut during = format!(
        "{} readings: peak Dirty {}, peak Writeback {}",
        window.readings,
        opt_mb(window.peak_dirty_kb),
        opt_mb(window.peak_writeback_kb)
    );
    if let Some(part) = device_part(&window) {
        during.push_str(&format!("; {part}"));
    }
    if label == "around" {
        during.push_str(" (start and end readings only, setup and extraction included)");
    }
    if trace.capped {
        during.push_str(" (the run's readings ran out: later ones are missing)");
    }
    lines.push(format!("  {label:<6}  {during}"));
    if !trace.cursor_written {
        lines.push(
            "  cursor  not written: a later cell may not see this one as its predecessor"
                .to_owned(),
        );
    }
    lines
}

/// The run's host history lines, for the context block.
pub(crate) fn condition_lines(conditions: &Conditions, started_unix_ms: i64) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(prev) = &conditions.previous_command {
        let ago = started_unix_ms / 1_000 - prev.ended_unix_s;
        lines.push(format!(
            "last brokkr command in this user's history, ended about {ago} s before: {}",
            prev.command
        ));
    }
    if let Some(at) = conditions.fstrim_timer_unix_ms {
        lines.push(format!(
            "fstrim.timer last triggered {} before (a timer firing, not a trim of scratch)",
            seconds((started_unix_ms - at).saturating_mul(1_000))
        ));
    }
    lines
}

/// What differs between two result iterations' host state, for the
/// `host:` line of `brokkr results --compare`.
pub(crate) fn compare_parts(a: &Iteration, b: &Iteration) -> Vec<String> {
    let (Some(ta), Some(tb)) = (&a.host, &b.host) else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    let gauge = |t: &HostTrace| t.start.as_ref().map(gauges);
    if gauge(ta) != gauge(tb) {
        parts.push(format!(
            "start {} vs {}",
            gauge(ta).unwrap_or_else(|| "(unread)".to_owned()),
            gauge(tb).unwrap_or_else(|| "(unread)".to_owned())
        ));
    }
    let (ga, gb) = (gap_line(ta), gap_line(tb));
    if ga != gb {
        parts.push(format!("before {ga} vs {gb}"));
    }
    parts
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::bench::store::{Cell, Cursor, Device, DeviceCounters, DeviceLayer};

    fn counters(written: i64, writes: i64, ticks: i64, discarded: i64) -> DeviceCounters {
        DeviceCounters {
            sectors_written: written,
            writes_completed: writes,
            write_ticks_ms: ticks,
            in_flight: 0,
            sectors_discarded: Some(discarded),
        }
    }

    fn reading(at_us: i64, c: DeviceCounters) -> Reading {
        Reading {
            at_us,
            dirty_kb: Some(2048),
            writeback_kb: Some(0),
            device: Some(c),
        }
    }

    fn device(diskseq: Option<u64>) -> Device {
        Device {
            name: "nvme0n1".to_owned(),
            major: 259,
            minor: 0,
            layer: DeviceLayer::Disk,
            diskseq,
        }
    }

    fn cell() -> Cell {
        Cell {
            run: "a".repeat(32),
            project: "pbfhogg".to_owned(),
            bench: "altw".to_owned(),
            iteration: 2,
            boot_id: Some("boot".to_owned()),
        }
    }

    fn trace(previous: Option<Cursor>, diskseq: Option<u64>) -> HostTrace {
        HostTrace {
            boot_id: Some("boot".to_owned()),
            device: Some(device(diskseq)),
            previous,
            cursor_written: true,
            block_start_us: Some(10_000_000),
            start: Some(reading(10_000_000, counters(4096, 10, 100, 2048))),
            end: Some(reading(20_000_000, counters(8192, 20, 300, 2048))),
            released: Some(reading(20_100_000, counters(8192, 20, 300, 2048))),
            release_completed: true,
            readings: Vec::new(),
            capped: false,
        }
    }

    fn ended(diskseq: Option<u64>) -> Cursor {
        Cursor::Ended {
            cell: cell(),
            device: Some(device(diskseq)),
            reading: reading(5_000_000, counters(2048, 5, 50, 0)),
            release_completed: true,
        }
    }

    #[test]
    fn gap_reports_elapsed_and_device_deltas_for_one_instance() {
        let line = gap_line(&trace(Some(ended(Some(7))), Some(7)));
        assert!(
            line.starts_with("5.0 s after the end of pbfhogg altw iteration 2"),
            "{line}"
        );
        assert!(
            line.contains("device wrote 1.0 MB, discarded 1.0 MB"),
            "{line}"
        );
    }

    #[test]
    fn gap_withholds_deltas_without_a_matching_diskseq() {
        for (old, new) in [(None, None), (Some(7), None), (Some(7), Some(8))] {
            let line = gap_line(&trace(Some(ended(old)), new));
            assert!(line.contains("no delta"), "{line}");
        }
    }

    #[test]
    fn unended_and_other_boot_predecessors_claim_no_gap() {
        let line = gap_line(&trace(Some(Cursor::InProgress { cell: cell() }), Some(7)));
        assert!(line.contains("never recorded its end"), "{line}");
        let mut other = trace(Some(ended(Some(7))), Some(7));
        other.boot_id = Some("other".to_owned());
        assert!(gap_line(&other).contains("another boot"));
    }

    /// A start reading lost to a failed clock read still names the recorded
    /// predecessor; only the gap's size is unknown.
    #[test]
    fn a_missing_start_names_the_predecessor_with_an_unknown_gap() {
        let mut ended_before = trace(Some(ended(Some(7))), Some(7));
        ended_before.start = None;
        let line = gap_line(&ended_before);
        assert!(line.contains("start reading is missing"), "{line}");
        let mut unended = trace(Some(Cursor::InProgress { cell: cell() }), Some(7));
        unended.start = None;
        assert!(gap_line(&unended).contains("never recorded its end"));
    }

    #[test]
    fn window_derives_write_wait_and_breaks_on_a_regression() {
        let t = trace(None, None);
        let w = host_trace::window(&t, i64::MIN, i64::MAX);
        assert_eq!(w.written_kb, Some(2048));
        assert_eq!(w.write_wait_ms, Some(20.0));
        let mut reset = t.clone();
        reset.readings = vec![reading(15_000_000, counters(1, 1, 1, 0))];
        assert_eq!(
            host_trace::window(&reset, i64::MIN, i64::MAX).written_kb,
            None
        );
        // A discard reset hidden behind a reading without discards.
        let mut hidden = t;
        let mut between = counters(6000, 15, 200, 0);
        between.sectors_discarded = None;
        hidden.readings = vec![reading(15_000_000, between)];
        hidden.end = Some(reading(20_000_000, counters(8192, 20, 300, 10)));
        assert_eq!(
            host_trace::window(&hidden, i64::MIN, i64::MAX).discarded_kb,
            None
        );
    }
}
