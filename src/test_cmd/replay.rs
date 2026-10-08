//! `brokkr test --from-run <run-id>`: run a recorded run's unresolved
//! executions again, diagnostically.
//!
//! The selection is exactly the original executions whose reconciled outcome
//! is `interrupted` or `unobserved` - the ones a stop left without a verdict.
//! Failed, timed-out, passed and ignored executions stand as the earlier
//! results they are. Nothing is deduplicated by name or by pair: the same test
//! selected by two lanes is two executions, and each is replayed under its own
//! lane's recipe.
//!
//! What it runs is what the original lane would have: the recipe the plan
//! stored ([`check_cmd::LaneReplay`]) names the cargo build, the launch
//! environment, the execution model and the thread policy, and `brokkr.toml`
//! is never consulted (`--from-run` is dispatched before the configuration is
//! even parsed). Before anything is executed every lane is built again with
//! the recorded shape and every executable, the runtime index and the support
//! builds are held to the fingerprints the original run recorded; a difference
//! refuses the whole replay, because changed source is a new experiment for an
//! ordinary invocation, not evidence about the run being continued. That
//! preflight is not what a launch relies on: lanes can share a build output
//! path, so each lane is built and verified AGAIN immediately before it
//! launches ([`run_selections`]), with nothing built after that verification.
//!
//! A serial shared-process lane's unresolved subset runs together in ONE
//! harness process per binary - never auto-isolated, which would hide the
//! interaction being chased. Libtest selection is by full name with `--exact`;
//! every original selection predicate (every `--skip`) is dropped once the
//! selection is resolved to names. A group with no names never launches (an
//! empty positive filter runs the whole harness), and an argv past the system
//! limit refuses the replay rather than splitting a shared process.
//!
//! It is a DIAGNOSTIC. It writes its own immutable record - a plan naming the
//! source run and the original `ExecutionId`s it selected, and its own
//! journal - and never appends to or edits the source's, never changes the
//! source's verdict, and certifies nothing. A continuation whose selected
//! executions all pass exits zero and says `diagnostic_completed`: those
//! executions passed in a new process against the current external state, and
//! the original run stays failed. `--list` prints the report from the
//! persisted evidence and executes nothing - the recovery path when a hard exit
//! stopped the original run from printing it.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::check_cmd::{
    self, Candidate, ExecutionId, LaneRecord, LaneReplay, LaneTap, ReplayGroup, ReplayModel,
};
use crate::error::DevError;
use crate::output;
use crate::test_runner::{self, LibtestOutcome};

use super::{Outcome, OneRun, RepeatState, RunReport};

/// One lane's share of the replay: its record, its recipe, and the groups of
/// original executions selected from it.
struct LaneSelection {
    record: LaneRecord,
    groups: Vec<ReplayGroup>,
    /// The thread counts the source run's parallel executor allocated, from
    /// its journal (empty for every other model).
    allocs: check_cmd::ThreadAllocations,
}

impl LaneSelection {
    fn replay(&self) -> &LaneReplay {
        // `replay_refusals` has already refused a lane without a recipe.
        self.record.replay.as_ref().unwrap_or_else(|| unreachable!("a lane without a recipe is refused first"))
    }
}

/// The continuation of run `run_id`. `list` prints the report and executes
/// nothing.
pub(super) fn from_run(
    project_root: &Path,
    state_root: &Path,
    run_id: &str,
    list: bool,
) -> Result<(), DevError> {
    let src = check_cmd::load_run(state_root, run_id)?;
    let recon = check_cmd::reconcile(&src.plan, &src.records, src.journal_closed, src.journal_errors.clone());
    let candidates = check_cmd::candidates_of(&recon);

    if list {
        match check_cmd::build_continuation(&src.plan, &recon, &src.records) {
            Some(report) => output::run_msg(&check_cmd::render_continuation(&report).join("\n")),
            None => output::run_msg(&format!("run {run_id}: nothing is unresolved - no execution is interrupted or unobserved")),
        }
        return Ok(());
    }
    if candidates.is_empty() {
        output::run_msg(&format!(
            "run {run_id}: no execution is interrupted or unobserved, so there is nothing to run; \
             failed, timed-out, passed and ignored executions stand as the results they are"
        ));
        return Ok(());
    }

    if let Some(inv) = &src.plan.invocation
        && Path::new(&inv.project_root) != project_root
    {
        return Err(DevError::Config(format!(
            "run {run_id} was recorded against {}, and this invocation is in {}: its recorded \
             executables and environment belong to that checkout, so it is not replayed from here",
            inv.project_root,
            project_root.display()
        )));
    }
    let allocs = check_cmd::thread_allocations(&src.records);
    let selections = select_lanes(&src.plan, &candidates, &allocs)?;

    let source_verdict_note = format!(
        "continuing run {run_id}: {} - diagnostic only, certifies nothing, and run {run_id} keeps its verdict",
        output::count(candidates.len(), "recorded execution")
    );
    output::run_msg(&source_verdict_note);

    // Preflight: nothing runs until EVERY lane is proven replayable - rebuilt
    // with its recorded shape and held to the recorded fingerprints - so a
    // replay of changed source refuses before it executes anything. This is
    // not the verification a launch relies on: a later lane's build can
    // replace an earlier lane's support executable at the same path after
    // this has passed, so each lane is built and verified again immediately
    // before it launches ([`run_selections`]).
    check_cmd::enter_phase("test");
    for sel in &selections {
        arm_lane(project_root, state_root, run_id, sel)?;
    }

    let plan = continuation_plan(run_id, &candidates, &selections, project_root);
    let journal = check_cmd::accounting_open(state_root, &plan, Some(run_id)).map_err(|e| {
        DevError::Build(format!("the continuation's own record could not be opened: {e}"))
    })?;
    output::detail(&format!(
        "continuation {}: plan {}, journal {}",
        plan.run_id,
        journal.plan.display(),
        journal.journal.display()
    ));
    let ran = run_selections(
        &selections,
        |sel| arm_lane(project_root, state_root, run_id, sel),
        |sel, armed, tap| run_lane(state_root, sel, armed, tap),
    );
    check_cmd::journal_close();

    let (records, own) = check_cmd::reconcile_run(&plan, Some(&journal.journal));
    let total = own.accounted.len();
    let passed = own.count(check_cmd::Outcome::Passed);
    if let Some(c) = check_cmd::build_continuation(&plan, &own, &records) {
        // Killed again: what is still unresolved is itself continuable.
        output::error(&check_cmd::render_continuation(&c).join("\n"));
    }
    let lanes_passed = ran?;
    if completed(&own, lanes_passed) {
        output::result_msg(&format!(
            "continuation of {run_id} (record {}): diagnostic_completed - {} passed in a new process \
             against the current state of the machine. Run {run_id} stays failed; nothing here certifies it.",
            plan.run_id,
            output::count(total, "execution")
        ));
        return Ok(());
    }
    output::error(&format!(
        "continuation of {run_id} (record {}): {passed} of {total} executions passed, {} failed, {} \
         timed out, {} interrupted, {} unobserved, {} anomalies{}{}",
        plan.run_id,
        own.count(check_cmd::Outcome::Failed),
        own.count(check_cmd::Outcome::TimedOut),
        own.count(check_cmd::Outcome::Interrupted),
        own.count(check_cmd::Outcome::Unobserved),
        own.anomalies.len(),
        if lanes_passed { "" } else { "; a lane reported a failure" },
        if own.journal_closed && own.journal_errors.is_empty() && own.truncated_streams.is_empty() {
            ""
        } else {
            "; the continuation's own record is incomplete"
        },
    ));
    Err(DevError::Build("test failed".into()))
}

