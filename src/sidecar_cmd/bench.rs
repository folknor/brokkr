//! A script-bench run as `brokkr sidecar` reads it (`docs/commands/measure.md`,
//! Script-bench children): one child of each selected iteration, its stored
//! evidence converted to the forms the views render, with the iteration as
//! the stream's run. The whole selection is judged before anything renders.

use crate::bench::evidence::{ChildEvidence, capture_loss, child_evidence};
use crate::bench::store::{IterationOutcome, RunOutcome, StoredRun};
use crate::error::DevError;
use crate::output;
use crate::sidecar_fmt::Process;

/// One selected child of each selected iteration, its evidence converted.
pub(super) struct BenchRun {
    run: StoredRun,
    /// `--run all`.
    all: bool,
    /// The child read in each selected iteration; `None` only when the one
    /// selected iteration launched none and no `--child` was given.
    child: Option<usize>,
    /// Each selected iteration with its child's evidence, ascending.
    selected: Vec<(usize, ChildEvidence)>,
    /// The selected iterations, ascending, whatever their children.
    iterations: Vec<usize>,
}

fn short(uuid: &str) -> String {
    crate::db::short_uuid(uuid)
}

fn children(n: usize) -> String {
    match n {
        0 => "no child".to_owned(),
        1 => "1 child".to_owned(),
        n => format!("{n} children"),
    }
}

fn outcome(outcome: &IterationOutcome) -> String {
    match outcome {
        IterationOutcome::Completed { verdict } => match verdict {
            Some(v) => format!("completed, verdict {v:?}"),
            None => "completed, verdict nil".to_owned(),
        },
        IterationOutcome::Failed { reason } => format!("failed: {reason}"),
        IterationOutcome::Interrupted => "interrupted".to_owned(),
    }
}

/// `--run`'s iterations: `all`, a number, or by default the result's
/// iteration, else the last one stored (where a run without a result
/// ended).
fn iterations(run: &StoredRun, run_arg: Option<&str>) -> Result<(Vec<usize>, bool), DevError> {
    let count = run.doc.iterations.len();
    match run_arg {
        Some("all") => Ok(((0..count).collect(), true)),
        Some(n) => {
            let idx = n.parse::<usize>().map_err(|_| {
                DevError::Usage(format!("--run: expected a number or 'all', got '{n}'"))
            })?;
            if idx >= count {
                return Err(DevError::Usage(format!(
                    "--run {idx}: bench run {} has {}",
                    short(&run.doc.uuid),
                    output::count(count, "iteration")
                )));
            }
            Ok((vec![idx], false))
        }
        None => Ok((
            run.best_iteration
                .or_else(|| count.checked_sub(1))
                .into_iter()
                .collect(),
            false,
        )),
    }
}

/// Judge the selection whole: every selected iteration must have the child
/// asked for (or, without `--child`, exactly one), every offending iteration
/// named. A single selected iteration that launched no child, with no
/// `--child`, is an answer rather than an error.
pub(super) fn select(
    run: StoredRun,
    run_arg: Option<&str>,
    child_arg: Option<usize>,
) -> Result<BenchRun, DevError> {
    let (iterations, all) = iterations(&run, run_arg)?;
    if let Some(c) = child_arg
        && iterations.is_empty()
    {
        return Err(DevError::Config(format!(
            "bench run {}: --child {c} names no child: the run attempted no iteration",
            short(&run.doc.uuid)
        )));
    }
    let counts: Vec<(usize, usize)> = iterations
        .iter()
        .map(|&i| (i, run.doc.iterations[i].children.len()))
        .collect();
    let lone_empty = child_arg.is_none() && counts.len() == 1 && counts[0].1 == 0;
    let offending: Vec<String> = counts
        .iter()
        .filter_map(|&(i, n)| match child_arg {
            Some(c) if c >= n => Some(format!("iteration {i} launched {}", children(n))),
            None if n == 0 && !lone_empty => Some(format!("iteration {i} launched no child")),
            None if n > 1 => Some(format!(
                "iteration {i} launched {}; pass --child N",
                children(n)
            )),
            _ => None,
        })
        .collect();
    if !offending.is_empty() {
        let what = child_arg.map_or_else(
            || "each view reads one child".to_owned(),
            |c| format!("--child {c} names no child of every selected iteration"),
        );
        return Err(DevError::Config(format!(
            "bench run {}: {what}: {}",
            short(&run.doc.uuid),
            offending.join("; ")
        )));
    }
    let child = if lone_empty || counts.is_empty() {
        None
    } else {
        Some(child_arg.unwrap_or(0))
    };
    let selected = match child {
        Some(c) => iterations
            .iter()
            .map(|&i| {
                (
                    i,
                    child_evidence(&run.doc.iterations[i].children[c].evidence, i),
                )
            })
            .collect(),
        None => Vec::new(),
    };
    Ok(BenchRun {
        run,
        all,
        child,
        selected,
        iterations,
    })
}

