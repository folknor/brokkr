//! Sweep preparation, execution order, and run accounting.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::build;
use crate::check_cmd::{self, LaneEntry};
use crate::config::DevConfig;
use crate::error::DevError;
use crate::output;
use crate::profile::PlannedSweep;
use crate::project::Project;
use crate::test_runner;

use super::display::{RepeatState, lone_test_tag, show_failed_invocation, show_reproduction};
use super::plan::{
    BuildShape, FocusedPlan, NoMatchNote, ReadySweep, Searched, SweepContext, SweepOutcome,
    direct_libtest_args, exclusion_describe, lint_allow_flags, prebuild_targets, prepare_sweep,
    resolve_debug, resolve_package, resolve_sweeps, run_pre_build, single_run, sweep_plan_line,
    test_argv,
};
use super::verdict::{
    OneRun, Outcome, RunReport, aggregate_exit, ceilings_for, format_repeat_summary, run_one,
    stop_for_budget,
};

/// Run `<NAME>` in every applicable sweep.
///
/// Two things bound the command beyond the per-test cap, both shared with
/// `check`: the phase watchdog (`check`'s 15-minute `test` ceiling, restarted
/// for every sweep and every `-N` iteration - there is no whole-run ceiling,
/// since a repeat run is bounded per iteration) and a `SigtermGuard`, so a
/// graceful `brokkr kill` ends the command as `Interrupted` (exit 130). A fired
/// watchdog exits 124 whatever the kill surfaced as.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    dev_config: &DevConfig,
    project: Project,
    project_root: &Path,
    state_root: &Path,
    name: &str,
    package: Option<&str>,
    repeat: u32,
    jobs: Option<u32>,
    profile_override: Option<bool>,
    timeout: Option<u64>,
    sweep_filter: Option<&str>,
) -> Result<(), DevError> {
    // Declared before the guard, so the guard drops first and the watchdog's
    // join is the last thing the command does. The run log is the caller's
    // (`main`, opened before the lock), so it outlives the join.
    let _ceiling = check_cmd::CheckWatchdog::arm_for("brokkr test", None);
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let result = run_sweeps(
        dev_config,
        project,
        project_root,
        state_root,
        name,
        package,
        repeat,
        jobs,
        profile_override,
        timeout,
        sweep_filter,
    );
    // Whatever `result` is: a ceiling that fired as the last child exited can
    // leave an `Ok` behind it, and a killed run is not a pass.
    if check_cmd::watchdog_fired().is_some() {
        return Err(DevError::ExitCode(check_cmd::WATCHDOG_EXIT_CODE));
    }
    if crate::shutdown::is_shutdown_requested() {
        return Err(DevError::Interrupted);
    }
    result
}

