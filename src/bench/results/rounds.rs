//! `brokkr results --invocation ID`: one `--rounds` invocation's cells laid
//! out on the plan every one of them carries (`store::Schedule`), per
//! workload each competitor's median and each competitor's per-round sign
//! against the first, every planned cell that holds no result named.
//!
//! What is derived here is not stored: a slot with no run is blocked when
//! the build it needs is stored as failed, and otherwise absent at the read,
//! which says nothing of why (a stop, a crash, a publication still in doubt).

use std::collections::BTreeMap;

use super::{Listing, StoredRun, ms, result_wall, short_commit, timestamp};
use crate::bench::store;

/// The summary of the invocation `prefix` names, or why there is none.
pub(crate) fn format_rounds(prefix: &str, listing: &Listing) -> String {
    let mut by_invocation: BTreeMap<&str, Vec<&StoredRun>> = BTreeMap::new();
    for run in &listing.runs {
        if let Some(schedule) = &run.doc.schedule {
            by_invocation
                .entry(schedule.invocation.as_str())
                .or_default()
                .push(run);
        }
    }
    if by_invocation.len() > 1 {
        let mut lines = vec![format!(
            "{} --rounds invocations start with {prefix}; give more digits:",
            by_invocation.len()
        )];
        for (id, runs) in &by_invocation {
            let started = runs
                .iter()
                .map(|r| r.doc.started_unix_ms)
                .min()
                .unwrap_or(0);
            lines.push(format!(
                "  {}  {}  {}",
                crate::db::short_uuid(id),
                timestamp(started),
                runs.first().map_or("", |r| r.doc.bench.as_str())
            ));
        }
        return lines.join("\n");
    }
    match by_invocation.into_iter().next() {
        None => format!("no --rounds invocation of this project starts with {prefix}"),
        Some((id, runs)) => match Cells::of(id, &runs) {
            Ok(cells) => cells.render(id, &runs, &listing.marks).join("\n"),
            Err(why) => format!("invocation {id}: {why}"),
        },
    }
}

/// One invocation's stored runs by slot, and its failed builds.
struct Cells<'r> {
    plan: &'r store::Plan,
    /// By (workload, round, competitor).
    measured: BTreeMap<(u32, u32, u32), &'r StoredRun>,
    failed_builds: BTreeMap<u32, &'r StoredRun>,
    /// The stored stop: the interrupted run, and the run order it stopped
    /// at, `None` for a stop during the builds, before any cell ran.
    stopped: Option<(&'r StoredRun, Option<Order>)>,
}

/// A slot's place in the order cells run: workload, round, position.
type Order = (u32, u32, u32);