/// Whether a continuation may call itself `diagnostic_completed`. Every
/// condition is required: the executors reported no failure (a harness can
/// pass every test and then exit nonzero or abort in teardown), and the
/// continuation's OWN record is whole and green - its journal closed with no
/// error, every stream read to its end, every selected execution passed, no
/// anomaly, nothing recorded that stopped it. Counting passes alone would
/// declare completion over a record that cannot show it.
fn completed(own: &check_cmd::Reconciliation, lanes_passed: bool) -> bool {
    lanes_passed && own.green()
}

/// Group the candidates by lane and refuse what the recipe cannot launch,
/// with every reason at once.
fn select_lanes(
    plan: &check_cmd::AccountingPlan,
    candidates: &[Candidate],
    allocs: &check_cmd::ThreadAllocations,
) -> Result<Vec<LaneSelection>, DevError> {
    let mut by_lane: Vec<(usize, Vec<ReplayGroup>)> = Vec::new();
    for (lane, group) in check_cmd::replay_groups(candidates) {
        match by_lane.iter_mut().find(|(l, _)| *l == lane) {
            Some((_, groups)) => groups.push(group),
            None => by_lane.push((lane, vec![group])),
        }
    }
    let mut refusals: Vec<String> = Vec::new();
    let mut out: Vec<LaneSelection> = Vec::new();
    for (lane, groups) in by_lane {
        let Some(record) = plan.lane(lane) else {
            refusals.push(format!("lane {lane} is not in the plan"));
            continue;
        };
        let why = check_cmd::replay_refusals(record, &groups, allocs);
        if why.is_empty() {
            out.push(LaneSelection { record: record.clone(), groups, allocs: allocs.clone() });
        } else {
            refusals.extend(why);
        }
    }
    if refusals.is_empty() {
        out.sort_by_key(|s| s.record.lane);
        return Ok(out);
    }
    Err(DevError::Config(format!(
        "run {} cannot be replayed from its record:\n  {}",
        plan.run_id,
        refusals.join("\n  ")
    )))
}

/// What [`arm_lane`] leaves ready to launch.
enum Armed {
    /// The libtest lanes launch the verified executables directly.
    Libtest,
    /// The engine lane launches the very listing it was verified from: its
    /// build and its verification are one step, so nothing builds between the
    /// two.
    Engine(Box<check_cmd::PreparedLane>),
}

/// Build one lane with its recorded shape and hold every artifact, the
/// runtime index, the support builds - and, on the engine lane, the engine's
/// launch environment - to what the original run recorded. The support builds
/// run first: they re-uplift the support executables this lane's tests reach
/// through `BROKKR_TEST_BIN_DIR`, wherever another lane's build left them.
///
/// Called twice per lane: once for every lane before anything executes, and
/// again immediately before the lane launches. Nothing builds after the call
/// that precedes a launch.
fn arm_lane(project_root: &Path, state_root: &Path, run_id: &str, sel: &LaneSelection) -> Result<Armed, DevError> {
    let replay = sel.replay();
    let env_refs: Vec<(&str, &str)> = replay.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut support: Vec<check_cmd::SupportArtifact> = Vec::new();
    for build in &replay.support_builds {
        support.extend(run_support_build(project_root, &sel.record.label, build, &env_refs)?);
    }
    if replay.model == ReplayModel::Nextest {
        let prepared = prepare_engine(project_root, state_root, sel)?;
        refuse_drift(run_id, sel, &engine_drift(sel, &prepared, &support)?)?;
        return Ok(Armed::Engine(Box::new(prepared)));
    }
    verify_with(
        run_id,
        sel,
        |i| check_cmd::test_binaries_with_runtime(project_root, &replay.resolutions[i].build_args, &env_refs, false),
        &support,
    )?;
    Ok(Armed::Libtest)
}

/// The refusal a difference between the recorded lane and what it builds now
/// makes. Changed source is a new experiment for an ordinary invocation; it is
/// no evidence about the run being continued.
fn refuse_drift(run_id: &str, sel: &LaneSelection, drift: &[String]) -> Result<(), DevError> {
    if drift.is_empty() {
        return Ok(());
    }
    Err(DevError::Config(format!(
        "run {run_id} is not replayed: lane '{}' no longer builds what the original run recorded - {}. \
         Changed source is a new experiment: run the tests in the ordinary way instead.",
        sel.record.label,
        drift.join("; ")
    )))
}

/// The lane's comparison, over a given build step: how the tests drive it
/// without cargo.
fn verify_with(
    run_id: &str,
    sel: &LaneSelection,
    build: impl FnMut(usize) -> Result<Option<(Vec<check_cmd::TestBinary>, check_cmd::BuildRuntimeIndex)>, DevError>,
    support: &[check_cmd::SupportArtifact],
) -> Result<(), DevError> {
    let replay = sel.replay();
    let resolutions: Vec<Option<String>> = replay.resolutions.iter().map(|r| r.resolution.clone()).collect();
    match drift_check(sel).run(&resolutions, build, support)? {
        None => Err(DevError::Reported(format!(
            "lane '{}' could not be rebuilt with its recorded shape, so run {run_id} is not replayed",
            sel.record.label
        ))),
        Some(d) => refuse_drift(run_id, sel, &d),
    }
}

/// What the lane was recorded as, to be held against a fresh build.
fn drift_check(sel: &LaneSelection) -> check_cmd::DriftCheck<'_> {
    let replay = sel.replay();
    check_cmd::DriftCheck {
        artifacts: sel.record.artifacts.clone(),
        target_filters: &replay.target_filters,
        runtime_fingerprint: replay.runtime_fingerprint.as_ref(),
        support_fingerprint: &replay.support_fingerprint,
    }
}