/// The per-test wall ceiling for a run: the default, or `--timeout` when it
/// lies within the hard limit.
fn test_ceiling(timeout: Option<u64>) -> Result<Duration, DevError> {
    if let Some(timeout) = timeout
        && (timeout == 0 || timeout > test_runner::ceilings::MAX_TEST_TIMEOUT.as_secs())
    {
        return Err(DevError::Config(format!(
            "--timeout must be between 1 and {} seconds",
            test_runner::ceilings::MAX_TEST_TIMEOUT.as_secs()
        )));
    }
    Ok(timeout.map_or(test_runner::TEST_TIMEOUT, Duration::from_secs))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_sweeps(
    dev_config: &DevConfig,
    project: Project,
    project_root: &Path,
    state_root: &Path,
    name: &str,
    package: Option<&str>,
    repeat: u32,
    jobs: Option<u32>,
    profile_override: Option<bool>,
    timeout: Option<u64>,
    sweep_filter: Option<&str>,
) -> Result<(), DevError> {
    let repeat = repeat.max(1);
    let ceiling = test_ceiling(timeout)?;
    let sweeps = resolve_sweeps(dev_config.test.as_ref(), &dev_config.check, sweep_filter)?;

    let (pkg, source) = resolve_package(package, dev_config, project)?;
    // Which sweeps the run is eligible to attempt: the resolved package always
    // overrides each sweep's own selection, sweeps whose rules leave it out
    // are excluded, and sweeps that would execute exactly what an earlier one
    // does are deduped. After `--sweep`, so a lane-qualified label reaches a
    // sweep the dedupe would otherwise fold away.
    let test_cfg = dev_config.test.as_ref();
    let debug_of = |s: &PlannedSweep| resolve_debug(profile_override, test_cfg, s.profile);
    // Each sweep's features are projected onto the one package: a token only
    // another of the sweep's packages routes is dropped, and said so.
    let oracle = check_cmd::FeatureOracle::at(project_root);
    let selection =
        check_cmd::PhaseSelection::for_brokkr_test(&sweeps, &pkg, source, &debug_of, &oracle)?;
    check_cmd::announce_feature_drops(&sweeps, &[&selection]);
    // A deduped sweep is not a sweep this run goes through.
    let multi = selection
        .entries()
        .iter()
        .filter(|e| !matches!(e, LaneEntry::Deduped(_)))
        .count()
        > 1;

    // `brokkr test` defaults to `cargo test --release` (debug=false ->
    // <target>/release); the dev profile flips both the cargo invocation
    // and BROKKR_TEST_BIN_DIR over to <target>/debug. Tests that spawn the
    // just-rebuilt binary read this var to skip the
    // `cfg!(debug_assertions)` profile guess (which silently lies when
    // a workspace pins `[profile.test]` overrides).
    // Per sweep, because a `[[check]]` entry may pin its own profile: the
    // project-wide default is what a sweep that doesn't care inherits, not
    // a decision imposed on one that does.
    let allow_flags = lint_allow_flags(dev_config);
    let target_dir = build::project_info(Some(project_root))?.target_dir;
    let cx = SweepContext {
        project,
        project_root,
        state_root,
        test_cfg: dev_config.test.as_ref(),
        target_dir: &target_dir,
        allow_flags: &allow_flags,
        name,
        jobs,
        multi,
        profile_override,
        timeout,
    };

    let mut reports: Vec<RunReport> = Vec::new();

    if multi {
        println!("{}", sweep_plan_line(&sweeps, &selection, &pkg));
    }
    // No per-sweep header lines: every line that belongs to one sweep says
    // which (the build line, the PASS/FAIL tags), when several run.

    // Every sweep is built and discovered before the first test runs: the
    // inventory of what this invocation will execute is what a stop in one
    // sweep is reported against, so the tests of the sweeps after it can be
    // named. (A sweep's discovery used to wait for the sweeps before it to
    // finish running.)
    let mut ready: Vec<ReadySweep<'_>> = Vec::new();
    for (sweep, entry) in sweeps.iter().zip(selection.entries().iter()) {
        let attempt = match entry {
            LaneEntry::Attempt(a) => a,
            // A sweep whose rules leave the package out doesn't carry this
            // sweep's features - forcing the build would fail on a foreign
            // feature (e.g. `-p nautilus-hyperliquid` under an ffi sweep it
            // isn't a member of). SKIP it like the zero-tests-matched case;
            // other sweeps still get their chance to run the test. Under
            // several sweeps the plan line already named it and why; a lone
            // sweep has no plan line, so it says so here.
            LaneEntry::Excluded(e) => {
                if !multi {
                    println!(
                        "[test]    SKIP {pkg}::{name} - {}",
                        exclusion_describe(e.notes())
                    );
                }
                reports.push(RunReport::bare(Outcome::NoMatch));
                continue;
            }
            // Runs what an earlier sweep runs: named on the plan line, not run.
            LaneEntry::Deduped(_) | LaneEntry::Disabled(_) | LaneEntry::NotApplicable(_) => {
                continue;
            }
        };
        match prepare_sweep(sweep, attempt, &cx)? {
            SweepOutcome::Ready(r) => ready.push(*r),
            SweepOutcome::Done(report) => reports.push(report),
        }
    }

    let inventory = TestInventory::open(&ready, &cx, repeat);
    let mut notes: Vec<NoMatchNote> = Vec::new();
    let ran = run_ready(
        &ready,
        &cx,
        repeat,
        ceiling,
        inventory.as_ref(),
        &mut reports,
        &mut notes,
    );

    // The journal says the run is over, however it ended; then what it left
    // unresolved is read back from disk.
    let continuation = inventory
        .map(TestInventory::close)
        .and_then(|i| i.continuation());
    if let Some(c) = &continuation {
        output::error(&check_cmd::render_continuation(c).join("\n"));
    }
    ran?;

    // A sweep that found no match is news only beside a sweep that did: then
    // "feature-gated out of this sweep" is real information. When none matched,
    // the closing error says it once.
    if reports
        .iter()
        .any(|r| matches!(r.outcome, Outcome::Pass | Outcome::Fail))
    {
        for note in &notes {
            println!(
                "[test]    SKIP {pkg}::{name} [{}] - {}",
                note.label, note.detail
            );
        }
    }

    if repeat > 1 {
        for line in format_repeat_summary(&reports) {
            println!("{line}");
        }
    }

    let outcomes: Vec<Outcome> = reports.iter().map(|r| r.outcome).collect();
    let searched = Searched::of(&ready).describe();
    aggregate_exit(&outcomes, &pkg, name, &searched)
}

/// Run every ready sweep's iterations, recording each in its lane's journal.
/// A blown time budget, or any error, ends the run; the stop is journaled
/// run-wide, so everything after it reads as unobserved for that reason.
pub(super) fn run_ready(
    ready: &[ReadySweep<'_>],
    cx: &SweepContext<'_>,
    repeat: u32,
    ceiling: Duration,
    inventory: Option<&TestInventory>,
    reports: &mut Vec<RunReport>,
    notes: &mut Vec<NoMatchNote>,
) -> Result<(), DevError> {
    let repeat_state = RepeatState::default();
    for (si, prepared) in ready.iter().enumerate() {
        // Sweeps can share build outputs (two feature sweeps with one
        // `build_packages` binary), so the build the up-front preparation did
        // for this sweep may have been overwritten by a later sweep's since.
        // And with one sweep, discovery itself ran each harness's code (`--list`)
        // after the build it lists, code that can touch the build's inputs. So
        // the sweep is built again immediately before it runs - a no-op that
        // re-uplifts its support binaries - and held to its prepared record,
        // always. It says nothing unless it fails or finds drift: a routine
        // re-verification narrated as a second build reads like one.
        check_cmd::enter_phase(check_cmd::Phase::Test);
        let fresh;
        let r: &ReadySweep<'_> = match settle_sweep(prepared, cx, inventory, si, repeat) {
            Ok(Settled::Unchanged) => prepared,
            Ok(Settled::Rebuilt(f)) => {
                fresh = *f;
                &fresh
            }
            Ok(Settled::Failed(report)) => {
                reports.push(report);
                continue;
            }
            Err(e) => {
                check_cmd::record_run_stop(&e, None);
                return Err(e);
            }
        };
        let env_refs: Vec<(&str, &str)> = r
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let plan = SweepPlan {
            shape: BuildShape {
                sweep: r.sweep,
                allow_args: &r.allow_args,
                run: single_run(r.sweep, &r.attempt)?,
                jobs: cx.jobs,
                profile: r.profile,
            },
            name: cx.name,
            focused: r.focused.as_ref(),
            exact: r.exact.clone(),
            env: &env_refs,
            project_root: cx.project_root,
            state_root: cx.state_root,
            ceiling,
            sweep_label: cx.multi.then_some(r.sweep.label.as_str()),
            repeat,
            repeat_state: &repeat_state,
        };
        for n in 1..=repeat {
            // Each iteration is its own bounded unit, with its own phase clock.
            check_cmd::enter_phase(check_cmd::Phase::Test);
            let tap = inventory.map(|i| i.tap(si, n));
            if let Some(tap) = &tap {
                tap.record(check_cmd::JournalRecord::LaneStarted { lane: tap.lane() });
            }
            let result = run_iteration(&plan, n, tap.as_ref());
            if let Some(tap) = &tap {
                let passed = result
                    .as_ref()
                    .is_ok_and(|r| r.outcome != Outcome::Fail && !r.timed_out);
                tap.record(check_cmd::JournalRecord::LaneFinished {
                    lane: tap.lane(),
                    passed,
                });
            }
            let stop = |e: DevError| {
                if let Some(tap) = &tap {
                    check_cmd::record_run_stop(&e, tap.decisive_termination());
                }
                e
            };
            let report = result.map_err(stop)?;
            let timed_out = report.timed_out;
            // Discovery is the same every iteration: one note per sweep.
            if n == 1 && report.outcome == Outcome::NoMatch {
                notes.push(NoMatchNote::of(r));
            }
            reports.push(report);
            // A blown time budget stops brokkr. Unlike a failing test - where a
            // `-N` run's whole purpose is to keep going and count the flakes -
            // a timeout means the run is already outside the contract, and
            // whatever wedged it is still there for the next iteration and the
            // next sweep to inherit.
            if timed_out {
                return Err(stop(stop_for_budget(reports, repeat, &r.sweep.label)));
            }
        }
    }
    Ok(())
}

/// What building a sweep again, just before it runs, came to.
pub(super) enum Settled<'s> {
    /// The build is what the preparation recorded: run the prepared plan.
    Unchanged,
    /// Another sweep's build had replaced something the plan holds. The sweep
    /// was prepared again and runs what is built now.
    Rebuilt(Box<ReadySweep<'s>>),
    /// The sweep no longer builds (already reported).
    Failed(RunReport),
}

/// Build `prepared` again and prove the build is the one its plan was taken
/// from; if it is not, supersede the plan, as a partial `check` lane does: the
/// sweep is prepared anew and runs what is built now, its lanes are journaled
/// as superseded (their inventory no longer describes what ran, so
/// reconciliation does not read their observations against it), and it says so.
/// Nothing builds between this verification and the sweep's first launch.
pub(super) fn settle_sweep<'s>(
    prepared: &ReadySweep<'s>,
    cx: &SweepContext<'_>,
    inventory: Option<&TestInventory>,
    sweep_index: usize,
    repeat: u32,
) -> Result<Settled<'s>, DevError> {
    // The re-verification runs quiet, so an error out of it (cargo could not
    // start, went idle, a hash failed) would otherwise arrive with nothing
    // saying which sweep it was.
    let rechecked = recheck_sweep(prepared, cx).inspect_err(|_| {
        output::error(&format!(
            "test {}: re-verifying the sweep's build before it runs failed",
            prepared.sweep.label
        ));
    });
    let Some(drift) = rechecked? else {
        return Ok(Settled::Failed(RunReport::bare(Outcome::BuildFailed)));
    };
    if drift.is_empty() {
        return Ok(Settled::Unchanged);
    }
    let reason = format!("artifacts changed since the plan - {}", drift.join("; "));
    if let Some(inv) = inventory {
        for n in 1..=repeat {
            let tap = inv.tap(sweep_index, n);
            tap.record(check_cmd::JournalRecord::LaneSuperseded {
                lane: tap.lane(),
                reason: reason.clone(),
            });
        }
    }
    // On the console: it explains the second build narration that follows and
    // why this sweep has no inventory.
    output::run_msg(&format!(
        "test {}: {reason}; the sweep runs what it builds now, and its inventory is unavailable",
        prepared.sweep.label
    ));
    Ok(
        match prepare_sweep(prepared.sweep, &prepared.attempt, cx)? {
            SweepOutcome::Ready(f) => Settled::Rebuilt(f),
            SweepOutcome::Done(report) => Settled::Failed(report),
        },
    )
}

