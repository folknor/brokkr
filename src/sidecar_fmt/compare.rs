//! `brokkr sidecar --compare A B`: two runs' process-stream segments, paired
//! by ordinal within each name.
//!
//! The k-th segment named N on A pairs with the k-th segment named N on B,
//! counted in the timeline's chronological order over every segment
//! `build_phases` yields, zero-length and sampleless ones included. A name
//! that repeats (one segment per partition, input or pass) therefore compares
//! its repeats in order rather than every A repeat against B's first one.
//! This is a correspondence convention, not identity recovery: a missing
//! emission shifts the later ordinals of its name, and same-name operations
//! that reorder pair different work. Every segment of either run lands in
//! exactly one row; the one a side lacks prints `(no match)` / `null`.
//!
//! The table is A-ordered. A segment only B has is placed after the row
//! holding B's latest paired segment before it (before every row if there is
//! none), in B order within that slot. When pairs cross, slots and B order can
//! disagree; `index` in the JSONL gives each side's own position.

use std::collections::HashMap;

use super::{
    PhaseStats, Process, avg_cores_f64, avg_cores_str, build_phases, clk_tck_per_second,
    phase_stats,
};
use crate::sidecar;

/// One side of a comparison: a typed run's selected run, or one child of a
/// script-bench run's iteration.
pub(crate) struct CompareSide<'a> {
    /// The run's full uuid.
    pub(crate) uuid: &'a str,
    /// For a script-bench side, the iteration and the child compared.
    pub(crate) child: Option<(usize, usize)>,
    pub(crate) samples: &'a [sidecar::Sample],
    pub(crate) markers: &'a [sidecar::Marker],
    pub(crate) process: Process,
}

impl CompareSide<'_> {
    /// The side as the table heads it: the short uuid, and for a bench child
    /// its iteration and child, `uuid/ITER/CHILD` as an operand names it.
    fn label(&self) -> String {
        let short = crate::db::short_uuid(self.uuid);
        match self.child {
            Some((iteration, child)) => format!("{short}/{iteration}/{child}"),
            None => short,
        }
    }
}

/// One row of the comparison: the indices into each run's segment list.
#[derive(Debug, PartialEq, Eq)]
struct PairRow {
    /// 1-based position of this segment among the segments of its name.
    occurrence: usize,
    /// Whether the name occurs more than once on either side, so the ordinal
    /// carries information.
    repeated: bool,
    a: Option<usize>,
    b: Option<usize>,
}

/// Pair two segment-name sequences by ordinal within each name, ordered as
/// the module doc describes. Pure, so the pairing is tested apart from the
/// rendering.
fn pair_by_ordinal(a: &[&str], b: &[&str]) -> Vec<PairRow> {
    let mut b_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, name) in b.iter().enumerate() {
        b_by_name.entry(name).or_default().push(j);
    }
    let mut a_counts: HashMap<&str, usize> = HashMap::new();
    for name in a {
        *a_counts.entry(name).or_default() += 1;
    }
    let repeated = |name: &str| {
        let on_a = a_counts.get(name).copied().unwrap_or(0);
        let on_b = b_by_name.get(name).map_or(0, Vec::len);
        on_a.max(on_b) > 1
    };

    // Sort key: (slot, kind, B index). A row i sits in slot i + 1; a B-only
    // row sits in its anchor's slot after it (kind 1), or in slot 0 before
    // every row when no paired B segment precedes it.
    let mut keyed: Vec<((usize, u8, usize), PairRow)> = Vec::with_capacity(a.len() + b.len());
    let mut paired_row: Vec<Option<usize>> = vec![None; b.len()];
    let mut seen_a: HashMap<&str, usize> = HashMap::new();
    for (i, name) in a.iter().enumerate() {
        let k = seen_a.entry(name).or_default();
        *k += 1;
        let j = b_by_name.get(name).and_then(|js| js.get(*k - 1)).copied();
        if let Some(j) = j {
            paired_row[j] = Some(i);
        }
        keyed.push((
            (i + 1, 0, 0),
            PairRow {
                occurrence: *k,
                repeated: repeated(name),
                a: Some(i),
                b: j,
            },
        ));
    }
    let mut seen_b: HashMap<&str, usize> = HashMap::new();
    let mut slot = 0;
    for (j, name) in b.iter().enumerate() {
        let k = seen_b.entry(name).or_default();
        *k += 1;
        match paired_row[j] {
            Some(i) => slot = i + 1,
            None => keyed.push((
                (slot, 1, j),
                PairRow {
                    occurrence: *k,
                    repeated: repeated(name),
                    a: None,
                    b: Some(j),
                },
            )),
        }
    }
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, row)| row).collect()
}