impl BenchRun {
    pub(super) fn uuid(&self) -> &str {
        &self.run.doc.uuid
    }
    /// Whether there is a child to show: `false` for a run that attempted no
    /// iteration and for one selected iteration that launched none.
    pub(super) fn has_child(&self) -> bool {
        self.child.is_some()
    }

    /// The selected iteration and child, for a single-iteration selection.
    pub(super) fn one(&self) -> Option<(usize, usize)> {
        match (self.all, self.child, self.iterations.as_slice()) {
            (false, Some(child), [iteration]) => Some((*iteration, child)),
            _ => None,
        }
    }

    /// `--run`'s answer as the typed views take it: one iteration, or every
    /// one.
    pub(super) fn filter(&self) -> Option<usize> {
        if self.all {
            None
        } else {
            self.iterations.first().copied()
        }
    }

    fn child_of(&self, iteration: usize) -> Option<&crate::bench::store::Child> {
        self.run.doc.iterations[iteration].children.get(self.child?)
    }

    /// The provenance header, on stderr: the run, then the selection and,
    /// for one child, how it ran.
    pub(super) fn print_header(&self) {
        let doc = &self.run.doc;
        let commit: String = doc.commit.chars().take(8).collect();
        let dirty = if doc.dirty { "-dirty" } else { "" };
        let started = crate::bench::results::timestamp(doc.started_unix_ms);
        let result = match &doc.outcome {
            RunOutcome::Completed => "completed".to_owned(),
            RunOutcome::Failed { reason } => format!("failed: {reason}"),
            RunOutcome::Interrupted => "interrupted".to_owned(),
        };
        output::sidecar_info(&format!(
            "bench {} (project {})  started {started}  commit: {commit}{dirty}  run {result}",
            doc.bench, doc.project
        ));
        let total = doc.iterations.len();
        if total == 0 {
            return;
        }
        // `--run all` over several iterations names the child read in each;
        // one iteration, however selected, is shown as itself.
        if self.all
            && let Some(c) = self.child
            && self.iterations.len() > 1
        {
            output::sidecar_info(&format!("all {total} iterations, child {c} of each"));
            return;
        }
        let Some(&i) = self.iterations.first() else {
            return;
        };
        let iteration = &doc.iterations[i];
        let marked = if self.run.best_iteration == Some(i) {
            " (result)"
        } else {
            ""
        };
        output::sidecar_info(&format!(
            "iteration {i} of {total}{marked}: {}",
            outcome(&iteration.outcome)
        ));
        let Some(child) = self.child_of(i) else {
            return;
        };
        let c = self.child.unwrap_or(0);
        let pid = child.pid.map_or_else(String::new, |p| format!("  PID {p}"));
        #[allow(clippy::cast_precision_loss)]
        let ms = |us: i64| format!("{:.3} ms", us as f64 / 1000.0);
        output::sidecar_info(&format!(
            "child {c} of {}: {} {}{pid}  at +{}  wall {}",
            iteration.children.len(),
            child.program,
            child.argv.join(" "),
            ms(child.origin_offset_us),
            ms(child.wall_us),
        ));
        if !matches!(
            child.ending,
            crate::bench::store::ChildEnding::Exited { code: 0 }
        ) {
            output::sidecar_error(&crate::bench::results::child_ending(&child.ending));
        }
    }

    /// What a view says when there is no child: the run attempted nothing,
    /// or its one selected iteration launched no child.
    pub(super) fn no_child_message(&self) -> String {
        match self.iterations.first() {
            None => format!(
                "bench run {} attempted no iteration",
                short(&self.run.doc.uuid)
            ),
            Some(i) => format!("iteration {i} launched no child"),
        }
    }