/// The sweep's support builds and test build, again, held to the prepared
/// record. `Ok(None)` is a build that failed (already reported).
pub(super) fn recheck_sweep(
    prepared: &ReadySweep<'_>,
    cx: &SweepContext<'_>,
) -> Result<Option<Vec<String>>, DevError> {
    let env_refs: Vec<(&str, &str)> = prepared
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut support: Vec<check_cmd::SupportArtifact> = Vec::new();
    for req in prepared.attempt.support() {
        match run_pre_build(
            cx.project_root,
            prepared.sweep,
            req,
            &env_refs,
            &prepared.allow_args,
            prepared.profile,
        )? {
            Some(artifacts) => support.extend(artifacts),
            None => return Ok(None),
        }
    }
    let shape = BuildShape {
        sweep: prepared.sweep,
        allow_args: &prepared.allow_args,
        run: single_run(prepared.sweep, &prepared.attempt)?,
        jobs: cx.jobs,
        profile: prepared.profile,
    };
    sweep_drift(
        prepared,
        |_| prebuild_targets(&shape, &env_refs, cx.project_root, true, None),
        &support,
    )
}

/// Every way a rebuild differs from the prepared sweep: a harness gone or
/// rebuilt with other content, the runtime index (build-script facts, support
/// bins behind `CARGO_BIN_EXE_*`), and the `build_packages` executables the
/// tests spawn. `Ok(None)` is a build that failed. A doc-only sweep has no
/// harnesses to rebuild; its support binaries are still held to the record.
pub(super) fn sweep_drift(
    prepared: &ReadySweep<'_>,
    build: impl FnMut(
        usize,
    ) -> Result<
        Option<(Vec<check_cmd::TestBinary>, check_cmd::BuildRuntimeIndex)>,
        DevError,
    >,
    support: &[check_cmd::SupportArtifact],
) -> Result<Option<Vec<String>>, DevError> {
    let (artifacts, runtime, resolutions) = match &prepared.focused {
        Some(f) => (
            f.artifacts.clone(),
            Some(&prepared.runtime_fingerprint),
            vec![None],
        ),
        None => (Vec::new(), None, Vec::new()),
    };
    let check = check_cmd::DriftCheck {
        artifacts,
        target_filters: &[],
        runtime_fingerprint: runtime,
        support_fingerprint: &prepared.support_fingerprint,
    };
    check.run(&resolutions, build, support)
}