/// The engine lane's build, as the engine reads it: the recorded cargo
/// selection, listed under a filterset selecting exactly the replayed
/// (binary, test) pairs. The sweep env is the recorded one; what the engine
/// resolves from cargo configuration itself is fingerprinted into the result
/// and compared by [`engine_drift`].
fn prepare_engine(project_root: &Path, state_root: &Path, sel: &LaneSelection) -> Result<check_cmd::PreparedLane, DevError> {
    let replay = sel.replay();
    let Some(resolution) = replay.resolutions.first() else {
        return Err(DevError::Config(format!("lane '{}' recorded no build", sel.record.label)));
    };
    let pairs: Vec<(String, String)> = sel
        .groups
        .iter()
        .flat_map(|g| g.tests.iter().map(|t| (g.unit.id(), t.clone())))
        .collect();
    let target_dir = crate::build::project_info(Some(project_root))?.target_dir;
    let inputs = check_cmd::LaneInputs {
        project: None,
        project_root,
        state_root,
        target_dir: &target_dir,
        allow_flags: &[],
        commands: false,
        certifying: false,
    };
    // The recorded build selection already carries the lint allows it was
    // built with; the env is the recorded sweep env.
    let env = check_cmd::LaneEnv { allow_args: Vec::new(), project_env: Vec::new(), env: replay.env.clone() };
    check_cmd::build_nextest_lane(
        &inputs,
        &sel.record.label,
        resolution.build_args.clone(),
        &env,
        &check_cmd::EngineFilter::Exact { pairs: &pairs, ignored_mode: replay.ignored_mode },
        replay.ignored_mode,
    )
}

/// Every way a freshly built engine lane differs from the recorded one: the
/// executables, the runtime index, the support builds, and the engine's launch
/// environment.
fn engine_drift(
    sel: &LaneSelection,
    prepared: &check_cmd::PreparedLane,
    support: &[check_cmd::SupportArtifact],
) -> Result<Vec<String>, DevError> {
    let replay = sel.replay();
    let current: Vec<(Option<String>, String, String)> = prepared
        .resolutions
        .iter()
        .flat_map(|r| r.binaries.iter().map(|b| (r.resolution.clone(), b.binary.executable.clone(), b.hash.clone())))
        .collect();
    let runtime_now = prepared.runtime_fingerprint.clone().unwrap_or_default();
    let mut drift = drift_check(sel).compare(&current, &runtime_now, &check_cmd::support_fingerprint(support)?);
    drift.extend(check_cmd::engine_launch_drift(&replay.engine_launch, &prepared.engine_launch));
    Ok(drift)
}

/// One recorded `build_packages` pre-build, again.
fn run_support_build(
    project_root: &Path,
    lane: &str,
    build: &check_cmd::SupportBuild,
    env: &[(&str, &str)],
) -> Result<Vec<check_cmd::SupportArtifact>, DevError> {
    let args: Vec<&str> = build.args.iter().map(String::as_str).collect();
    output::detail(&format!("cargo {} (replay support build: {lane})", build.args.join(" ")));
    let captured = super::cargo_with_deadline(&args, project_root, env, "replay support build")?;
    if !captured.status.success() {
        output::error(&format!("failing command: cargo {}", build.args.join(" ")));
        output::error(&String::from_utf8_lossy(&captured.stderr));
        return Err(DevError::Build(format!(
            "build failed for package '{}' in lane '{lane}'",
            build.package
        )));
    }
    Ok(check_cmd::support_artifacts(&String::from_utf8_lossy(&captured.stdout), &build.package))
}

/// The continuation's own plan: the source's lanes that were selected, each
/// expecting exactly the selected executions - under the source's lane indices,
/// so an `ExecutionId` means the same thing in both records.
fn continuation_plan(
    run_id: &str,
    candidates: &[Candidate],
    selections: &[LaneSelection],
    project_root: &Path,
) -> check_cmd::AccountingPlan {
    let lanes = selections
        .iter()
        .map(|sel| LaneRecord {
            executions: candidates
                .iter()
                .filter(|c| c.id.lane == sel.record.lane)
                .map(|c| c.id.pair.clone())
                .collect(),
            ignored_selected: Vec::new(),
            outside_claim: Vec::new(),
            doc_carrier: false,
            doc_streams_required: false,
            ..sel.record.clone()
        })
        .collect();
    check_cmd::AccountingPlan {
        run_id: check_cmd::new_run_id(),
        certifying: false,
        complete: true,
        lanes,
        invocation: Some(check_cmd::Invocation {
            cwd: std::env::current_dir().map_or_else(|_| ".".to_owned(), |p| p.to_string_lossy().into_owned()),
            project_root: project_root.to_string_lossy().into_owned(),
        }),
        continuation: Some(check_cmd::ContinuationOf {
            source_run_id: run_id.to_owned(),
            selected: candidates.iter().map(|c| c.id.clone()).collect::<Vec<ExecutionId>>(),
        }),
        ..check_cmd::AccountingPlan::default()
    }
}

/// Execute every selected lane in order, each built and verified IMMEDIATELY
/// before it launches (`arm`), and nothing built between that verification and
/// the launch. Preflight (every lane, before anything runs) is not enough on
/// its own: lanes of one invocation can share a build output path, so a later
/// lane's build replaces what an earlier lane's verification saw. A lane that
/// no longer verifies when its turn comes refuses the rest of the replay -
/// journaled run-wide, like any stop - and the lanes already run stand.
///
/// A blown time budget, or any error, stops the replay and is journaled
/// run-wide, so what it did not reach reads as unobserved for that reason.
/// `Ok(false)` is a replay that ran to its end with a lane reporting a failure.
fn run_selections<A>(
    selections: &[LaneSelection],
    mut arm: impl FnMut(&LaneSelection) -> Result<A, DevError>,
    mut run: impl FnMut(&LaneSelection, A, &LaneTap) -> Result<LaneResult, DevError>,
) -> Result<bool, DevError> {
    let mut all_passed = true;
    for sel in selections {
        // Each lane is its own bounded unit, with its own phase clock, which
        // its rebuild is charged to.
        check_cmd::enter_phase("test");
        let armed = match arm(sel) {
            Ok(a) => a,
            Err(e) => {
                check_cmd::record_run_stop(&e, None);
                return Err(e);
            }
        };
        let tap = LaneTap::new(sel.record.lane);
        tap.record(check_cmd::JournalRecord::LaneStarted { lane: sel.record.lane });
        let result = run(sel, armed, &tap);
        let passed = matches!(result, Ok(LaneResult { passed: true, .. }));
        tap.record(check_cmd::JournalRecord::LaneFinished { lane: sel.record.lane, passed });
        match result {
            Err(e) => {
                check_cmd::record_run_stop(&e, tap.decisive_termination());
                return Err(e);
            }
            Ok(r) => {
                all_passed &= r.passed;
                if r.timed_out {
                    let e = DevError::Verify(format!(
                        "a replayed test exceeded its time budget in lane '{}' - stopping",
                        sel.record.label
                    ));
                    check_cmd::record_run_stop(&e, tap.decisive_termination());
                    return Err(e);
                }
            }
        }
    }
    if !all_passed {
        // The footer states the counts; the reconciliation decides the exit.
        output::detail("replay: at least one lane reported a failure");
    }
    Ok(all_passed)
}