/// Percent change of B's duration against A's, from microseconds. `None`
/// unless both are positive: a zero-length side (two markers on one stamp,
/// or a final segment closed at its own marker) has no duration to compare.
#[allow(clippy::cast_precision_loss)]
fn delta_pct(a_us: i64, b_us: i64) -> Option<f64> {
    (a_us > 0 && b_us > 0).then(|| (b_us - a_us) as f64 / a_us as f64 * 100.0)
}

/// One side of a row: the segment's position in its run and its readings.
struct Side<'a> {
    index: usize,
    name: &'a str,
    start_us: i64,
    duration_us: i64,
    stats: PhaseStats,
}

fn side<'a>(
    phases: &'a [(String, i64, i64)],
    samples: &[sidecar::Sample],
    index: Option<usize>,
) -> Option<Side<'a>> {
    let index = index?;
    let (name, start_us, end_us) = phases.get(index)?;
    Some(Side {
        index,
        name,
        start_us: *start_us,
        duration_us: end_us - start_us,
        stats: phase_stats(samples, *start_us, *end_us),
    })
}

/// Print the comparison of two sidecar timelines. Default is JSONL (a
/// `compare_summary` record, then one `compare_phase` record per row);
/// `human = true` gives the fixed-width table.
pub(crate) fn print_compare_timeline(a: &CompareSide<'_>, b: &CompareSide<'_>, human: bool) {
    let phases_a = build_phases(a.markers, a.samples, a.process);
    let phases_b = build_phases(b.markers, b.samples, b.process);
    let names_a: Vec<&str> = phases_a.iter().map(|(n, _, _)| n.as_str()).collect();
    let names_b: Vec<&str> = phases_b.iter().map(|(n, _, _)| n.as_str()).collect();
    let pairs = pair_by_ordinal(&names_a, &names_b);
    let rows: Vec<CompareRow> = pairs
        .iter()
        .map(|pair| CompareRow {
            pair,
            a: side(&phases_a, a.samples, pair.a),
            b: side(&phases_b, b.samples, pair.b),
        })
        .collect();
    let clk_tck = clk_tck_per_second();

    if human {
        print_human(a, b, &rows, clk_tck);
    } else {
        print_jsonl(a, b, phases_a.len(), phases_b.len(), &rows, clk_tck);
    }
}

struct CompareRow<'a> {
    pair: &'a PairRow,
    a: Option<Side<'a>>,
    b: Option<Side<'a>>,
}

impl CompareRow<'_> {
    fn name(&self) -> &str {
        self.a.as_ref().or(self.b.as_ref()).map_or("", |s| s.name)
    }

    fn delta_pct(&self) -> Option<f64> {
        delta_pct(self.a.as_ref()?.duration_us, self.b.as_ref()?.duration_us)
    }
}

/// The 31-column cell for one side: duration, peak anon, disk read, cores.
/// A script-bench child's phase no sample landed in has its readings shown
/// as `-`: absent, not zero.
fn human_cell(side: Option<&Side>, process: Process, clk_tck: i64) -> String {
    side.map_or_else(
        || "(no match)".to_owned(),
        |s| {
            if process.is_child() && s.stats.samples == 0 {
                return format!(
                    "{:>5}ms {:>8} {:>7} {:>6}",
                    s.duration_us / 1_000,
                    "-",
                    "-",
                    "-"
                );
            }
            format!(
                "{:>5}ms {:>6}kB {:>5}MB {:>5}c",
                s.duration_us / 1_000,
                s.stats.peak_anon,
                s.stats.disk_read_kb / 1024,
                avg_cores_str(s.stats.cpu_delta_jiffies, s.stats.sample_span_us, clk_tck),
            )
        },
    )
}