/// The run's inventory and journal: one lane per (sweep, iteration), each
/// expecting the harnesses' matched tests.
pub(super) struct TestInventory {
    pub(super) plan: check_cmd::AccountingPlan,
    pub(super) journal: PathBuf,
    /// Lane index of sweep `s`'s first iteration, per ready sweep.
    pub(super) first_lane: Vec<usize>,
}

impl TestInventory {
    /// Prepare the plan and open the journal, before the first test runs. A
    /// run whose plan cannot be persisted goes on without a record, and says
    /// so: its stop cannot be reported from it.
    pub(super) fn open(
        ready: &[ReadySweep<'_>],
        cx: &SweepContext<'_>,
        repeat: u32,
    ) -> Option<Self> {
        if ready.is_empty() {
            return None;
        }
        let mut lanes: Vec<check_cmd::LaneRecord> = Vec::new();
        let mut first_lane = Vec::new();
        for r in ready {
            first_lane.push(lanes.len());
            for n in 1..=repeat {
                let label = if repeat > 1 {
                    format!("{} (run {n}/{repeat})", r.sweep.label)
                } else {
                    r.sweep.label.clone()
                };
                lanes.push(test_lane_record(r, lanes.len(), label));
            }
        }
        let plan = check_cmd::AccountingPlan {
            run_id: check_cmd::new_run_id(),
            certifying: false,
            complete: true,
            lanes,
            ..check_cmd::AccountingPlan::default()
        };
        match check_cmd::accounting_open(cx.state_root, &plan) {
            Ok(paths) => {
                output::detail(&format!(
                    "test: run {}, plan {}, journal {}",
                    plan.run_id,
                    paths.plan.display(),
                    paths.journal.display()
                ));
                Some(Self {
                    plan,
                    journal: paths.journal,
                    first_lane,
                })
            }
            Err(e) => {
                output::error(&format!("test: the run's record could not be opened: {e}"));
                None
            }
        }
    }

    /// The tap of sweep `sweep_index`'s iteration `n`.
    pub(super) fn tap(&self, sweep_index: usize, n: u32) -> check_cmd::LaneTap {
        let lane = self.first_lane.get(sweep_index).copied().unwrap_or(0) + (n as usize - 1);
        check_cmd::LaneTap::new(lane)
    }

    /// The journal's closing line: the run is over.
    pub(super) fn close(self) -> ClosedInventory {
        check_cmd::journal_close();
        ClosedInventory {
            plan: self.plan,
            journal: self.journal,
        }
    }
}

/// An inventory whose journal is closed, ready to be read back.
pub(super) struct ClosedInventory {
    pub(super) plan: check_cmd::AccountingPlan,
    pub(super) journal: PathBuf,
}

impl ClosedInventory {
    pub(super) fn continuation(&self) -> Option<check_cmd::ContinuationReport> {
        let (records, recon) = check_cmd::reconcile_run(&self.plan, Some(&self.journal));
        check_cmd::build_continuation(&self.plan, &recon, &records)
    }
}

/// One (sweep, iteration)'s lane in the run's plan: the executions its
/// harnesses are expected to report and the artifacts' identity.
pub(super) fn test_lane_record(
    r: &ReadySweep<'_>,
    lane: usize,
    label: String,
) -> check_cmd::LaneRecord {
    use check_cmd::{Inventory, LaneIdx, LaneKind, LaneRecord, PairId, Resolution};
    let lane = LaneIdx::new(lane);
    let shape = check_cmd::shape_id(r.sweep);
    let Some(focused) = &r.focused else {
        // Doctests are not enumerable: the lane exists, expecting nothing.
        // `LaneRecord::empty` gives a doc-only lane the doctest inventory.
        return LaneRecord::empty(lane, label, LaneKind::DocOnly, shape);
    };
    let mut rec = LaneRecord::empty(lane, label, LaneKind::Serial, shape.clone());
    rec.include_ignored = true;
    let executions = focused
        .runs
        .iter()
        .flat_map(|run| {
            run.names.iter().map(|name| PairId {
                shape: shape.clone(),
                resolution: Resolution::Combined,
                unit: run.unit.clone(),
                test: name.clone().into(),
            })
        })
        .collect();
    rec.inventory = Inventory::Prepared {
        executions,
        ignored_selected: Vec::new(),
        outside_claim: Vec::new(),
        artifacts: focused.artifacts.clone(),
        unlisted: Vec::new(),
    };
    rec
}

/// One sweep, built and ready: what every `-N` iteration of it shares.
pub(super) struct SweepPlan<'a> {
    pub(super) shape: BuildShape<'a>,
    pub(super) name: &'a str,
    /// The harnesses holding a match, run directly; `None` for a doc-only
    /// sweep's single `cargo test --doc`.
    pub(super) focused: Option<&'a FocusedPlan>,
    /// The resolved full name `--timeout` runs with `--exact`.
    pub(super) exact: Option<String>,
    pub(super) env: &'a [(&'a str, &'a str)],
    pub(super) project_root: &'a Path,
    pub(super) state_root: &'a Path,
    pub(super) ceiling: Duration,
    /// The sweep's label when several sweeps run, so its tag says which.
    pub(super) sweep_label: Option<&'a str>,
    pub(super) repeat: u32,
    pub(super) repeat_state: &'a RepeatState,
}