/// How one lane's replay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaneResult {
    passed: bool,
    /// A test blew its time budget: stops the replay.
    timed_out: bool,
}

impl From<check_cmd::EngineRun> for LaneResult {
    fn from(r: check_cmd::EngineRun) -> Self {
        Self { passed: r.passed, timed_out: r.timed_out }
    }
}

fn run_lane(state_root: &Path, sel: &LaneSelection, armed: Armed, tap: &LaneTap) -> Result<LaneResult, DevError> {
    let replay = sel.replay();
    let ceiling = Duration::from_secs(replay.per_test_ceiling_secs);
    output::run_msg(&format!(
        "replay {}: {} ({}), {}",
        sel.record.label,
        output::count(sel.groups.iter().map(|g| g.tests.len()).sum::<usize>(), "execution"),
        replay.model.as_str(),
        match sel.groups.len() {
            1 => "1 binary".to_owned(),
            n => format!("{n} binaries"),
        }
    ));
    match (replay.model, armed) {
        (ReplayModel::SerialShared, _) => run_shared(state_root, sel, tap, ceiling),
        (ReplayModel::Parallel, _) => run_parallel(state_root, sel, tap, ceiling),
        (ReplayModel::Isolated, _) => run_isolated(state_root, sel, tap, ceiling),
        (ReplayModel::Nextest, Armed::Engine(prepared)) => run_engine(sel, &prepared, tap),
        (ReplayModel::Nextest, Armed::Libtest) => Err(DevError::Build(format!(
            "lane '{}' reached the engine unprepared",
            sel.record.label
        ))),
    }
}

/// The recorded launch of one group's binary.
fn recorded_binary<'a>(replay: &'a LaneReplay, group: &ReplayGroup) -> Result<&'a check_cmd::BinaryReplay, DevError> {
    replay
        .resolutions
        .iter()
        .filter(|r| r.resolution == group.resolution)
        .flat_map(|r| r.binaries.iter())
        .find(|b| check_cmd::BinaryUnit::of(&b.binary) == group.unit)
        .ok_or_else(|| DevError::Config(format!("the recipe does not hold {}", group.unit.id())))
}

fn origin_of(group: &ReplayGroup, one_test: Option<String>) -> check_cmd::StreamOrigin {
    check_cmd::StreamOrigin::Binary { resolution: group.resolution.clone(), unit: group.unit.clone(), one_test }
}

fn env_pairs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

/// Serial shared-process: one harness process per binary, all of its selected
/// tests in it, one at a time under the per-test cap.
fn run_shared(state_root: &Path, sel: &LaneSelection, tap: &LaneTap, ceiling: Duration) -> Result<LaneResult, DevError> {
    let replay = sel.replay();
    let repeat_state = RepeatState::default();
    let mut result = LaneResult { passed: true, timed_out: false };
    for group in &sel.groups {
        let binary = recorded_binary(replay, group)?;
        let threads = replay.test_threads.unwrap_or(1);
        let Some(args) = check_cmd::libtest_replay_argv(replay, &group.tests, threads) else {
            // Never launched: an empty positive filter would run the whole harness.
            continue;
        };
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let ceilings = match group.tests.as_slice() {
            [only] => test_runner::Ceilings::one_test(ceiling, only.clone()),
            _ => test_runner::Ceilings::shared_harness_with(ceiling),
        };
        let origin = origin_of(group, None);
        let observe = test_runner::Observe {
            sink: Some(tap.sink(move |_| origin.clone(), false)),
            ..Default::default()
        };
        let tag = format!("{} {}", sel.record.label, binary.binary.label());
        let report: RunReport = super::run_one(
            test_runner::Launch::Direct { program: &binary.binary.executable },
            &arg_refs,
            Path::new(&binary.cwd),
            state_root,
            &env_pairs(&binary.env),
            &OneRun {
                tag: &tag,
                target: &binary.binary.label(),
                ceilings,
                announce: false,
                expected: Some(group.tests.len()),
                observe,
            },
            &repeat_state,
            false,
        )?;
        result.passed &= report.outcome == Outcome::Pass;
        if report.timed_out {
            result.timed_out = true;
            break;
        }
    }
    Ok(result)
}

/// One parallel launch: the group, its libtest argv, its thread count.
type ParallelLaunch<'a> = (&'a ReplayGroup, Vec<String>, u32);

/// A parallel lane's launches: each group with its libtest argv and the thread
/// count (= budget slots claimed) the ORIGINAL executor allocated that binary,
/// read from the source journal. The original allocation came from measured
/// serial costs capped at each binary's slowest test; recomputing from the
/// remaining test counts would change how many tests share a process at once.
/// A group with no names yields no launch (an empty positive filter would run
/// the whole harness).
fn parallel_launches(sel: &LaneSelection) -> Result<Vec<ParallelLaunch<'_>>, DevError> {
    let replay = sel.replay();
    let mut out = Vec::new();
    for group in &sel.groups {
        let threads = check_cmd::group_threads(replay, sel.record.lane, group, &sel.allocs)
            .map_err(|why| DevError::Config(format!("{}: {why}", sel.record.label)))?;
        if let Some(args) = check_cmd::libtest_replay_argv(replay, &group.tests, threads) {
            out.push((group, args, threads));
        }
    }
    Ok(out)
}