#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
fn print_human(a: &CompareSide<'_>, b: &CompareSide<'_>, rows: &[CompareRow], clk_tck: i64) {
    let name_width = rows
        .iter()
        .map(|r| r.name().chars().count())
        .max()
        .unwrap_or(0)
        .max(20);
    // The ordinal column appears only when some name repeats; a blank cell
    // means the name occurs once on each side that has it.
    let ordinal_width = rows
        .iter()
        .filter(|r| r.pair.repeated)
        .map(|r| r.pair.occurrence.to_string().len())
        .max();
    let ordinal = |cell: &str| ordinal_width.map_or_else(String::new, |w| format!(" {cell:>w$}"));

    println!(
        "{:<name_width$}{} {:>31} {:>31} {:>7}",
        "Phase",
        ordinal("#"),
        format!("Run A ({})", a.label()),
        format!("Run B ({})", b.label()),
        "Delta",
    );
    for row in rows {
        let occurrence = if row.pair.repeated {
            row.pair.occurrence.to_string()
        } else {
            String::new()
        };
        let delta = row
            .delta_pct()
            .map_or_else(|| "-".to_owned(), |pct| format!("{pct:+.0}%"));
        println!(
            "{:<name_width$}{} {:>31} {:>31} {:>7}",
            row.name(),
            ordinal(&occurrence),
            human_cell(row.a.as_ref(), a.process, clk_tck),
            human_cell(row.b.as_ref(), b.process, clk_tck),
            delta,
        );
    }
}

/// One side as JSON. A script-bench child's side also carries `end_us` and
/// `duration_us`, and its readings are `null` for a phase no sample landed
/// in; a typed side keeps the fields it always had.
fn side_json(side: Option<&Side>, process: Process, clk_tck: i64) -> serde_json::Value {
    side.map_or(serde_json::Value::Null, |s| {
        let mut obj = serde_json::json!({
            "index": s.index,
            "start_us": s.start_us,
            "duration_ms": s.duration_us / 1_000,
            "peak_anon_kb": s.stats.peak_anon,
            "disk_read_kb": s.stats.disk_read_kb,
            "cpu_delta_jiffies": s.stats.cpu_delta_jiffies,
            "sample_span_us": s.stats.sample_span_us,
            "avg_cores": avg_cores_f64(s.stats.cpu_delta_jiffies, s.stats.sample_span_us, clk_tck),
        });
        if process.is_child()
            && let Some(map) = obj.as_object_mut()
        {
            map.insert("end_us".to_owned(), (s.start_us + s.duration_us).into());
            map.insert("duration_us".to_owned(), s.duration_us.into());
            map.insert("samples".to_owned(), s.stats.samples.into());
            if s.stats.samples == 0 {
                for field in [
                    "peak_anon_kb",
                    "disk_read_kb",
                    "cpu_delta_jiffies",
                    "sample_span_us",
                ] {
                    map.insert(field.to_owned(), serde_json::Value::Null);
                }
            }
        }
        obj
    })
}