    /// The capture warnings, then which iteration is shown when the run has
    /// several.
    pub(super) fn warn_and_show(&self) {
        self.warn_capture();
        let total = self.run.doc.iterations.len();
        if total > 1 && self.child.is_some() {
            let showing = match self.filter() {
                Some(i) => format!("iteration {i}/{total}"),
                None => format!("all {total} iterations"),
            };
            output::sidecar_msg(&format!("showing {showing} (use --run to override)"));
        }
    }

    /// Each selected child's capture warnings, one line per cause set and
    /// per clock finding, labelled with the selection that reads it.
    pub(super) fn warn_capture(&self) {
        let Some(c) = self.child else {
            return;
        };
        for &(i, _) in &self.selected {
            let Some(child) = self.child_of(i) else {
                continue;
            };
            let e = &child.evidence;
            let label = format!("sidecar {} --run {i} --child {c}", short(self.uuid()));
            let mut causes = capture_loss(&e.loss).causes();
            if e.calls_overflowed {
                causes.push("more call ids arrived than the census holds".to_owned());
            }
            if !causes.is_empty() {
                output::warn(&format!(
                    "{label}: stored capture is incomplete: {}",
                    causes.join(", ")
                ));
            }
            let clock = crate::sidecar::ClockOutliers {
                count: usize::try_from(e.clock.outliers).unwrap_or(usize::MAX),
                min_us: e.clock.min_us,
            };
            if let Some(message) = clock.message(&label) {
                output::warn(&message);
            }
        }
    }

    fn evidence(&self, run: Option<usize>) -> impl Iterator<Item = &(usize, ChildEvidence)> {
        self.selected
            .iter()
            .filter(move |(i, _)| run.is_none_or(|r| r == *i))
    }

    /// The samples, each with its iteration.
    pub(super) fn samples(&self, run: Option<usize>) -> (Vec<crate::sidecar::Sample>, Vec<usize>) {
        let mut samples = Vec::new();
        let mut runs = Vec::new();
        for (i, e) in self.evidence(run) {
            samples.extend(e.samples.iter().cloned());
            runs.extend(std::iter::repeat_n(*i, e.samples.len()));
        }
        (samples, runs)
    }

    pub(super) fn markers(&self, run: Option<usize>) -> Vec<crate::sidecar::Marker> {
        self.evidence(run)
            .flat_map(|(_, e)| e.markers.iter().cloned())
            .collect()
    }

    pub(super) fn counters(&self, run: Option<usize>) -> Vec<crate::sidecar::Counter> {
        self.evidence(run)
            .flat_map(|(_, e)| e.counters.iter().cloned())
            .collect()
    }

    pub(super) fn sink_ends(&self, run: Option<usize>) -> Vec<crate::sidecar::StoredSinkEnd> {
        self.evidence(run)
            .flat_map(|(_, e)| e.sink_ends.iter().cloned())
            .collect()
    }

    /// The process of iteration `run`'s child: its interval is known.
    pub(super) fn process(&self, run: usize) -> Process {
        self.child_of(run)
            .map_or(Process::Sampled, |child| Process::Child {
                wall_us: child.wall_us,
            })
    }

    /// Refuse a `--call` no selected child knows, judged on each child's
    /// retained evidence before any view filter, and say which iterations
    /// know it only through the census of call ids (their records of it
    /// were not kept).
    pub(super) fn check_call(&self, run: Option<usize>, call: &str) -> Result<(), DevError> {
        let mut known = false;
        let mut census_only = Vec::new();
        let mut overflowed = false;
        for (i, e) in self.evidence(run) {
            let retained = e.markers.iter().any(|m| m.call.as_deref() == Some(call))
                || e.counters.iter().any(|c| c.call.as_deref() == Some(call))
                || e.sink_ends.iter().any(|s| s.call == call);
            let child = self.child_of(*i);
            let seen = child.is_some_and(|c| c.evidence.calls_seen.contains(call));
            overflowed |= child.is_some_and(|c| c.evidence.calls_overflowed);
            known |= retained || seen;
            if seen && !retained {
                census_only.push(*i);
            }
        }
        if !known {
            let caveat = if overflowed {
                "; the census of call ids overflowed, so it may have arrived unrecorded"
            } else {
                ""
            };
            return Err(DevError::Config(format!(
                "--call: no call '{call}' in the selected runs{caveat}"
            )));
        }
        for i in census_only {
            output::sidecar_msg(&format!(
                "iteration {i}: call '{call}' was named by records the capture did not keep"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "bench_tests.rs"]
mod tests;