/// Parallel: the lane's binaries concurrently under its recorded budget, each
/// with the thread count the original run allocated it.
fn run_parallel(state_root: &Path, sel: &LaneSelection, tap: &LaneTap, ceiling: Duration) -> Result<LaneResult, DevError> {
    let replay = sel.replay();
    let budget = replay.parallel_budget.unwrap_or(1).max(1);
    let pool = check_cmd::Budget::new(budget);
    let abort = AtomicBool::new(false);
    let mut runs: Vec<(String, Result<test_runner::ParallelRun, DevError>)> = Vec::new();
    let mut launches: Vec<(&ReplayGroup, &check_cmd::BinaryReplay, Vec<String>, u32)> = Vec::new();
    for (group, args, slots) in parallel_launches(sel)? {
        let binary = recorded_binary(replay, group)?;
        // Journaled again, so a continuation that is itself cut short can be
        // continued under the same policy.
        tap.record(check_cmd::JournalRecord::ThreadAllocation {
            lane: tap.lane(),
            resolution: group.resolution.clone(),
            unit: group.unit.clone(),
            threads: slots,
        });
        launches.push((group, binary, args, slots));
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = launches
            .iter()
            .map(|(group, binary, args, slots)| {
                let (pool, abort) = (&pool, &abort);
                scope.spawn(move || {
                    pool.acquire(*slots)?;
                    if abort.load(std::sync::atomic::Ordering::SeqCst) {
                        pool.release(*slots);
                        return Err(DevError::Interrupted);
                    }
                    let origin = origin_of(group, None);
                    let sink = tap.sink(move |_| origin.clone(), false);
                    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                    let out = test_runner::run_libtest_parallel(
                        &binary.binary.executable,
                        &arg_refs,
                        Path::new(&binary.cwd),
                        state_root,
                        &env_pairs(&binary.env),
                        test_runner::PARALLEL_SWEEP_TIMEOUT,
                        ceiling,
                        Some(abort),
                        Some(&sink),
                        |_| {},
                        |_| {},
                        |_| {},
                    );
                    if out.as_ref().is_ok_and(|r| r.timed_out || matches!(r.outcome, LibtestOutcome::HungTest(_))) {
                        abort.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    pool.release(*slots);
                    out
                })
            })
            .collect();
        for ((_, binary, _, _), h) in launches.iter().zip(handles) {
            runs.push((
                binary.binary.label(),
                h.join().unwrap_or_else(|_| Err(DevError::Build("a replay thread panicked".into()))),
            ));
        }
    });
    if crate::shutdown::is_shutdown_requested() {
        tap.terminate(check_cmd::TerminationScope::Lane, check_cmd::stop_cause(), None);
        return Err(DevError::Interrupted);
    }
    if abort.load(std::sync::atomic::Ordering::SeqCst) {
        tap.terminate(check_cmd::TerminationScope::Lane, check_cmd::TerminationCause::SiblingTimeout, None);
    }
    let mut result = LaneResult { passed: true, timed_out: false };
    for (label, run) in runs {
        match run {
            Err(DevError::Interrupted) => {}
            Err(e) => {
                output::error(&format!("replay {} {label}: {e}", sel.record.label));
                result.passed = false;
            }
            Ok(r) => report_captured(sel, &label, &r, &mut result, state_root),
        }
    }
    Ok(result)
}

/// Print one finished process's verdict the way the lanes do: a timeout or a
/// hang is the run's budget blown, a failure carries the test's own output.
fn report_captured(
    sel: &LaneSelection,
    label: &str,
    run: &test_runner::ParallelRun,
    result: &mut LaneResult,
    state_root: &Path,
) {
    if run.timed_out {
        output::error(&format!("replay {} {label} exceeded the parallel test timeout and was killed", sel.record.label));
        result.passed = false;
        result.timed_out = true;
    } else if let LibtestOutcome::HungTest(hung) = &run.outcome {
        output::error(&format!("replay {} {label}:", sel.record.label));
        output::error(&test_runner::format_hung_test(hung, state_root));
        result.passed = false;
        result.timed_out = true;
    } else if !run.captured.status.success() {
        let stdout = String::from_utf8_lossy(&run.captured.stdout);
        let stderr = String::from_utf8_lossy(&run.captured.stderr);
        output::error(&format!("replay {} {label} failed:", sel.record.label));
        output::error(&crate::cargo_filter::filter_test(&stdout, &stderr));
        result.passed = false;
    }
}

/// Isolated: one process per test, one at a time, each under the standard cap.
fn run_isolated(state_root: &Path, sel: &LaneSelection, tap: &LaneTap, ceiling: Duration) -> Result<LaneResult, DevError> {
    let replay = sel.replay();
    let mut result = LaneResult { passed: true, timed_out: false };
    for group in &sel.groups {
        let binary = recorded_binary(replay, group)?;
        for name in &group.tests {
            let args = check_cmd::isolated_replay_argv(replay, name);
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let origin = origin_of(group, Some(name.clone()));
            let sink = tap.sink(move |_| origin.clone(), false);
            let out = test_runner::run_libtest_parallel(
                &binary.binary.executable,
                &arg_refs,
                Path::new(&binary.cwd),
                state_root,
                &env_pairs(&binary.env),
                ceiling,
                ceiling,
                None,
                Some(&sink),
                |_| {},
                |_| {},
                |_| {},
            )?;
            let label = format!("{} {name}", binary.binary.label());
            report_captured(sel, &label, &out, &mut result, state_root);
            if result.timed_out {
                // The deadline is charged to that one execution; everything the
                // lane will now never run is stopped by a sibling's timeout.
                tap.record(check_cmd::JournalRecord::Terminated(check_cmd::Termination {
                    scope: check_cmd::TerminationScope::Lane,
                    lane: Some(tap.lane()),
                    stream: None,
                    cause: check_cmd::TerminationCause::PerTestDeadline,
                    test: Some(name.clone()),
                    charged: Some(check_cmd::ChargedTo {
                        resolution: group.resolution.clone(),
                        unit: group.unit.clone(),
                    }),
                }));
                tap.terminate(check_cmd::TerminationScope::Lane, check_cmd::TerminationCause::SiblingTimeout, None);
                return Ok(result);
            }
        }
    }
    Ok(result)
}