#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
fn print_jsonl(
    a: &CompareSide<'_>,
    b: &CompareSide<'_>,
    a_phases: usize,
    b_phases: usize,
    rows: &[CompareRow],
    clk_tck: i64,
) {
    let mut header = serde_json::json!({
        "type": "compare_summary",
        "a_uuid": a.uuid,
        "b_uuid": b.uuid,
        "a_phases": a_phases,
        "b_phases": b_phases,
    });
    if let Some(map) = header.as_object_mut() {
        for (prefix, side) in [("a", a), ("b", b)] {
            if let Some((iteration, child)) = side.child {
                map.insert(format!("{prefix}_iteration"), iteration.into());
                map.insert(format!("{prefix}_child"), child.into());
            }
        }
    }
    println!("{header}");

    for row in rows {
        // Round to 2 decimals - same rationale as avg_cores.
        let delta = row.delta_pct().map(|pct| (pct * 100.0).round() / 100.0);
        let obj = serde_json::json!({
            "type": "compare_phase",
            "name": row.name(),
            "occurrence": row.pair.occurrence,
            "a": side_json(row.a.as_ref(), a.process, clk_tck),
            "b": side_json(row.b.as_ref(), b.process, clk_tck),
            "delta_pct": delta,
        });
        println!("{obj}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(occurrence: usize, repeated: bool, a: Option<usize>, b: Option<usize>) -> PairRow {
        PairRow {
            occurrence,
            repeated,
            a,
            b,
        }
    }

    // The ticket's shape: six same-named partitions a side pair in order, not
    // every A partition against B's first.
    #[test]
    fn repeats_pair_by_ordinal() {
        let a = ["SCAN", "P", "P", "P", "DONE"];
        let b = ["SCAN", "P", "P", "P", "DONE"];
        let rows = pair_by_ordinal(&a, &b);
        assert_eq!(
            rows,
            [
                row(1, false, Some(0), Some(0)),
                row(1, true, Some(1), Some(1)),
                row(2, true, Some(2), Some(2)),
                row(3, true, Some(3), Some(3)),
                row(1, false, Some(4), Some(4)),
            ]
        );
    }

    // Surplus repeats on either side are one-sided rows; a surplus on B sits
    // after B's last paired repeat, not after the run's tail.
    #[test]
    fn surplus_repeats_are_one_sided_rows() {
        let a = ["P", "P", "P", "DONE"];
        let b = ["P", "P", "DONE"];
        assert_eq!(
            pair_by_ordinal(&a, &b),
            [
                row(1, true, Some(0), Some(0)),
                row(2, true, Some(1), Some(1)),
                row(3, true, Some(2), None),
                row(1, false, Some(3), Some(2)),
            ]
        );
        let rows = pair_by_ordinal(&b, &a);
        assert_eq!(
            rows,
            [
                row(1, true, Some(0), Some(0)),
                row(2, true, Some(1), Some(1)),
                row(3, true, None, Some(2)),
                row(1, false, Some(2), Some(3)),
            ]
        );
    }

    // Every segment of either side appears in exactly one row, B-only ones
    // included, and a B-only segment with no paired predecessor leads.
    #[test]
    fn every_segment_lands_once() {
        let a = ["X", "Y"];
        let b = ["W", "Y", "U", "X", "V"];
        let rows = pair_by_ordinal(&a, &b);
        assert_eq!(
            rows,
            [
                row(1, false, None, Some(0)),
                row(1, false, Some(0), Some(3)),
                row(1, false, None, Some(4)),
                row(1, false, Some(1), Some(1)),
                row(1, false, None, Some(2)),
            ]
        );
        let mut seen_b: Vec<usize> = rows.iter().filter_map(|r| r.b).collect();
        seen_b.sort_unstable();
        assert_eq!(seen_b, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn delta_needs_both_durations() {
        assert_eq!(delta_pct(1_000, 1_500), Some(50.0));
        assert_eq!(delta_pct(0, 1_500), None);
        assert_eq!(delta_pct(1_000, 0), None);
        // Sub-millisecond durations still compare.
        assert_eq!(delta_pct(400, 800), Some(100.0));
    }

    // A marker stamped after the last sample closes the final segment at its
    // own start instead of a reversed interval.
    #[test]
    fn a_final_marker_after_the_last_sample_is_zero_length() {
        let marker = |idx: i32, ts: i64, name: &str| sidecar::Marker {
            marker_idx: idx,
            timestamp_us: ts,
            name: name.to_owned(),
            run_idx: 0,
            call: None,
        };
        let sample = |ts: i64| sidecar::Sample {
            timestamp_us: ts,
            ..super::super::timeline_arg_tests::zero_sample()
        };
        let markers = [marker(0, 0, "A"), marker(1, 500_000, "B")];
        let samples = [sample(100_000), sample(200_000)];
        let phases = build_phases(&markers, &samples, Process::Sampled);
        assert_eq!(
            phases,
            [
                ("A".to_owned(), 0, 500_000),
                ("B".to_owned(), 500_000, 500_000)
            ]
        );

        // A bench child's known wall closes its last phase, samples or not;
        // a marker past the wall moves only the boundary; a markerless child
        // is its whole interval.
        let child = Process::Child { wall_us: 900_000 };
        assert_eq!(
            build_phases(&markers, &[], child),
            [
                ("A".to_owned(), 0, 500_000),
                ("B".to_owned(), 500_000, 900_000)
            ]
        );
        let short = Process::Child { wall_us: 400_000 };
        assert_eq!(
            build_phases(&markers, &samples, short),
            [
                ("A".to_owned(), 0, 500_000),
                ("B".to_owned(), 500_000, 500_000)
            ]
        );
        assert_eq!(
            build_phases(&[], &[], child),
            [("(all)".to_owned(), 0, 900_000)]
        );
    }
}