/// What one slot holds.
enum Held<'r> {
    Run(&'r StoredRun),
    Blocked {
        build: u32,
    },
    /// After the stop a stored interrupted run records.
    Stopped(&'r StoredRun),
    Absent,
}

/// Where `slot` falls in the order cells run.
fn run_order(slot: store::Slot) -> Order {
    (slot.workload, slot.round, slot.position)
}

impl<'r> Cells<'r> {
    /// The runs laid out on their plan: refused when they disagree on it or
    /// two claim one slot, since they are then not one invocation's.
    fn of(id: &str, runs: &[&'r StoredRun]) -> Result<Self, String> {
        let Some(plan) = runs
            .first()
            .and_then(|r| r.doc.schedule.as_ref())
            .map(|s| &s.plan)
        else {
            return Err(format!("no run of {id} holds a schedule"));
        };
        let mut cells = Self {
            plan,
            measured: BTreeMap::new(),
            failed_builds: BTreeMap::new(),
            stopped: None,
        };
        for run in runs {
            let Some(schedule) = &run.doc.schedule else {
                continue;
            };
            if schedule.plan != *plan {
                return Err(format!(
                    "its runs disagree on the plan (run {} against run {}), so no summary can \
                     trust either",
                    crate::db::short_uuid(&run.doc.uuid),
                    crate::db::short_uuid(&runs[0].doc.uuid)
                ));
            }
            let slot = schedule.slot;
            let twice = match schedule.phase {
                store::Phase::Build { build } => cells.failed_builds.insert(build, run).is_some(),
                store::Phase::Measure => cells
                    .measured
                    .insert((slot.workload, slot.round, slot.competitor), run)
                    .is_some(),
            };
            if twice {
                return Err(format!(
                    "one slot is stored twice (run {}), so the runs are not one invocation's",
                    crate::db::short_uuid(&run.doc.uuid)
                ));
            }
            if run.doc.outcome == store::RunOutcome::Interrupted {
                let at = match schedule.phase {
                    store::Phase::Build { .. } => None,
                    store::Phase::Measure => Some(run_order(slot)),
                };
                if cells.stopped.replace((run, at)).is_some() {
                    return Err(
                        "two runs record a stop, and an invocation stops once, so the runs are \
                         not one invocation's"
                            .to_owned(),
                    );
                }
            }
        }
        // Nothing runs after the stop.
        if let Some((stop, at)) = cells.stopped
            && let Some(run) = cells.measured.values().find(|run| {
                run.doc.schedule.as_ref().is_some_and(|s| match at {
                    None => true,
                    Some(at) => run_order(s.slot) > at,
                })
            })
        {
            return Err(format!(
                "run {} measured a slot after the stop run {} records, so the runs are not one \
                 invocation's",
                crate::db::short_uuid(&run.doc.uuid),
                crate::db::short_uuid(&stop.doc.uuid)
            ));
        }
        // A build that failed ran no cell of it.
        if let Some(run) = cells.measured.values().find(|run| {
            run.doc.schedule.as_ref().is_some_and(|s| {
                cells
                    .failed_builds
                    .contains_key(&plan.builds[s.slot.competitor as usize])
            })
        }) {
            return Err(format!(
                "run {} measured a slot whose build is stored as failed, so the runs are not one \
                 invocation's",
                crate::db::short_uuid(&run.doc.uuid)
            ));
        }
        Ok(cells)
    }

    fn held(&self, workload: u32, round: u32, competitor: usize) -> Held<'r> {
        let c = u32::try_from(competitor).unwrap_or(u32::MAX);
        if let Some(run) = self.measured.get(&(workload, round, c)) {
            return Held::Run(run);
        }
        let build = self.plan.builds[competitor];
        if self.failed_builds.contains_key(&build) {
            return Held::Blocked { build };
        }
        let count = u32::try_from(self.plan.competitors.len()).unwrap_or(u32::MAX);
        let order = (workload, round, store::position(c, count, round));
        match self.stopped {
            Some((run, None)) => Held::Stopped(run),
            Some((run, Some(at))) if order > at => Held::Stopped(run),
            _ => Held::Absent,
        }
    }

    fn render(&self, id: &str, runs: &[&StoredRun], marks: &store::MarkView) -> Vec<String> {
        let plan = self.plan;
        let started = runs
            .iter()
            .map(|r| r.doc.started_unix_ms)
            .min()
            .unwrap_or(0);
        let mut benches: Vec<&str> = runs.iter().map(|r| marks.name(&r.doc.bench)).collect();
        benches.sort_unstable();
        benches.dedup();
        let mut lines = vec![
            format!(
                "invocation {id}: {} started {} on {}",
                benches.join(", "),
                timestamp(started),
                runs[0].doc.hostname
            ),
            format!(
                "  {} rounds of {} competitors by {}, {}",
                plan.rounds,
                plan.competitors.len(),
                plan.axis.name(),
                crate::output::count(plan.workloads.len(), "workload")
            ),
        ];
        for (c, competitor) in plan.competitors.iter().enumerate() {
            let commit = runs
                .iter()
                .find(|r| {
                    r.doc
                        .schedule
                        .as_ref()
                        .is_some_and(|s| s.slot.competitor as usize == c)
                })
                .map_or_else(|| "?".to_owned(), |r| short_commit(r));
            lines.push(format!(
                "  competitor {}: {competitor}, build {} at {commit}",
                c + 1,
                plan.builds[c] + 1
            ));
        }
        for (build, run) in &self.failed_builds {
            let why = match &run.doc.outcome {
                store::RunOutcome::Failed { reason } => {
                    format!("failed: {reason}; every cell of it is blocked")
                }
                _ => "was interrupted: the invocation stopped before any cell ran".to_owned(),
            };
            lines.push(format!(
                "  build {} {why} (run {})",
                build + 1,
                crate::db::short_uuid(&run.doc.uuid)
            ));
        }
        lines.push(
            "  competitors are matched by the keys their params bound; inputs that differ \
             under a key are named"
                .to_owned(),
        );
        let hidden = runs
            .iter()
            .filter(|r| marks.hidden(&r.doc.uuid, &r.doc.bench))
            .count();
        if hidden > 0 {
            lines.push(format!(
                "  {} hidden by a mark, counted as stored",
                crate::output::count(hidden, "run")
            ));
        }
        if runs.iter().any(|r| r.doc.dirty) {
            lines.push("  some cells ran a dirty tree".to_owned());
        }
        for (w, workload) in plan.workloads.iter().enumerate() {
            let w = u32::try_from(w).unwrap_or(u32::MAX);
            if plan.workloads.len() > 1 {
                lines.push(format!("workload {}: {workload}", w + 1));
            }
            self.workload(w, &mut lines);
        }
        lines
    }

    /// One workload: each competitor's walls by round, then the medians and
    /// signs, the inputs that differ, and every slot without a result.
    fn workload(&self, w: u32, lines: &mut Vec<String>) {
        let plan = self.plan;
        let mut walls: Vec<Vec<Option<i64>>> = Vec::with_capacity(plan.competitors.len());
        let mut kinds = Vec::new();
        let mut gaps: Vec<String> = Vec::new();
        for (c, competitor) in plan.competitors.iter().enumerate() {
            let mut row = Vec::new();
            for round in 0..plan.rounds {
                let wall = match self.held(w, round, c) {
                    Held::Run(run) => {
                        let wall = result_wall(run);
                        match wall {
                            Some(_) => kinds.push(kind(run)),
                            None => gaps.push(format!(
                                "{competitor} round {}: {} (run {})",
                                round + 1,
                                without_result(run),
                                crate::db::short_uuid(&run.doc.uuid)
                            )),
                        }
                        wall
                    }
                    Held::Blocked { build } => {
                        gaps.push(format!(
                            "{competitor} round {}: blocked by build {}",
                            round + 1,
                            build + 1
                        ));
                        None
                    }
                    Held::Stopped(run) => {
                        gaps.push(format!(
                            "{competitor} round {}: not run, the invocation stopped (run {})",
                            round + 1,
                            crate::db::short_uuid(&run.doc.uuid)
                        ));
                        None
                    }
                    Held::Absent => {
                        gaps.push(format!("{competitor} round {}: absent", round + 1));
                        None
                    }
                };
                row.push(wall);
            }
            walls.push(row);
        }
        if kinds.iter().any(|k| Some(k) != kinds.first()) {
            lines.push(
                "  the results are not one kind of number (what was measured, or a measured \
                 call's wall against the block's, differs between cells), so none are compared"
                    .to_owned(),
            );
        } else {
            compared(plan, &walls, lines);
        }
        self.inputs(w, lines);
        lines.extend(gaps.into_iter().map(|gap| format!("  {gap}")));
    }

    /// Matched by key, not by content: a param two competitors both bound
    /// whose files or `from` path differ is named, the competing param aside.
    fn inputs(&self, w: u32, lines: &mut Vec<String>) {
        let plan = self.plan;
        let firsts: Vec<Option<&StoredRun>> = (0..plan.competitors.len())
            .map(|c| {
                (0..plan.rounds).find_map(|round| match self.held(w, round, c) {
                    Held::Run(run) => Some(run),
                    _ => None,
                })
            })
            .collect();
        // Against the first competitor that holds a run in this workload.
        let Some((b, base)) = firsts
            .iter()
            .enumerate()
            .find_map(|(c, run)| run.map(|run| (c, run)))
        else {
            return;
        };
        let identity = |bound: Option<&store::BoundParam>| {
            bound.map(|b| {
                (
                    b.from.clone(),
                    b.values
                        .iter()
                        .map(|v| v.digest.clone())
                        .collect::<Vec<_>>(),
                )
            })
        };
        for (c, other) in firsts.iter().enumerate().skip(b + 1) {
            let Some(other) = other else { continue };
            let names: std::collections::BTreeSet<&String> = base
                .doc
                .params
                .keys()
                .chain(other.doc.params.keys())
                .collect();
            let differing: Vec<String> = names
                .into_iter()
                .filter(|name| {
                    !matches!(&plan.axis, store::Axis::Param { name: axis } if axis == *name)
                })
                .filter_map(|name| {
                    let (ours, theirs) = (base.doc.params.get(name), other.doc.params.get(name));
                    match (ours, theirs) {
                        (Some(_), None) | (None, Some(_)) => {
                            Some(format!("{name} (bound on one side)"))
                        }
                        _ => (identity(ours) != identity(theirs)).then(|| name.clone()),
                    }
                })
                .collect();
            if !differing.is_empty() {
                lines.push(format!(
                    "  inputs differ, {} against {}: {}",
                    plan.competitors[c],
                    plan.competitors[b],
                    differing.join(", ")
                ));
            }
        }
    }
}

/// Why a stored cell has no result.
fn without_result(run: &StoredRun) -> String {
    match &run.doc.outcome {
        store::RunOutcome::Failed { reason } => format!("failed: {reason}"),
        store::RunOutcome::Interrupted => "interrupted".to_owned(),
        store::RunOutcome::Completed => "no result".to_owned(),
    }
}

/// The kind of number a run's result is, which two results must share to be
/// compared: the class it pairs in (an incidental capture is not a clean
/// timing), and a measured call's wall or the block's.
fn kind(run: &StoredRun) -> (store::PairingClass, bool) {
    (run.doc.pairing_class(), run.doc.child_measured())
}

/// Each competitor's median over its results, then each against the first,
/// round by round where both hold a result: the signs, ties counted, the
/// paired count, and the median paired difference.
fn compared(plan: &store::Plan, walls: &[Vec<Option<i64>>], lines: &mut Vec<String>) {
    for (c, competitor) in plan.competitors.iter().enumerate() {
        let mut got: Vec<i64> = walls[c].iter().flatten().copied().collect();
        let line = match median(&mut got) {
            Some(m) => format!("median {} of {}", ms(m), got.len()),
            None => "no result".to_owned(),
        };
        lines.push(format!("  {competitor}: {line}, {} planned", plan.rounds));
    }
    for c in 1..plan.competitors.len() {
        let (mut faster, mut slower, mut tied) = (0usize, 0usize, 0usize);
        let mut deltas = Vec::new();
        for (base, this) in walls[0].iter().zip(&walls[c]) {
            if let (Some(base), Some(this)) = (base, this) {
                match this.cmp(base) {
                    std::cmp::Ordering::Less => faster += 1,
                    std::cmp::Ordering::Greater => slower += 1,
                    std::cmp::Ordering::Equal => tied += 1,
                }
                deltas.push(this - base);
            }
        }
        let paired = deltas.len();
        let delta = median(&mut deltas).map_or_else(String::new, |d| {
            format!(
                ", median paired difference {}{}",
                if d > 0 { "+" } else { "" },
                ms(d)
            )
        });
        lines.push(format!(
            "  {} against {}: faster in {faster}, slower in {slower}, tied in {tied} of \
             {paired} paired rounds{delta}",
            plan.competitors[c], plan.competitors[0]
        ));
    }
}

/// The median, for an even count the mean of the middle two rounded toward
/// the lower, computed wide so no pair of stored walls overflows it.
fn median(values: &mut [i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        return Some(values[mid]);
    }
    let sum = i128::from(values[mid - 1]) + i128::from(values[mid]);
    i64::try_from(sum.div_euclid(2)).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn median_of_odd_and_even_counts() {
        assert_eq!(super::median(&mut []), None);
        assert_eq!(super::median(&mut [3, 1, 2]), Some(2));
        assert_eq!(super::median(&mut [4, 1, 3, 2]), Some(2));
        assert_eq!(super::median(&mut [-3, 0]), Some(-2));
        assert_eq!(super::median(&mut [i64::MAX, i64::MAX]), Some(i64::MAX));
    }
}