/// The nextest engine, process-per-test, executing the listing it was
/// verified from. The engine is fail-fast; a test that blew its per-test
/// budget is reported structurally, so the replay stops there like every
/// other lane does.
fn run_engine(sel: &LaneSelection, prepared: &check_cmd::PreparedLane, tap: &LaneTap) -> Result<LaneResult, DevError> {
    let Some(np) = &prepared.nextest else {
        return Err(DevError::Build(format!("lane '{}' reached the engine unprepared", sel.record.label)));
    };
    check_cmd::run_nextest_engine(&sel.record.label, sel.replay().test_threads, np, tap).map(LaneResult::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::check_cmd::{
        AccountingPlan, BinaryUnit, ExecutionId, Invocation, JournalRecord, LaneKind, PairId, PlannedArtifact,
        Termination, TerminationCause, TerminationScope,
    };
    use crate::test_runner::ObsEvent;

    fn unit() -> BinaryUnit {
        BinaryUnit {
            package_id: "path+file:///x/core#core@0.1.0".into(),
            package: "core".into(),
            kind: "test".into(),
            target: "suite".into(),
        }
    }

    fn binary_at(path: &Path) -> check_cmd::TestBinary {
        let mut b = check_cmd::test_binary_for_tests("core", "test", "suite");
        b.executable = path.to_string_lossy().into_owned();
        b
    }

    fn pair(test: &str) -> PairId {
        PairId { shape: "s".into(), resolution: None, unit: unit(), test: test.into() }
    }

    /// A lane of two tests whose executable is `exe` with planned hash `hash`.
    fn lane_record(exe: &Path, hash: &str) -> LaneRecord {
        LaneRecord {
            prepared: true,
            executions: vec![pair("a"), pair("b")],
            artifacts: vec![PlannedArtifact {
                resolution: None,
                unit: unit(),
                executable: exe.to_string_lossy().into_owned(),
                hash: hash.into(),
            }],
            replay: Some(LaneReplay {
                version: check_cmd::REPLAY_VERSION,
                refusal: None,
                model: ReplayModel::SerialShared,
                ignored_mode: check_cmd::IgnoredMode::Exclude,
                engine_launch: Vec::new(),
                test_threads: Some(1),
                parallel_budget: None,
                harness_args: Vec::new(),
                per_test_ceiling_secs: 20,
                env: Vec::new(),
                support_builds: Vec::new(),
                target_filters: Vec::new(),
                runtime_fingerprint: None,
                support_fingerprint: Vec::new(),
                resolutions: vec![check_cmd::ResolutionReplay {
                    resolution: None,
                    build_args: Vec::new(),
                    binaries: vec![check_cmd::BinaryReplay {
                        binary: binary_at(exe),
                        cwd: "/x/core".into(),
                        env: Vec::new(),
                    }],
                }],
            }),
            ..LaneRecord::empty(0, "default".into(), LaneKind::Serial, "s".into())
        }
    }

    /// Write a recorded run to disk as the accounting module does: its plan,
    /// and a journal of `records`. No global journal is opened.
    fn write_run(state_root: &Path, plan: &AccountingPlan, records: &[JournalRecord]) -> std::path::PathBuf {
        let dir = check_cmd::accounting_base(state_root).join(&plan.run_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plan.json"), serde_json::to_vec(plan).unwrap()).unwrap();
        let body: String = records.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
        std::fs::write(dir.join("journal.jsonl"), body).unwrap();
        dir
    }

    fn started(test: &str) -> JournalRecord {
        JournalRecord::Observed {
            lane: 0,
            stream: 1,
            origin: check_cmd::StreamOrigin::Binary { resolution: None, unit: unit(), one_test: None },
            event: ObsEvent::Started { name: test.into() },
        }
    }

    fn finished(test: &str) -> JournalRecord {
        JournalRecord::Observed {
            lane: 0,
            stream: 1,
            origin: check_cmd::StreamOrigin::Binary { resolution: None, unit: unit(), one_test: None },
            event: ObsEvent::Finished { name: test.into(), result: test_runner::TestResult::Ok },
        }
    }

    fn deadline() -> JournalRecord {
        JournalRecord::Terminated(Termination {
            scope: TerminationScope::Run,
            lane: None,
            stream: None,
            cause: TerminationCause::PhaseDeadline,
            test: None,
            charged: None,
        })
    }

    fn killed_run(id: &str, exe: &Path, hash: &str, project_root: &Path) -> (AccountingPlan, Vec<JournalRecord>) {
        let plan = AccountingPlan {
            run_id: id.into(),
            complete: true,
            lanes: vec![lane_record(exe, hash)],
            invocation: Some(Invocation { cwd: "/x".into(), project_root: project_root.to_string_lossy().into_owned() }),
            ..AccountingPlan::default()
        };
        (plan, vec![started("a"), deadline()])
    }

    fn accounting_entries(state_root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(check_cmd::accounting_base(state_root))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Validation case: `--list` prints the report from the persisted evidence
    /// and executes nothing - it needs no checkout (the project root here does
    /// not exist), writes no record of its own and leaves the source's files
    /// as they were.
    #[test]
    fn list_executes_nothing_and_writes_nothing() {
        let state = crate::test_scratch::scratch("replay", "list_only");
        let (plan, records) = killed_run("1700000000000-5", Path::new("/t/suite-1"), "h", Path::new("/nonexistent"));
        let dir = write_run(&state, &plan, &records);
        let (plan_before, journal_before) =
            (std::fs::read(dir.join("plan.json")).unwrap(), std::fs::read(dir.join("journal.jsonl")).unwrap());

        from_run(Path::new("/nonexistent/elsewhere"), &state, &plan.run_id, true).unwrap();

        assert_eq!(accounting_entries(&state), vec!["1700000000000-5".to_owned()], "no continuation record");
        assert_eq!(std::fs::read(dir.join("plan.json")).unwrap(), plan_before);
        assert_eq!(std::fs::read(dir.join("journal.jsonl")).unwrap(), journal_before);
    }

    /// Validation case: an empty selection never launches. A source with
    /// nothing interrupted or unobserved - everything passed - replays
    /// nothing: no build, no process, no record.
    #[test]
    fn an_empty_selection_never_launches() {
        let state = crate::test_scratch::scratch("replay", "empty_selection");
        let (plan, _) = killed_run("1700000000000-6", Path::new("/t/suite-1"), "h", Path::new("/nonexistent"));
        let all_passed = vec![started("a"), finished("a"), started("b"), finished("b")];
        write_run(&state, &plan, &all_passed);
        // A project root that does not exist and an executable that does not
        // exist: any attempt to build or launch would fail loudly.
        from_run(Path::new("/nonexistent"), &state, &plan.run_id, false).unwrap();
        assert_eq!(accounting_entries(&state), vec!["1700000000000-6".to_owned()]);
    }

    /// Validation case: a pruned source run is a clear error from the command
    /// itself, listing or running.
    #[test]
    fn a_pruned_source_run_is_a_clear_error() {
        let state = crate::test_scratch::scratch("replay", "pruned");
        for list in [true, false] {
            let err = from_run(Path::new("/p"), &state, "1700000000000-9", list).unwrap_err().to_string();
            assert!(err.contains("is gone"), "{err}");
        }
    }

    /// A replay against another checkout is refused before anything builds:
    /// the recorded executables and environment belong to the checkout the
    /// run was made in.
    #[test]
    fn a_replay_in_another_checkout_is_refused() {
        let state = crate::test_scratch::scratch("replay", "other_checkout");
        let (plan, records) = killed_run("1700000000000-7", Path::new("/t/suite-1"), "h", Path::new("/the/original"));
        write_run(&state, &plan, &records);
        let err = from_run(Path::new("/somewhere/else"), &state, &plan.run_id, false).unwrap_err().to_string();
        assert!(err.contains("was recorded against /the/original"), "{err}");
        assert_eq!(accounting_entries(&state).len(), 1, "nothing was recorded");
    }

    /// Validation case: `--from-run` refuses on artifact drift. The rebuild of
    /// the recorded shape is held to the recorded content hash, and a
    /// difference refuses the replay with what differs - while the same
    /// artifact verifies.
    #[test]
    fn replay_refuses_on_artifact_drift() {
        let dir = crate::test_scratch::scratch("replay", "drift");
        let exe = dir.join("suite-1");
        std::fs::write(&exe, b"the planned binary").unwrap();
        let planned = test_runner::hash_file(&exe).unwrap();
        let sel = LaneSelection {
            record: lane_record(&exe, &planned),
            groups: vec![ReplayGroup { resolution: None, unit: unit(), tests: vec!["a".into()] }],
            allocs: check_cmd::ThreadAllocations::new(),
        };
        let rebuild = |_: usize| {
            Ok(Some((vec![binary_at(&exe)], check_cmd::BuildRuntimeIndex::default())))
        };
        verify_with("1-2", &sel, rebuild, &[]).unwrap();

        std::fs::write(&exe, b"edited source, rebuilt").unwrap();
        let err = verify_with("1-2", &sel, rebuild, &[]).unwrap_err().to_string();
        assert!(err.contains("run 1-2 is not replayed"), "{err}");
        assert!(err.contains("rebuilt with different content"), "{err}");
        assert!(err.contains("new experiment"), "{err}");

        // A build that fails is its own refusal, not a pass.
        let broken = |_: usize| Ok(None);
        assert!(verify_with("1-2", &sel, broken, &[]).unwrap_err().to_string().contains("could not be rebuilt"));
    }

    fn selection(lane: usize, label: &str) -> LaneSelection {
        let mut record = lane_record(Path::new("/t/suite-1"), "h");
        record.lane = lane;
        record.label = label.into();
        LaneSelection {
            record,
            groups: vec![ReplayGroup { resolution: None, unit: unit(), tests: vec!["a".into()] }],
            allocs: check_cmd::ThreadAllocations::new(),
        }
    }

    /// A parallel lane's replay launches each binary with the thread count the
    /// original executor journaled for it, not one derived from the remaining
    /// tests: two unresolved tests of a binary that ran with one thread replay
    /// with one, and a binary with no journaled allocation refuses.
    #[test]
    fn a_parallel_replay_uses_the_journaled_thread_allocation() {
        let mut sel = selection(2, "par");
        sel.groups[0].tests = vec!["a".into(), "b".into()];
        let replay = sel.record.replay.as_mut().unwrap();
        replay.model = ReplayModel::Parallel;
        replay.test_threads = None;
        replay.parallel_budget = Some(8);
        // What the executor journals, and what a run's journal hands back.
        let journal = [JournalRecord::ThreadAllocation { lane: 2, resolution: None, unit: unit(), threads: 1 }];
        sel.allocs = check_cmd::thread_allocations(&journal);

        let launches = parallel_launches(&sel).unwrap();
        assert_eq!(launches.len(), 1);
        let (_, args, slots) = &launches[0];
        assert_eq!(*slots, 1, "the original allocation, not the two remaining tests");
        assert!(args.contains(&"--test-threads=1".to_owned()), "{args:?}");

        sel.allocs.clear();
        let err = parallel_launches(&sel).unwrap_err().to_string();
        assert!(err.contains("no thread allocation"), "{err}");
    }

    /// The engine lane's replay compares the launch fingerprint the recipe
    /// recorded with the one a fresh preparation resolved, through the real
    /// `engine_drift`: equal is clean, any difference refuses. (Where the
    /// fingerprint comes from - the engine's real discovery in preparation -
    /// is covered by the nextest lane's real-engine test.)
    #[test]
    fn the_engine_launch_fingerprint_is_compared_in_replay() {
        let mut sel = selection(0, "engine");
        sel.record.artifacts.clear();
        let replay = sel.record.replay.as_mut().unwrap();
        replay.model = ReplayModel::Nextest;
        replay.engine_launch = vec!["cargo-env {A}".into(), "runner None".into()];
        let fresh = |launch: &[&str]| {
            let mut p = check_cmd::PreparedLane::bare(check_cmd::LaneEnv::default());
            p.engine_launch = launch.iter().map(|s| (*s).to_owned()).collect();
            p
        };
        assert!(engine_drift(&sel, &fresh(&["cargo-env {A}", "runner None"]), &[]).unwrap().is_empty());
        let drift = engine_drift(&sel, &fresh(&["cargo-env {A, B}", "runner None"]), &[]).unwrap();
        assert!(drift.iter().any(|d| d.contains("launch environment")), "{drift:?}");
        // And the refusal reaches the user as a drift refusal.
        let err = refuse_drift("1-2", &sel, &drift).unwrap_err().to_string();
        assert!(err.contains("run 1-2 is not replayed") && err.contains("launch environment"), "{err}");
    }

    const GREEN: LaneResult = LaneResult { passed: true, timed_out: false };

    /// Validation case: a later lane's build can replace what an earlier
    /// lane's verification saw (two lanes share a support executable's path),
    /// so each lane is built and verified again IMMEDIATELY before it
    /// launches, after the preflight that checked every lane. The order is the
    /// whole point: verify-all-then-execute-all let the second lane's build
    /// land between the first lane's verification and its launch.
    #[test]
    fn each_lane_is_armed_again_immediately_before_it_launches() {
        let selections = vec![selection(0, "first"), selection(1, "second")];
        let log = std::cell::RefCell::new(Vec::<String>::new());
        let arm = |s: &LaneSelection| {
            log.borrow_mut().push(format!("arm {}", s.record.label));
            Ok::<(), DevError>(())
        };
        let run = |s: &LaneSelection, (): (), _: &LaneTap| {
            log.borrow_mut().push(format!("run {}", s.record.label));
            Ok(GREEN)
        };
        for s in &selections {
            arm(s).unwrap();
        }
        assert!(run_selections(&selections, arm, run).unwrap());
        assert_eq!(
            *log.borrow(),
            ["arm first", "arm second", "arm first", "run first", "arm second", "run second"],
            "preflight every lane, then arm-then-run each in turn"
        );
    }

    /// A lane that no longer verifies when its turn comes refuses the rest of
    /// the replay: the lane before it ran, it and the ones after never do.
    #[test]
    fn a_lane_that_drifts_after_the_preflight_is_not_launched() {
        let selections = vec![selection(0, "first"), selection(1, "second"), selection(2, "third")];
        let ran = std::cell::RefCell::new(Vec::<String>::new());
        let arms = std::cell::Cell::new(0);
        let arm = |s: &LaneSelection| {
            arms.set(arms.get() + 1);
            // The second lane's launch-time rebuild finds another lane's
            // executable in its place.
            if s.record.label == "second" {
                Err(DevError::Config("second no longer builds what was recorded".into()))
            } else {
                Ok(())
            }
        };
        let run = |s: &LaneSelection, (): (), _: &LaneTap| {
            ran.borrow_mut().push(s.record.label.clone());
            Ok(GREEN)
        };
        let err = run_selections(&selections, arm, run).unwrap_err().to_string();
        assert!(err.contains("no longer builds"), "{err}");
        assert_eq!(*ran.borrow(), ["first"]);
        assert_eq!(arms.get(), 2, "the third lane was never even built");
    }

    /// Validation case: a per-test timeout stops the replay - including a
    /// nextest lane's, whose structured timeout used to be dropped (always
    /// `timed_out: false`), so the replay went on into later lanes against
    /// whatever state the timeout left.
    #[test]
    fn a_timeout_in_a_lane_stops_the_replay() {
        let selections = vec![selection(0, "engine"), selection(1, "after")];
        let ran = std::cell::RefCell::new(Vec::<String>::new());
        let run = |s: &LaneSelection, (): (), _: &LaneTap| {
            ran.borrow_mut().push(s.record.label.clone());
            // The engine's disposition, carried through the conversion the
            // nextest lane uses.
            Ok(LaneResult::from(check_cmd::EngineRun { passed: false, timed_out: true }))
        };
        let err = run_selections(&selections, |_| Ok(()), run).unwrap_err();
        assert!(matches!(err, DevError::Verify(_)), "{err}");
        assert!(err.to_string().contains("exceeded its time budget"), "{err}");
        assert_eq!(*ran.borrow(), ["engine"], "the later lane is not launched against the timeout's state");
    }

    /// A failure is preserved, not swallowed: the replay runs on (a failure is
    /// not a stop) but reports that a lane failed, which is what stops it
    /// being declared complete.
    #[test]
    fn a_failed_lane_is_reported_to_the_caller() {
        let selections = vec![selection(0, "bad"), selection(1, "good")];
        let run = |s: &LaneSelection, (): (), _: &LaneTap| {
            Ok(LaneResult { passed: s.record.label == "good", timed_out: false })
        };
        assert!(!run_selections(&selections, |_| Ok(()), run).unwrap());
    }

    /// Validation case: a continuation is `diagnostic_completed` only when the
    /// executors reported no failure AND its own record is whole and green. A
    /// harness that passes every test and then exits nonzero in teardown
    /// counted as complete when completion was "every execution passed, no
    /// anomaly"; so did a continuation whose journal was never closed.
    #[test]
    fn completion_needs_clean_executors_and_a_whole_green_record() {
        let mut record = lane_record(Path::new("/t/suite-1"), "h");
        record.executions = vec![pair("a")];
        let plan = AccountingPlan { run_id: "1-1".into(), complete: true, lanes: vec![record], ..AccountingPlan::default() };
        let origin = || check_cmd::StreamOrigin::Binary { resolution: None, unit: unit(), one_test: None };
        let obs = |event: ObsEvent| JournalRecord::Observed { lane: 0, stream: 1, origin: origin(), event };
        let mut records = vec![
            obs(ObsEvent::SuiteStarted { test_count: 1 }),
            started("a"),
            finished("a"),
            obs(ObsEvent::SuiteFinished { passed: 1, failed: 0, ignored: 0 }),
            obs(ObsEvent::StreamEnded { end: test_runner::StreamEnd::Eof }),
        ];
        // `started`/`finished` above are stream 1, lane 0, the unit's origin.
        let recon = |records: &[JournalRecord], closed: bool| check_cmd::reconcile(&plan, records, closed, Vec::new());

        records.push(JournalRecord::Closed);
        let green = recon(&records, true);
        assert!(completed(&green, true), "{:?}", green.anomalies);
        assert!(!completed(&green, false), "an executor that reported a failure never completes");
        assert!(!completed(&recon(&records, false), true), "a journal that was not closed is not whole");

        let mut crashed = records.clone();
        crashed.insert(5, JournalRecord::ProcessExited { lane: 0, stream: 1, code: Some(101), signal: None });
        let r = recon(&crashed, true);
        assert_eq!(r.count(check_cmd::Outcome::Passed), 1, "every test passed");
        assert!(!completed(&r, true), "the process that ran them did not exit cleanly");
    }

    /// A lane whose recipe is missing refuses the whole replay, naming it.
    #[test]
    fn a_lane_without_a_recipe_refuses_the_replay() {
        let mut record = lane_record(Path::new("/t/suite-1"), "h");
        record.replay = None;
        record.unavailable = Some("no inventory".into());
        let plan = AccountingPlan { run_id: "1-1".into(), lanes: vec![record], ..AccountingPlan::default() };
        let cand = Candidate {
            id: ExecutionId { lane: 0, attempt: 1, pair: pair("a") },
            outcome: check_cmd::Outcome::Unobserved,
            detail: None,
        };
        let err = select_lanes(&plan, &[cand], &check_cmd::ThreadAllocations::new()).err().unwrap().to_string();
        assert!(err.contains("default: the plan holds no replay recipe"), "{err}");
        assert!(err.contains("run 1-1 cannot be replayed"), "{err}");
    }

    /// The continuation's plan expects exactly the selected executions, under
    /// the source's lane indices, names its source and the original execution
    /// ids, and keeps the recipe so it can itself be continued.
    #[test]
    fn the_continuation_plan_names_its_source_and_expects_only_the_selection() {
        let record = lane_record(Path::new("/t/suite-1"), "h");
        let cand = Candidate {
            id: ExecutionId { lane: 0, attempt: 1, pair: pair("b") },
            outcome: check_cmd::Outcome::Unobserved,
            detail: None,
        };
        let sel = LaneSelection {
            record,
            groups: vec![ReplayGroup { resolution: None, unit: unit(), tests: vec!["b".into()] }],
            allocs: check_cmd::ThreadAllocations::new(),
        };
        let plan = continuation_plan("1700000000000-5", std::slice::from_ref(&cand), &[sel], Path::new("/r"));
        assert_ne!(plan.run_id, "1700000000000-5");
        assert_eq!(plan.lanes.len(), 1);
        assert_eq!(plan.lanes[0].lane, 0);
        assert_eq!(plan.lanes[0].executions, vec![pair("b")], "a was not selected");
        assert!(plan.lanes[0].replay.is_some());
        let of = plan.continuation.unwrap();
        assert_eq!(of.source_run_id, "1700000000000-5");
        assert_eq!(of.selected, vec![cand.id]);
        assert!(!plan.certifying);
    }
}