/// Run iteration `n` of a sweep and report it, printing its footers. `tap` is
/// the iteration's lane in the run's journal.
#[allow(clippy::too_many_lines)] // linear: run, account, report, footers
pub(super) fn run_iteration(
    plan: &SweepPlan<'_>,
    n: u32,
    tap: Option<&check_cmd::LaneTap>,
) -> Result<RunReport, DevError> {
    let (pkg, name, repeat) = (plan.shape.pkg(), plan.name, plan.repeat);
    let tag = |target: Option<&str>| {
        let quals: Vec<&str> = plan.sweep_label.into_iter().chain(target).collect();
        let quals = if quals.is_empty() {
            String::new()
        } else {
            format!(" [{}]", quals.join(", "))
        };
        let run = if repeat > 1 {
            format!(" run {n}/{repeat}")
        } else {
            String::new()
        };
        format!("{pkg}::{name}{quals}{run}")
    };
    // Under `-N`, the invocation and build-time framing is identical every
    // iteration - print it for run 1 only and let repeats collapse to their
    // PASS/FAIL footer line.
    let announce = n == 1;

    let Some(focused) = plan.focused else {
        // A doc-only sweep: one rustdoc run over the package, the user's
        // substring unchanged.
        let s = &plan.shape;
        let args = test_argv(s, name);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let invocation = format!(
            "cargo {} (sweep: {}; cwd {})",
            arg_refs.join(" "),
            s.sweep.label,
            plan.project_root.display()
        );
        if announce {
            output::detail(&invocation);
        }
        let report = run_one(
            test_runner::Launch::Cargo,
            &arg_refs,
            plan.project_root,
            plan.state_root,
            plan.env,
            &OneRun {
                tag: &tag(None),
                test_tag: None,
                target: "",
                ceilings: ceilings_for(&None, plan.ceiling),
                announce,
                expected: None,
                listed: None,
                // Doctests have no inventory: nothing to attribute them to.
                observe: test_runner::Observe::default(),
            },
            plan.repeat_state,
            n > 1,
        )
        .inspect_err(|_| show_failed_invocation(plan.repeat_state, &invocation))?;
        show_reproduction(
            plan.repeat_state,
            matches!(report.outcome, Outcome::Fail | Outcome::BuildFailed),
            &invocation,
        );
        return Ok(report);
    };

    if focused.runs.is_empty() {
        // Nothing prints here: whether "no match in this sweep" is news depends
        // on whether another sweep ran the name, which only the caller knows
        // (see `NoMatchNote`).
        return Ok(RunReport::bare(Outcome::NoMatch));
    }

    // Under `--timeout`, the resolved name exactly; otherwise the user's
    // substring, unchanged.
    let filter = plan.exact.as_deref().unwrap_or(name);
    let args = direct_libtest_args(filter, plan.exact.is_some());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut parts = Vec::new();
    for run in &focused.runs {
        // The cwd rides the invocation line: the harness runs outside cargo,
        // under the envelope's cwd, and this is the one place it is named.
        // It goes to the run log; the console shows it only for a harness
        // that fails, where it is the reproduction line.
        let invocation = format!(
            "{} {} (cwd {})",
            run.program,
            arg_refs.join(" "),
            run.cwd.display()
        );
        if announce {
            output::detail(&invocation);
        }
        let test_tag = |test: &str, wall: &str| lone_test_tag(plan, &run.label, n, test, wall);
        let env: Vec<(&str, &str)> = run
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // The harness's stream is this binary's, and under `--timeout` this
        // one test's: what lets a kill or a crash be charged to it.
        let observe = tap.map_or_else(test_runner::Observe::default, |tap| {
            let origin = check_cmd::StreamOrigin::Binary {
                resolution: None,
                unit: run.unit.clone(),
                one_test: plan.exact.clone(),
            };
            test_runner::Observe {
                sink: Some(tap.sink(move |_| origin.clone(), false)),
                ..Default::default()
            }
        });
        let report = run_one(
            test_runner::Launch::Direct {
                program: &run.program,
            },
            &arg_refs,
            &run.cwd,
            plan.state_root,
            &env,
            &OneRun {
                tag: &tag(Some(&run.label)),
                test_tag: Some(&test_tag),
                target: &run.label,
                ceilings: ceilings_for(&plan.exact, plan.ceiling),
                announce: false,
                expected: Some(run.matched),
                listed: Some(&run.names),
                observe,
            },
            plan.repeat_state,
            n > 1,
        )
        .inspect_err(|_| show_failed_invocation(plan.repeat_state, &invocation))?;
        show_reproduction(
            plan.repeat_state,
            matches!(report.outcome, Outcome::Fail | Outcome::BuildFailed),
            &invocation,
        );
        // A blown budget stops everything, the remaining harnesses included.
        let stop = report.timed_out;
        parts.push(report);
        if stop {
            break;
        }
    }
    Ok(RunReport::merge(parts))
}
