//! `brokkr test <NAME>` - named-test cargo runner.
//!
//! Runs the tests matching `<NAME>` with the host/check features and
//! `--include-ignored --nocapture --test-threads=1`. Per sweep: one prebuild,
//! then every prebuilt harness lists itself directly (libtest's JSON
//! discovery, see `focused`), and only the harnesses holding a match run -
//! each one directly, as its own process, under cargo's reconstructed launch
//! envelope (`check_cmd::DirectRuntime`). A failing or crashing harness never
//! hides the ones after it, and one with no match costs a listing, not a cargo
//! invocation. `harness = false` targets are excluded and named. A doc-only
//! sweep is still one `cargo test --doc`, since ordinary discovery cannot see
//! doctests. Defaults to release;
//! `--debug` switches to the dev profile, `--release` forces it back, and
//! when neither is passed the `[test] debug` toml field decides. Streams
//! the test's own
//! stdout/stderr live (filtering out cargo/test-harness framing noise), then
//! prints a `[test]` PASS/FAIL footer per sweep with wall time. The cargo and
//! harness invocations go to the run log; the console shows one only when it
//! failed (beside its diagnostics, or after a failing harness's footer, as the
//! reproduction line). No `sweep:` header lines: a line that belongs to one
//! sweep names it when several run. Under `-N`, the once-only lines print for
//! run 1 only - repeats collapse to their footer line.
//!
//! Feature selection follows the same priority ladder as
//! `brokkr check`'s test phase, with two intentional differences:
//! profile libtest filters (`only` / `skip` / `tests`) are dropped
//! (the user's `<NAME>` is the filter), and CLI `--features` is not
//! accepted. Profile-declared `env` vars *are* propagated, so a
//! profile that gates platform tests behind `BROKKR_TEST_PLATFORM=1`
//! still works under `brokkr test`.
//!
//! Every test gets a 20s hard cap; exceeding it fails the run and stops it,
//! between `-N` iterations included. `--timeout <SECS>` (1-280) is the only
//! exception anywhere in brokkr, and it also makes the *attribution* exact:
//! discovery resolves `<NAME>` to one (harness, full test name), and that one
//! binary is run with libtest `--exact`, making the process the unit of one
//! test. Resolving the full name matters - `--exact` on the user's substring
//! would match nothing and silently run zero tests. What it bounds is that
//! process from spawn to exit: its static constructors, the test, teardown.
//! The build and every other harness's listing fall outside it. Because a
//! higher ceiling only makes sense for one isolated test, it is gated: a
//! `<NAME>` matching more than one test in a sweep - two harnesses holding the
//! same name count twice - is a hard error before anything runs.
//!
//! A `<NAME>` matching every discovered test of the package is refused (see
//! `matches_whole_suite`) - a whole-suite run is `brokkr check`'s job.
//!
//! `--sweep <LABEL>` narrows the resolved profile's sweep set to the one
//! matching sweep (e.g. `--sweep all`), instead of running every sweep the
//! profile lists. An unknown label is a hard error listing the available
//! ones. Combined with `--timeout`, this is the usual way to iterate on a
//! single hung test without paying for the other sweeps' rebuilds.

pub(crate) mod focused;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::build;
use crate::cargo_filter;
use crate::check_cmd::{self, LaneEntry};
use crate::config::{self, DevConfig};
use crate::error::DevError;
use crate::output;
use crate::profile::ResolvedSweep;
use crate::project::Project;
use crate::rustflags;
use crate::test_runner::{self, LibtestOutcome};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    BuildFailed,
    /// Cargo ran but no test matched the name. Could mean "wrong name" or
    /// "feature-gated out of this sweep" - only distinguishable by checking
    /// the other sweeps' outcomes. The aggregator in `run` decides how to
    /// exit based on whether any sweep saw a Pass.
    NoMatch,
}

/// What a [`Failure`] names: a test libtest gave a failing verdict, or
/// something about the harness itself (a crash, a stream that stopped
/// reporting, a hang, a disagreement with discovery). Only a `Test` failure
/// has a name a rerun can select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureKind {
    Test,
    Harness,
}

/// One failure a run reported.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Failure {
    kind: FailureKind,
    /// The test harness it came from (`test:cli_sort`, `lib:pkg`), or empty
    /// when the run was one invocation over the whole package (`--timeout`, a
    /// doc-only sweep).
    target: String,
    /// What failed: a test name, or a harness-level failure (a crash, a stream
    /// that stopped reporting, a hang).
    what: String,
    loc: Option<String>,
    msg: Option<String>,
}

impl Failure {
    /// What the `-N` summary groups on: the harness, what failed, and where -
    /// the panic location, falling back to the message. Location first because
    /// a flaky assertion's message often carries the varying value
    /// (`rolled 0`, `rolled 2`) while its location does not move.
    fn key(&self) -> String {
        let site = self.loc.as_deref().or(self.msg.as_deref()).unwrap_or("");
        format!("{:?}|{}|{}|{site}", self.kind, self.target, self.what)
    }

    /// The footer form: the harness is already in the run's tag.
    fn describe(&self) -> String {
        match (&self.msg, &self.loc) {
            (Some(m), Some(l)) => format!("{}: {m} @ {l}", self.what),
            (Some(m), None) => format!("{}: {m}", self.what),
            (None, Some(l)) => format!("{} @ {l}", self.what),
            (None, None) => self.what.clone(),
        }
    }

    /// The summary form, which spans harnesses and so names the one each
    /// failure came from.
    fn describe_qualified(&self) -> String {
        if self.target.is_empty() {
            self.describe()
        } else {
            format!("[{}] {}", self.target, self.describe())
        }
    }
}

/// The identity of a whole failure set: every failure's key, sorted. Two runs
/// failing the same tests the same way share it whatever order their harnesses
/// reported in; a run failing one test more does not.
fn failure_set_key(failures: &[Failure]) -> String {
    let mut keys: Vec<String> = failures.iter().map(Failure::key).collect();
    keys.sort();
    keys.dedup();
    keys.join("\n")
}

/// One run's outcome plus every failure it reported - one sweep's iteration,
/// across all of that sweep's test harnesses.
struct RunReport {
    /// The run blew a time budget rather than merely failing. Stops the command:
    /// see the `-N` loop in [`run`].
    timed_out: bool,
    outcome: Outcome,
    /// Empty for a PASS/SKIP/BUILD FAILED, and for a FAIL nothing could be
    /// attributed to (the summary then says `unknown failure`).
    failures: Vec<Failure>,
}

impl RunReport {
    fn bare(outcome: Outcome) -> Self {
        Self {
            outcome,
            timed_out: false,
            failures: Vec::new(),
        }
    }

    /// A run that blew a time budget. Fails, and stops the command.
    fn timed_out(target: &str, msg: String) -> Self {
        Self {
            outcome: Outcome::Fail,
            timed_out: true,
            failures: vec![Failure {
                kind: FailureKind::Harness,
                target: target.to_owned(),
                what: msg,
                loc: None,
                msg: None,
            }],
        }
    }

    /// Fold one iteration's per-harness reports into the sweep's. Any FAIL
    /// fails it, then any BUILD FAILED; it passes when some harness ran the
    /// test, and is a SKIP only when no harness matched anything - a name
    /// matching nothing in most harnesses is the normal case once each runs
    /// on its own.
    fn merge(parts: Vec<RunReport>) -> Self {
        let has = |o: Outcome| parts.iter().any(|p| p.outcome == o);
        let outcome = if has(Outcome::Fail) {
            Outcome::Fail
        } else if has(Outcome::BuildFailed) {
            Outcome::BuildFailed
        } else if has(Outcome::Pass) {
            Outcome::Pass
        } else {
            Outcome::NoMatch
        };
        Self {
            outcome,
            timed_out: parts.iter().any(|p| p.timed_out),
            failures: parts.into_iter().flat_map(|p| p.failures).collect(),
        }
    }
}

/// Shared across the `-N` repeat loop: failure signatures (panic
/// location, falling back to message) whose full streamed block has
/// already been shown once. Later runs failing with a seen signature
/// have their block suppressed - the FAIL footer alone carries the
/// per-run message.
#[derive(Default)]
struct RepeatState {
    seen_failures: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Reproduction lines already printed, so a harness failing on every
    /// iteration names its command once.
    seen_hints: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl RepeatState {
    /// Record a reproduction line; true if this is its first occurrence.
    fn first_hint(&self, line: &str) -> bool {
        self.seen_hints
            .lock()
            .map(|mut s| s.insert(line.to_owned()))
            .unwrap_or(true)
    }

    /// Record a signature; true if this is its first occurrence.
    fn first_sighting(&self, sig: &str) -> bool {
        self.seen_failures
            .lock()
            .map(|mut s| s.insert(sig.to_owned()))
            .unwrap_or(true)
    }
}

/// Display destination for the streamed test output: live (run 1) or
/// buffered (repeats), where the buffer is flushed - or dropped for an
/// already-seen failure - once the outcome is known.
type LineSink = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Run `<NAME>` in every applicable sweep.
///
/// Two things bound the command beyond the per-test cap, both shared with
/// `check`: the phase watchdog (`check`'s 15-minute `test` ceiling, restarted
/// for every sweep and every `-N` iteration - there is no whole-run ceiling,
/// since a repeat run is bounded per iteration) and a `SigtermGuard`, so a
/// graceful `brokkr kill` ends the command as `Interrupted` (exit 130). A fired
/// watchdog exits 124 whatever the kill surfaced as.
#[allow(clippy::too_many_arguments)]
pub fn run(
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

/// The whole run's shape, printed up front: the sweeps the run is eligible to
/// go through, so a PASS in the first one is visibly not the end of the
/// command. Rendered from the run's selection.
///
/// It is also the only place a scope-excluded or deduped sweep is named -
/// those print no SKIP line of their own - so it keeps the reasons apart: a
/// sweep whose `packages` list leaves the package out, one that excludes it by
/// `test_exclude_packages`, and one that would run exactly what an earlier
/// sweep runs.
fn sweep_plan_line(sweeps: &[ResolvedSweep], selection: &check_cmd::PhaseSelection, pkg: &str) -> String {
    let mut eligible: Vec<&str> = Vec::new();
    let mut not_listed: Vec<&str> = Vec::new();
    let mut excluded: Vec<&str> = Vec::new();
    let mut deduped: Vec<String> = Vec::new();
    for (s, entry) in sweeps.iter().zip(selection.entries()) {
        match entry {
            LaneEntry::Attempt(_) => eligible.push(&s.label),
            LaneEntry::Excluded(e) => match exclusion_rule(e.notes()) {
                Some(check_cmd::AdmissionRule::TestExcluded) => excluded.push(&s.label),
                _ => not_listed.push(&s.label),
            },
            LaneEntry::Deduped(d) => {
                let onto = sweeps.get(d.onto()).map_or("?", |o| o.label.as_str());
                deduped.push(format!("{} (as {onto})", s.label));
            }
            LaneEntry::Disabled(_) | LaneEntry::NotApplicable(_) => {}
        }
    }
    let mut line = format!(
        "[test]    {} for {pkg}: {}",
        output::count(eligible.len(), "sweep"),
        eligible.join(", ")
    );
    let mut out = Vec::new();
    if !not_listed.is_empty() {
        out.push(format!("not this package: {}", not_listed.join(", ")));
    }
    if !excluded.is_empty() {
        out.push(format!("excluded by test_exclude_packages: {}", excluded.join(", ")));
    }
    if !deduped.is_empty() {
        out.push(format!("deduped: {}", deduped.join(", ")));
    }
    if !out.is_empty() {
        line.push_str(&format!(" ({})", out.join("; ")));
    }
    line
}

/// The rule that excluded `brokkr test`'s one package from a sweep. One
/// package, so one note.
fn exclusion_rule(notes: &[check_cmd::AdmissionNote]) -> Option<check_cmd::AdmissionRule> {
    notes.first().map(|n| n.rule)
}

/// The lone-sweep SKIP line's wording for an exclusion.
fn exclusion_describe(notes: &[check_cmd::AdmissionNote]) -> &'static str {
    match exclusion_rule(notes) {
        Some(check_cmd::AdmissionRule::TestExcluded) => "package excluded from this sweep (test_exclude_packages)",
        _ => "package not in this sweep's packages list",
    }
}

/// What one sweep came to before any test ran.
enum SweepOutcome<'s> {
    /// Built, discovered and ready to run.
    Ready(Box<ReadySweep<'s>>),
    /// Nothing to run: skipped, or its build failed.
    Done(RunReport),
}

/// One sweep, built and discovered: everything its runs share, owned, so
/// every sweep can be prepared before the first one runs.
struct ReadySweep<'s> {
    sweep: &'s ResolvedSweep,
    /// The sweep's attempt in the run's selection: its package selection.
    attempt: check_cmd::Attempt,
    debug: bool,
    env: Vec<(String, String)>,
    allow_args: Vec<String>,
    /// The harnesses holding a match, run directly; `None` for a doc-only
    /// sweep's single `cargo test --doc`.
    focused: Option<FocusedPlan>,
    /// The resolved full name `--timeout` runs with `--exact`.
    exact: Option<String>,
    support_fingerprint: Vec<String>,
    runtime_fingerprint: Vec<String>,
}

/// Build one attempted sweep's support binaries and test harnesses and
/// discover the harnesses holding a match. Only an attempt reaches here: a
/// sweep the selection excludes or dedupes builds nothing.
fn prepare_sweep<'s>(
    sweep: &'s ResolvedSweep,
    attempt: &check_cmd::Attempt,
    cx: &SweepContext<'_>,
) -> Result<SweepOutcome<'s>, DevError> {
    let (project_root, name) = (cx.project_root, cx.name);
    let (profile_override, timeout) = (cx.profile_override, cx.timeout);
    // The sweep's pre-build, enumeration and first run share one phase clock,
    // as a `check` test phase's builds and runs do.
    check_cmd::enter_phase("test");

    let debug = resolve_debug(profile_override, cx.test_cfg, sweep.profile);
    let profile_dir = if debug { "debug" } else { "release" };

    let (env_owned, allow_args) =
        sweep_env(sweep, cx.project, project_root, cx.target_dir, profile_dir, cx.allow_flags);
    let env_refs: Vec<(&str, &str)> =
        env_owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    // Pre-build any binary packages declared by this sweep. Skipped
    // when build_packages is empty (fallback path). Failure here
    // short-circuits the sweep with a BuildFailed outcome so the
    // aggregator marks it as failed.
    let mut support: Vec<check_cmd::SupportArtifact> = Vec::new();
    for build_pkg in &sweep.build_packages {
        match run_pre_build(project_root, sweep, build_pkg, &env_refs, &allow_args, debug)? {
            Some(artifacts) => support.extend(artifacts),
            None => return Ok(SweepOutcome::Done(RunReport::bare(Outcome::BuildFailed))),
        }
    }

    // A doc-only sweep is one rustdoc run, unchanged: ordinary discovery
    // cannot see doctests. Every other sweep prebuilds, has each harness
    // list itself, and runs only the harnesses holding a match - see
    // `plan_focused`.
    let shape = BuildShape { sweep, allow_args: &allow_args, selection: attempt.selection(), jobs: cx.jobs, debug };
    let (focused, runtime_fingerprint) = if sweep.doc_only {
        if timeout.is_some() {
            return Err(doc_only_timeout_refusal(sweep));
        }
        (None, Vec::new())
    } else {
        // Lines that belong to one sweep say which, when several run.
        let label = cx.multi.then_some(sweep.label.as_str());
        let Some((binaries, index)) = prebuild_targets(&shape, &env_refs, project_root, false, label)? else {
            return Ok(SweepOutcome::Done(RunReport::bare(Outcome::BuildFailed)));
        };
        let fingerprint = index.fingerprint()?;
        let plan = plan_focused(&shape, name, timeout.is_some(), &binaries, index, &env_refs, project_root, label)?;
        (Some(plan), fingerprint)
    };
    // Hashed after every build the sweep did: the pre-builds can be re-uplifted
    // by the test build, and the file the tests read is whichever landed last.
    let support_fingerprint = check_cmd::support_fingerprint(&support)?;
    let exact = focused.as_ref().and_then(|f| f.exact.clone());
    Ok(SweepOutcome::Ready(Box::new(ReadySweep {
        sweep,
        attempt: attempt.clone(),
        debug,
        env: env_owned,
        allow_args,
        focused,
        exact,
        support_fingerprint,
        runtime_fingerprint,
    })))
}

/// The run-wide inputs a sweep is prepared and run from.
struct SweepContext<'a> {
    project: Project,
    project_root: &'a Path,
    state_root: &'a Path,
    test_cfg: Option<&'a crate::config::TestConfig>,
    target_dir: &'a Path,
    allow_flags: &'a [String],
    name: &'a str,
    jobs: Option<u32>,
    multi: bool,
    /// `--debug`/`--release`, which a sweep's own profile pin outranks.
    profile_override: Option<bool>,
    /// `--timeout`, the one test's raised cap.
    timeout: Option<u64>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_sweeps(
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
    let ceiling = timeout.map_or(test_runner::TEST_TIMEOUT, Duration::from_secs);
    let sweeps = resolve_sweeps(dev_config.test.as_ref(), &dev_config.check, sweep_filter)?;

    let (pkg, source) = resolve_package(package, dev_config, project)?;
    // Which sweeps the run is eligible to attempt: the resolved package always
    // overrides each sweep's own selection, sweeps whose rules leave it out
    // are excluded, and sweeps that would execute exactly what an earlier one
    // does are deduped. After `--sweep`, so a lane-qualified label reaches a
    // sweep the dedupe would otherwise fold away.
    let test_cfg = dev_config.test.as_ref();
    let debug_of = |s: &ResolvedSweep| resolve_debug(profile_override, test_cfg, s.profile);
    let selection = check_cmd::PhaseSelection::for_brokkr_test(&sweeps, &pkg, source, &debug_of)?;
    // A deduped sweep is not a sweep this run goes through.
    let multi = selection.entries().iter().filter(|e| !matches!(e, LaneEntry::Deduped(_))).count() > 1;

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
    for (sweep, entry) in sweeps.iter().zip(selection.entries()) {
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
                    println!("[test]    SKIP {pkg}::{name} - {}", exclusion_describe(e.notes()));
                }
                reports.push(RunReport::bare(Outcome::NoMatch));
                continue;
            }
            // Runs what an earlier sweep runs: named on the plan line, not run.
            LaneEntry::Deduped(_) | LaneEntry::Disabled(_) | LaneEntry::NotApplicable(_) => continue,
        };
        match prepare_sweep(sweep, attempt, &cx)? {
            SweepOutcome::Ready(r) => ready.push(*r),
            SweepOutcome::Done(report) => reports.push(report),
        }
    }

    let inventory = TestInventory::open(&ready, &cx, repeat);
    let mut notes: Vec<NoMatchNote> = Vec::new();
    let ran = run_ready(&ready, &cx, repeat, ceiling, inventory.as_ref(), &mut reports, &mut notes);

    // The journal says the run is over, however it ended; then what it left
    // unresolved is read back from disk.
    let continuation = inventory.map(TestInventory::close).and_then(|i| i.continuation());
    if let Some(c) = &continuation {
        output::error(&check_cmd::render_continuation(c).join("\n"));
    }
    ran?;

    // A sweep that found no match is news only beside a sweep that did: then
    // "feature-gated out of this sweep" is real information. When none matched,
    // the closing error says it once.
    if reports.iter().any(|r| matches!(r.outcome, Outcome::Pass | Outcome::Fail)) {
        for note in &notes {
            println!("[test]    SKIP {pkg}::{name} [{}] - {}", note.label, note.detail);
        }
    }

    if repeat > 1 {
        for line in format_repeat_summary(&reports) {
            println!("{line}");
        }
    }

    let outcomes: Vec<Outcome> = reports.iter().map(|r| r.outcome).collect();
    aggregate_exit(&outcomes, &pkg, name, &Searched::of(&ready))
}

/// A sweep whose search found no match, held until it is known whether another
/// sweep did.
struct NoMatchNote {
    label: String,
    detail: String,
}

impl NoMatchNote {
    fn of(r: &ReadySweep<'_>) -> Self {
        let what = match &r.focused {
            Some(f) => format!("no test matched in any of {}", output::count(f.searched, "searched harness")),
            None => "no doctest matched".to_owned(),
        };
        Self {
            label: r.sweep.label.clone(),
            detail: format!("{what} (likely feature-gated out of this sweep)"),
        }
    }
}

/// What the run's sweeps searched, for the closing error when nothing matched.
#[derive(Default)]
struct Searched {
    harnesses: usize,
    /// Sweeps that ran only `cargo test --doc`: their search is not countable.
    doc_only: usize,
    labels: Vec<String>,
}

impl Searched {
    fn of(ready: &[ReadySweep<'_>]) -> Self {
        let mut s = Self::default();
        for r in ready {
            s.labels.push(r.sweep.label.clone());
            match &r.focused {
                Some(f) => s.harnesses += f.searched,
                None => s.doc_only += 1,
            }
        }
        s
    }

    /// ` (44 harnesses searched in default and strategy-features)`, or nothing
    /// when no sweep searched at all (a lone sweep scoped out of the package,
    /// which printed its own reason).
    fn describe(&self) -> String {
        if self.labels.is_empty() {
            return String::new();
        }
        let mut what = Vec::new();
        if self.harnesses > 0 || self.doc_only == 0 {
            what.push(output::count(self.harnesses, "harness"));
        }
        if self.doc_only > 0 {
            what.push("doctests".to_owned());
        }
        let mut s = format!(" ({} searched", what.join(" and "));
        if let [rest @ .., last] = self.labels.as_slice() {
            if rest.is_empty() {
                s.push_str(&format!(" in {last}"));
            } else {
                s.push_str(&format!(" in {} and {last}", rest.join(", ")));
            }
        }
        s.push(')');
        s
    }
}

/// Run every ready sweep's iterations, recording each in its lane's journal.
/// A blown time budget, or any error, ends the run; the stop is journaled
/// run-wide, so everything after it reads as unobserved for that reason.
fn run_ready(
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
        check_cmd::enter_phase("test");
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
        let env_refs: Vec<(&str, &str)> = r.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let plan = SweepPlan {
            shape: BuildShape {
                sweep: r.sweep,
                allow_args: &r.allow_args,
                selection: r.attempt.selection(),
                jobs: cx.jobs,
                debug: r.debug,
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
            check_cmd::enter_phase("test");
            let tap = inventory.map(|i| i.tap(si, n));
            if let Some(tap) = &tap {
                tap.record(check_cmd::JournalRecord::LaneStarted { lane: tap.lane() });
            }
            let result = run_iteration(&plan, n, tap.as_ref());
            if let Some(tap) = &tap {
                let passed = result.as_ref().is_ok_and(|r| r.outcome != Outcome::Fail && !r.timed_out);
                tap.record(check_cmd::JournalRecord::LaneFinished { lane: tap.lane(), passed });
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
enum Settled<'s> {
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
fn settle_sweep<'s>(
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
            tap.record(check_cmd::JournalRecord::LaneSuperseded { lane: tap.lane(), reason: reason.clone() });
        }
    }
    // On the console: it explains the second build narration that follows and
    // why this sweep has no inventory.
    output::run_msg(&format!(
        "test {}: {reason}; the sweep runs what it builds now, and its inventory is unavailable",
        prepared.sweep.label
    ));
    Ok(match prepare_sweep(prepared.sweep, &prepared.attempt, cx)? {
        SweepOutcome::Ready(f) => Settled::Rebuilt(f),
        SweepOutcome::Done(report) => Settled::Failed(report),
    })
}

/// The sweep's support builds and test build, again, held to the prepared
/// record. `Ok(None)` is a build that failed (already reported).
fn recheck_sweep(prepared: &ReadySweep<'_>, cx: &SweepContext<'_>) -> Result<Option<Vec<String>>, DevError> {
    let env_refs: Vec<(&str, &str)> = prepared.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut support: Vec<check_cmd::SupportArtifact> = Vec::new();
    for build_pkg in &prepared.sweep.build_packages {
        match run_pre_build(cx.project_root, prepared.sweep, build_pkg, &env_refs, &prepared.allow_args, prepared.debug)? {
            Some(artifacts) => support.extend(artifacts),
            None => return Ok(None),
        }
    }
    let shape = BuildShape {
        sweep: prepared.sweep,
        allow_args: &prepared.allow_args,
        selection: prepared.attempt.selection(),
        jobs: cx.jobs,
        debug: prepared.debug,
    };
    sweep_drift(prepared, |_| prebuild_targets(&shape, &env_refs, cx.project_root, true, None), &support)
}

/// Every way a rebuild differs from the prepared sweep: a harness gone or
/// rebuilt with other content, the runtime index (build-script facts, support
/// bins behind `CARGO_BIN_EXE_*`), and the `build_packages` executables the
/// tests spawn. `Ok(None)` is a build that failed. A doc-only sweep has no
/// harnesses to rebuild; its support binaries are still held to the record.
fn sweep_drift(
    prepared: &ReadySweep<'_>,
    build: impl FnMut(usize) -> Result<Option<(Vec<check_cmd::TestBinary>, check_cmd::BuildRuntimeIndex)>, DevError>,
    support: &[check_cmd::SupportArtifact],
) -> Result<Option<Vec<String>>, DevError> {
    let (artifacts, runtime, resolutions) = match &prepared.focused {
        Some(f) => (f.artifacts.clone(), Some(&prepared.runtime_fingerprint), vec![None]),
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
struct TestInventory {
    plan: check_cmd::AccountingPlan,
    journal: PathBuf,
    /// Lane index of sweep `s`'s first iteration, per ready sweep.
    first_lane: Vec<usize>,
}

impl TestInventory {
    /// Prepare the plan and open the journal, before the first test runs. A
    /// run whose plan cannot be persisted goes on without a record, and says
    /// so: its stop cannot be reported from it.
    fn open(ready: &[ReadySweep<'_>], cx: &SweepContext<'_>, repeat: u32) -> Option<Self> {
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
                Some(Self { plan, journal: paths.journal, first_lane })
            }
            Err(e) => {
                output::error(&format!("test: the run's record could not be opened: {e}"));
                None
            }
        }
    }

    /// The tap of sweep `sweep_index`'s iteration `n`.
    fn tap(&self, sweep_index: usize, n: u32) -> check_cmd::LaneTap {
        let lane = self.first_lane.get(sweep_index).copied().unwrap_or(0) + (n as usize - 1);
        check_cmd::LaneTap::new(lane)
    }

    /// The journal's closing line: the run is over.
    fn close(self) -> ClosedInventory {
        check_cmd::journal_close();
        ClosedInventory { plan: self.plan, journal: self.journal }
    }
}

/// An inventory whose journal is closed, ready to be read back.
struct ClosedInventory {
    plan: check_cmd::AccountingPlan,
    journal: PathBuf,
}

impl ClosedInventory {
    fn continuation(&self) -> Option<check_cmd::ContinuationReport> {
        let (records, recon) = check_cmd::reconcile_run(&self.plan, Some(&self.journal));
        check_cmd::build_continuation(&self.plan, &recon, &records)
    }
}

/// One (sweep, iteration)'s lane in the run's plan: the executions its
/// harnesses are expected to report and the artifacts' identity.
fn test_lane_record(r: &ReadySweep<'_>, lane: usize, label: String) -> check_cmd::LaneRecord {
    use check_cmd::{LaneKind, LaneRecord, PairId};
    let shape = check_cmd::shape_id(r.sweep);
    let Some(focused) = &r.focused else {
        // Doctests are not enumerable: the lane exists, expecting nothing.
        return LaneRecord { prepared: true, ..LaneRecord::empty(lane, label, LaneKind::DocOnly, shape) };
    };
    let mut rec = LaneRecord::empty(lane, label, LaneKind::Serial, shape.clone());
    rec.prepared = true;
    rec.include_ignored = true;
    rec.artifacts = focused.artifacts.clone();
    for run in &focused.runs {
        for name in &run.names {
            rec.executions.push(PairId {
                shape: shape.clone(),
                resolution: None,
                unit: run.unit.clone(),
                test: name.clone(),
            });
        }
    }
    rec
}

/// One sweep, built and ready: what every `-N` iteration of it shares.
struct SweepPlan<'a> {
    shape: BuildShape<'a>,
    name: &'a str,
    /// The harnesses holding a match, run directly; `None` for a doc-only
    /// sweep's single `cargo test --doc`.
    focused: Option<&'a FocusedPlan>,
    /// The resolved full name `--timeout` runs with `--exact`.
    exact: Option<String>,
    env: &'a [(&'a str, &'a str)],
    project_root: &'a Path,
    state_root: &'a Path,
    ceiling: Duration,
    /// The sweep's label when several sweeps run, so its tag says which.
    sweep_label: Option<&'a str>,
    repeat: u32,
    repeat_state: &'a RepeatState,
}

/// Run iteration `n` of a sweep and report it, printing its footers. `tap` is
/// the iteration's lane in the run's journal.
fn run_iteration(
    plan: &SweepPlan<'_>,
    n: u32,
    tap: Option<&check_cmd::LaneTap>,
) -> Result<RunReport, DevError> {
    let (pkg, name, repeat) = (plan.shape.pkg(), plan.name, plan.repeat);
    let tag = |target: Option<&str>| {
        let quals: Vec<&str> = plan.sweep_label.into_iter().chain(target).collect();
        let quals =
            if quals.is_empty() { String::new() } else { format!(" [{}]", quals.join(", ")) };
        let run = if repeat > 1 { format!(" run {n}/{repeat}") } else { String::new() };
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
        let invocation = format!("cargo {} (sweep: {}; cwd {})", arg_refs.join(" "), s.sweep.label, plan.project_root.display());
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
        ).inspect_err(|_| show_failed_invocation(plan, &invocation))?;
        show_reproduction(plan, &invocation, &report);
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
        let invocation = format!("{} {} (cwd {})", run.program, arg_refs.join(" "), run.cwd.display());
        if announce {
            output::detail(&invocation);
        }
        let test_tag = |test: &str, wall: &str| lone_test_tag(plan, &run.label, n, test, wall);
        let env: Vec<(&str, &str)> = run.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        // The harness's stream is this binary's, and under `--timeout` this
        // one test's: what lets a kill or a crash be charged to it.
        let observe = tap.map_or_else(test_runner::Observe::default, |tap| {
            let origin = check_cmd::StreamOrigin::Binary {
                resolution: None,
                unit: run.unit.clone(),
                one_test: plan.exact.clone(),
            };
            test_runner::Observe { sink: Some(tap.sink(move |_| origin.clone(), false)), ..Default::default() }
        });
        let report = run_one(
            test_runner::Launch::Direct { program: &run.program },
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
        ).inspect_err(|_| show_failed_invocation(plan, &invocation))?;
        show_reproduction(plan, &invocation, &report);
        // A blown budget stops everything, the remaining harnesses included.
        let stop = report.timed_out;
        parts.push(report);
        if stop {
            break;
        }
    }
    Ok(RunReport::merge(parts))
}

/// Print a failed run's invocation on the console: the reproduction line. A
/// passing run's goes to the run log only. Once per distinct line per command,
/// so a test failing on every `-N` iteration names its invocation once.
fn show_reproduction(plan: &SweepPlan<'_>, invocation: &str, report: &RunReport) {
    if matches!(report.outcome, Outcome::Fail | Outcome::BuildFailed) {
        show_failed_invocation(plan, invocation);
    }
}

fn show_failed_invocation(plan: &SweepPlan<'_>, invocation: &str) {
    if plan.repeat_state.first_hint(invocation) {
        output::run_msg(invocation);
    }
}

/// The footer tag a lone failing test leads with: its full name, then the same
/// qualifiers as the filter-led tag, built from the parts rather than by
/// rewriting that tag. The filter joins the wall time when it differs.
fn lone_test_tag(plan: &SweepPlan<'_>, harness: &str, n: u32, test: &str, wall: &str) -> String {
    let (pkg, name, repeat) = (plan.shape.pkg(), plan.name, plan.repeat);
    let quals: Vec<&str> = plan.sweep_label.into_iter().chain(std::iter::once(harness)).collect();
    let run_n = if repeat > 1 { format!(" run {n}/{repeat}") } else { String::new() };
    let filter = if test == name { String::new() } else { format!("; filter: {name}") };
    format!("{pkg}::{test} [{}]{run_n} ({wall}{filter})", quals.join(", "))
}

/// One sweep's focused run: the harnesses holding a match, ready to execute.
///
/// Each harness is its own process, which is load-bearing, not incidental.
/// One `cargo test -p PKG NAME` stops at the first harness with a failing test,
/// so a mutation check reading "these tests went red" would read an incomplete
/// list; and `--no-fail-fast` keeps every harness in ONE libtest stream, where
/// a harness that dies mid-test (an abort, a stack overflow - what a mutation
/// produces) leaves its suite open, the next harness's `suite/started` looks
/// forged, and the per-test clock bills the dead test to the next, healthy
/// harness. One process per harness gives each a fresh tracker, and every
/// failure belongs to the harness that printed it - the same test name in two
/// harnesses is two failures, not one.
struct FocusedPlan {
    runs: Vec<HarnessRun>,
    /// How many harnesses were listed, for the no-match note and the closing
    /// error when none matched.
    searched: usize,
    /// Under `--timeout`, the one full test name the run is invoked with.
    exact: Option<String>,
    /// Every harness the prebuild produced - searched, matched or not, and
    /// the `harness = false` ones excluded from the search - by path and
    /// content: the artifact set a rebuild before the run is held to.
    artifacts: Vec<check_cmd::PlannedArtifact>,
}

/// One harness to execute directly, with the launch envelope cargo would
/// have given it.
struct HarnessRun {
    label: String,
    program: String,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    /// How many of its discovered tests matched - what its run must report.
    matched: usize,
    /// The matched test names: the executions this harness is expected to
    /// report, which the run's inventory records.
    names: Vec<String>,
    unit: check_cmd::BinaryUnit,
}

/// Discover every eligible harness of the sweep and keep the ones holding a
/// match.
///
/// Discovery runs one harness at a time, in cargo's run order, as the cargo
/// invocations it replaces did: concurrent listing would run every harness's
/// static constructors at once, which no earlier run did. Every listing must
/// prove itself complete (see `focused::discover`) - a failed one stops the
/// run rather than reading as a harness with no match.
///
/// Also where the two breadth rules are judged, both over what discovery
/// listed (benchmarks included, since `cargo test` runs them): a `<NAME>`
/// matching every test of the package is refused, and under `--timeout` a
/// `<NAME>` matching more than one (harness, test) is.
#[allow(clippy::too_many_arguments)]
fn plan_focused(
    shape: &BuildShape<'_>,
    name: &str,
    timeout: bool,
    binaries: &[check_cmd::TestBinary],
    index: check_cmd::BuildRuntimeIndex,
    env: &[(&str, &str)],
    project_root: &Path,
    label: Option<&str>,
) -> Result<FocusedPlan, DevError> {
    // Direct execution would bypass a configured runner or misread a cross
    // build; refuse before running anything.
    check_cmd::refuse_configured_runner(project_root, env)?;
    let runtime = check_cmd::DirectRuntime::load(project_root, env, index)?;
    let split = focused::eligibility(binaries)?;
    if !split.excluded.is_empty() {
        let labels: Vec<String> = split.excluded.iter().map(|b| b.label()).collect();
        println!(
            "[test]    {}not searched: {} - `harness = false` targets are excluded from a focused \
             run, which selects libtest test names",
            sweep_prefix(label),
            labels.join(", ")
        );
    }
    let mut listed: Vec<(&check_cmd::TestBinary, Vec<String>)> = Vec::new();
    for b in &split.eligible {
        let names = focused::discover(b, &runtime, env, project_root).inspect_err(|_| {
            output::error(&format!("test {}: discovery failed for {}", shape.sweep.label, b.label()));
        })?
            .into_iter()
            .map(|l| l.name)
            .collect();
        listed.push((b, names));
    }

    let every: Vec<String> = listed.iter().flat_map(|(_, n)| n.iter().cloned()).collect();
    if matches_whole_suite(&every, name) {
        return Err(DevError::Config(format!(
            "`{name}` matches all {} tests in package `{}` (sweep '{}'): brokkr test is for one \
             test or a few, and streams every match's output live. Run the whole suite with \
             `brokkr check -p {}`, or narrow the name.",
            every.len(),
            shape.pkg(),
            shape.sweep.label,
            shape.pkg()
        )));
    }

    let searched = listed.len();
    let hits: Vec<(&check_cmd::TestBinary, Vec<String>)> = listed
        .into_iter()
        .filter_map(|(b, names)| {
            let m: Vec<String> =
                names.into_iter().filter(|t| focused::matches(t, name, false)).collect();
            (!m.is_empty()).then_some((b, m))
        })
        .collect();

    let exact = if timeout {
        let occurrences: Vec<String> = hits
            .iter()
            .flat_map(|(b, m)| m.iter().map(move |t| format!("{} {t}", b.label())))
            .collect();
        if occurrences.len() > 1 {
            return Err(DevError::Config(format!(
                "--timeout only applies to a single test, but `{name}` matches {} in sweep `{}`: \
                 {}. Narrow it to one fully-qualified test name, or drop --timeout to run them \
                 all at the 20s ceiling.",
                output::count(occurrences.len(), "test"),
                shape.sweep.label,
                occurrences.join(", ")
            )));
        }
        hits.first().and_then(|(_, m)| m.first().cloned())
    } else {
        None
    };

    let mut runs = Vec::with_capacity(hits.len());
    for (b, m) in &hits {
        let (cwd, envelope) = runtime.envelope(b, env)?;
        let cwd = if cwd.as_os_str() == "." { project_root.to_path_buf() } else { cwd };
        runs.push(HarnessRun {
            label: b.label(),
            program: b.executable.clone(),
            cwd,
            env: envelope,
            matched: m.len(),
            names: m.clone(),
            unit: check_cmd::BinaryUnit::of(b),
        });
    }
    if !runs.is_empty() {
        let labels: Vec<&str> = runs.iter().map(|r| r.label.as_str()).collect();
        println!(
            "[test]    {}{} of {} {} a match: {}",
            sweep_prefix(label),
            runs.len(),
            output::count(searched, "harness"),
            if runs.len() == 1 { "holds" } else { "hold" },
            labels.join(", ")
        );
    }
    let mut artifacts = Vec::with_capacity(binaries.len());
    for b in binaries {
        let hash = test_runner::hash_file(Path::new(&b.executable))
            .map_err(|e| DevError::Build(format!("could not hash {}: {e}", b.executable)))?;
        artifacts.push(check_cmd::PlannedArtifact {
            resolution: None,
            unit: check_cmd::BinaryUnit::of(b),
            executable: b.executable.clone(),
            hash,
        });
    }
    Ok(FocusedPlan { runs, searched, exact, artifacts })
}

/// `<label>: ` when several sweeps run (a line that belongs to one of them says
/// which), nothing when there is only one.
fn sweep_prefix(label: Option<&str>) -> String {
    label.map_or_else(String::new, |l| format!("{l}: "))
}

/// The libtest argv one directly executed harness runs with.
///
/// `--include-ignored --nocapture --test-threads=1` is what a focused run has
/// always meant. The JSON event stream is what the per-test budget is charged
/// from (records libtest states, not a `test NAME ... ` marker reconstructed
/// out of whatever the test printed); the reconstructor renders it back to
/// human text, so streamed output looks as it always did. `--exact` turns the
/// filter into an identity, which is what makes the process one test - the
/// only shape in which a per-test ceiling is a guarantee rather than a guess.
/// The caller must have resolved `filter` to a full name first; `--exact` on a
/// user's substring would match nothing.
fn direct_libtest_args(filter: &str, exact: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        filter,
        "--include-ignored",
        "--nocapture",
        "--test-threads=1",
        "-Z",
        "unstable-options",
        "--format",
        "json",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if exact {
        args.push("--exact".into());
    }
    args
}

/// `--timeout` within a doc-only sweep: refused, because the single-test
/// precondition needs doctest enumeration brokkr does not wire up.
///
/// The claim this refusal used to make - that doctests cannot be enumerated
/// because `--list` is a libtest flag and rustdoc has no listing contract - is
/// false on a current nightly: rustdoc forwards its test arguments into
/// libtest, which supports both `--list` and `--exact`, so a doc-only sweep can
/// be enumerated once merging is disabled (rustdoc's own
/// `--merge-doctests=no`, which reaches rustdoc through RUSTDOCFLAGS rather
/// than through cargo's `--` split). Until that plumbing exists, refuse - but
/// say which it is, so the next reader does not inherit a wrong reason for a
/// right refusal.
fn doc_only_timeout_refusal(sweep: &ResolvedSweep) -> DevError {
    DevError::Config(format!(
        "--timeout cannot apply within doc-only sweep '{}': brokkr does not enumerate doctests \
         yet, so the single-test precondition cannot be checked. Drop --timeout, or use --sweep \
         to run a non-doc sweep.",
        sweep.label
    ))
}

/// The `-N` closing summary: one counts line, then one group per distinct
/// failure SET with its occurrence count - the first-seen run's failures
/// represent it, one per line when there are several.
///
/// Grouped on the whole set, not on the first failure: once every harness runs,
/// one run can fail three tests, and keying on the first of them folded a run
/// that failed A and B into the group of a run that failed A alone - the flake
/// report then said nothing about B.
fn format_repeat_summary(reports: &[RunReport]) -> Vec<String> {
    let total = reports.len();
    let count = |o: Outcome| reports.iter().filter(|r| r.outcome == o).count();
    let mut parts = vec![format!("{} PASS", count(Outcome::Pass))];
    parts.push(format!("{} FAIL", count(Outcome::Fail)));
    let build_failed = count(Outcome::BuildFailed);
    if build_failed > 0 {
        parts.push(format!("{build_failed} BUILD FAILED"));
    }
    let skipped = count(Outcome::NoMatch);
    if skipped > 0 {
        parts.push(format!("{skipped} SKIP"));
    }
    let mut lines = vec![format!(
        "[test]    summary: {total} runs - {}",
        parts.join(", ")
    )];

    // (group key, display lines, count) - insertion order, the first-seen
    // run's failures represent the group.
    let mut groups: Vec<(String, Vec<String>, usize)> = Vec::new();
    for r in reports {
        if r.outcome != Outcome::Fail {
            continue;
        }
        let key = failure_set_key(&r.failures);
        if let Some(g) = groups.iter_mut().find(|g| g.0 == key) {
            g.2 += 1;
            continue;
        }
        let display = if r.failures.is_empty() {
            vec!["unknown failure".to_owned()]
        } else {
            r.failures.iter().map(Failure::describe_qualified).collect()
        };
        groups.push((key, display, 1));
    }
    for (_, display, n) in groups {
        let mut display = display.into_iter();
        if let Some(first) = display.next() {
            lines.push(format!("[test]      {n}x {first}"));
        }
        for more in display {
            lines.push(format!("[test]         + {more}"));
        }
    }
    lines
}

/// Build one cargo package with the sweep's feature flags before
/// running tests. Returns the executables it produced (the support binaries a
/// run's inventory hashes) on build success, `Ok(None)` on build failure
/// (already reported), `Err` on spawn failure.
fn run_pre_build(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: &str,
    env: &[(&str, &str)],
    allow_args: &[String],
    debug: bool,
) -> Result<Option<Vec<check_cmd::SupportArtifact>>, DevError> {
    let args = pre_build_argv(sweep, package, allow_args, debug);
    let line = format!("cargo {} (sweep build: {})", args.join(" "), sweep.label);
    output::detail(&line);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // The command goes to the run log on success; a build that fails or errors
    // out names what it ran on the console, beside its diagnostics.
    let captured = cargo_with_deadline(&arg_refs, project_root, env, "sweep pre-build").inspect_err(|_| {
        output::run_msg(&line);
    })?;

    if captured.status.success() {
        return Ok(Some(check_cmd::support_artifacts(&String::from_utf8_lossy(&captured.stdout), package)));
    }
    output::run_msg(&line);

    let stderr = String::from_utf8_lossy(&captured.stderr);
    // The diagnostics come out of `filter_clippy`, but the command that
    // produced them is `cargo build` - label it as such.
    let mut filtered = cargo_filter::filter_clippy(&stderr);
    if filtered.starts_with("cargo clippy:") {
        filtered = filtered.replacen("cargo clippy:", "cargo build:", 1);
    }
    if !filtered.is_empty() {
        output::error(&filtered);
    }
    println!(
        "[test]    BUILD FAILED {package} (sweep: {})",
        sweep.label
    );
    Ok(None)
}

/// Run one captured cargo invocation outside the libtest runner (the sweep
/// pre-build, the `--timeout` enumeration) under [`test_runner::IDLE_TIMEOUT`].
///
/// These used to run with no deadline at all, which recreated exactly the hang
/// the idle ceiling was written for: a cargo parked on "Blocking waiting for
/// file lock on build directory" behind another cargo, for as long as that
/// cargo lived.
///
/// An *idle* bound, as the libtest runner's is: five minutes with no output
/// on either stream, every `Compiling ...` line restarting the clock. A cold
/// build of any length that keeps reporting progress runs to completion; a
/// cargo parked on a lock prints its one "Blocking" line and then nothing,
/// and is killed. A wall bound here killed legitimately long cold builds.
/// Total wall time is bounded by the `test` phase ceiling `run` arms, which
/// covers the `--timeout` enumeration too. Overrunning the idle bound stops
/// the command, as any blown budget does. The child runs in its own process
/// group (so the kill takes rustc with it), which the command's
/// `SigtermGuard` makes safe: an interrupt is forwarded to the group by the
/// runner's flag poll.
fn cargo_with_deadline(
    args: &[&str],
    project_root: &Path,
    env: &[(&str, &str)],
    what: &str,
) -> Result<output::CapturedOutput, DevError> {
    let run = output::run_captured_with_idle_deadline(
        "cargo",
        args,
        project_root,
        env,
        test_runner::IDLE_TIMEOUT,
        true,
    )?;
    if run.killed_on_deadline {
        let stderr = String::from_utf8_lossy(&run.captured.stderr);
        return Err(DevError::Build(format!(
            "{what} (`cargo {}`) printed nothing for {}s and was killed - cargo parked on \
             a build-directory lock held by another cargo looks exactly like this\n{}",
            args.join(" "),
            test_runner::IDLE_TIMEOUT.as_secs(),
            stderr.trim_end()
        )));
    }
    // The prebuild's artifact stream is what names the harnesses; one cut off
    // mid-stream could name fewer than were built.
    if run.output_cut {
        return Err(DevError::Build(format!(
            "{what} (`cargo {}`) exited, but its output did not close - something it started \
             still holds the pipe - so what it reported may be truncated and is not used",
            args.join(" ")
        )));
    }
    Ok(run.captured)
}

/// The `cargo build` argv for one sweep pre-build.
///
/// It carries the same two things every other cargo command derived from the
/// sweep carries, for the same reasons: the pinned feature unification (a
/// package-mode sweep's pre-build otherwise resolves ambiently while its
/// `cargo test` resolves per package - two feature graphs for one lane), and
/// the `[lints] allow` `--config` share, since a pre-build compiling the crate
/// under an unsuppressed lint the project denies fails before the test run is
/// ever reached. `allow_args` goes before the selection, as in [`test_argv`].
fn pre_build_argv(
    sweep: &ResolvedSweep,
    package: &str,
    allow_args: &[String],
    debug: bool,
) -> Vec<String> {
    // `json-render-diagnostics`: the artifact stream on stdout names the
    // support executables the run's inventory hashes, while compiler
    // diagnostics are still rendered as text on stderr for the failure path.
    // The message format does not enter cargo's fingerprint.
    let mut args: Vec<String> = vec!["build".into(), "--message-format=json-render-diagnostics".into()];
    args.extend(sweep.unification_args());
    args.extend(allow_args.iter().cloned());
    if !debug {
        args.push("--release".into());
    }
    args.extend(sweep.cargo_feature_args.iter().cloned());
    // Its own explicit single-package selection, never the run's: a support
    // package is usually not the package under test.
    args.extend(check_cmd::package_args(&check_cmd::Selection::Explicit(vec![package.to_owned()])));
    args
}

/// Everything that shapes a sweep's build, shared by its prebuild and every
/// run that follows it, so the two cannot name different graphs.
struct BuildShape<'a> {
    sweep: &'a ResolvedSweep,
    allow_args: &'a [String],
    /// The sweep's selection in this run: always the resolved package, as an
    /// override of the sweep's own selection.
    selection: &'a check_cmd::Selection,
    jobs: Option<u32>,
    debug: bool,
}

impl BuildShape<'_> {
    /// The one package this run tests, for messages.
    fn pkg(&self) -> &str {
        self.selection.packages().and_then(<[String]>::first).map_or("", String::as_str)
    }
}

/// The cargo half every invocation of a sweep shares: subcommand, resolution
/// pin, lint allows, profile, features, `-j`, and the one package.
///
/// `allow_args` goes before the selection: `--config` is a cargo option, and
/// everything past the `--` split belongs to libtest.
fn cargo_head(shape: &BuildShape<'_>, leading: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = leading.iter().map(|s| (*s).to_owned()).collect();
    // The lane's pinned resolution, same as every cargo command `check`
    // builds. This path is already one package per invocation (`-p pkg`
    // below), so package mode's one-resolution-per-package rule is satisfied
    // by construction and needs no loop here.
    args.extend(shape.sweep.unification_args());
    args.extend(shape.allow_args.iter().cloned());
    if !shape.debug {
        args.push("--release".into());
    }
    args.extend(shape.sweep.cargo_feature_args.iter().cloned());
    if let Some(j) = shape.jobs {
        args.push("-j".into());
        args.push(j.to_string());
    }
    args.extend(check_cmd::package_args(shape.selection));
    args
}

/// The prebuild that compiles a split sweep's test harnesses and lists them.
///
/// `--tests` because that is the selection a named `cargo test -p PKG NAME`
/// resolves to (a positional test name with no target flag selects all test
/// targets, not cargo's default set): the default set also builds every
/// example merely to check it compiles, so an unrelated broken example would
/// newly fail the build. `json-render-diagnostics` keeps the artifact records
/// on stdout for the listing while compiler errors stay readable on stderr.
fn prebuild_argv(shape: &BuildShape<'_>) -> Vec<String> {
    let mut args =
        cargo_head(shape, &["test", "--no-run", "--message-format=json-render-diagnostics"]);
    args.push("--tests".into());
    args
}

/// The `cargo test --doc` argv for one doc-only sweep's run of `name`. The
/// libtest half is [`direct_libtest_args`]'s, minus the filter, which cargo
/// takes positionally.
fn test_argv(shape: &BuildShape<'_>, name: &str) -> Vec<String> {
    let mut args = cargo_head(shape, &["test"]);
    // A doc-only sweep runs doctests and nothing else, here as everywhere:
    // <NAME> filters within the `--doc` pseudo-target, and a name matching
    // no doctest SKIPs like any feature-gated miss.
    args.push("--doc".into());
    args.push(name.into());
    args.push("--".into());
    args.extend(direct_libtest_args(name, false).into_iter().skip(1));
    args
}

/// Build a sweep's test harnesses once and list them in cargo's run order,
/// with the runtime index direct execution reconstructs cargo's launch
/// environment from. `Ok(None)` is a build failure, already reported.
///
/// Bounded like the sweep pre-build, by the idle ceiling: a cold build of any
/// length that keeps printing runs to completion, a cargo parked on a lock
/// does not.
fn prebuild_targets(
    shape: &BuildShape<'_>,
    env: &[(&str, &str)],
    project_root: &Path,
    quiet: bool,
    label: Option<&str>,
) -> Result<Option<(Vec<check_cmd::TestBinary>, check_cmd::BuildRuntimeIndex)>, DevError> {
    let args = prebuild_argv(shape);
    output::detail(&format!("cargo {} (sweep: {})", args.join(" "), shape.sweep.label));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    // The command is routine on success and goes to the run log; a build that
    // fails or errors out names what it ran on the console, beside its
    // diagnostics - the rule `brokkr check` follows.
    let announce = || output::run_msg(&format!("cargo {} (sweep: {})", args.join(" "), shape.sweep.label));
    let captured = cargo_with_deadline(&arg_refs, project_root, env, "test build").inspect_err(|_| {
        announce();
    })?;
    if !captured.status.success() {
        announce();
        let stderr = String::from_utf8_lossy(&captured.stderr);
        let filtered = cargo_filter::filter_test_build_failure(&stderr);
        if !filtered.is_empty() {
            output::error(&filtered);
        }
        println!(
            "[test]    BUILD FAILED {} (sweep: {})",
            shape.pkg(), shape.sweep.label
        );
        return Ok(None);
    }
    // An unrecognised stream is not an empty one: treating it as "no harnesses"
    // would SKIP every sweep and report a bad test name for what is really a
    // cargo whose output brokkr could not read.
    let Some(built) = check_cmd::prebuilt_harnesses(&String::from_utf8_lossy(&captured.stdout))
    else {
        return Err(DevError::Build(format!(
            "cargo exited successfully but produced no recognisable artifact stream for: cargo {} \
             - brokkr cannot tell which test harnesses exist",
            args.join(" ")
        )));
    };
    if !quiet {
        println!(
            "[test]    {}{} built in {:.1}s",
            sweep_prefix(label),
            output::count(built.0.len(), "harness"),
            started.elapsed().as_secs_f64(),
        );
    }
    Ok(Some(built))
}

/// The smallest package the whole-suite refusal applies to. Below it, a name
/// matching every test is as likely a deliberate "run this small crate's
/// tests" as a degenerate filter, and the flood it would cause is small.
const WHOLE_SUITE_FLOOR: usize = 5;

/// Whether `name`, as libtest's substring filter, selects every listed test of
/// a package at or above [`WHOLE_SUITE_FLOOR`] - the test for refusing it.
///
/// `brokkr test` streams each test's output live under `--nocapture`; it is
/// the tool for one test or a few. A substring every test name contains -
/// `::`, `_`, a single letter - turns it into a whole-suite run that buries the
/// verdicts under every test's prints, which is `brokkr check -p`'s job. The
/// parse-time blank check catches `""`; this catches every other spelling of
/// it, judged by what the filter matches rather than how it looks. `listed` is
/// what discovery found across the package's eligible harnesses, benchmarks
/// included.
fn matches_whole_suite(listed: &[String], name: &str) -> bool {
    listed.len() >= WHOLE_SUITE_FLOOR && listed.iter().all(|t| t.contains(name))
}

/// Renders a lone failing test's footer tag from its full name and the wall.
type TestTag<'a> = dyn Fn(&str, &str) -> String + 'a;

/// How one invocation is labelled and bounded.
struct OneRun<'a> {
    tag: &'a str,
    /// The footer tag a single failing test leads with instead of `tag`,
    /// rendered from the run's structured context given the test's full name
    /// and the wall time; `None` where the run has no such context (a doc-only
    /// sweep).
    test_tag: Option<&'a TestTag<'a>>,
    /// The harness, for attributing failures; empty for a whole-package run.
    target: &'a str,
    ceilings: test_runner::Ceilings,
    /// Print the "built in Xs; running tests" framing line (the doc-only
    /// cargo run's; a directly run harness has no build phase).
    announce: bool,
    /// For a directly executed harness, how many tests discovery listed as
    /// matching: the run must account for exactly that many. `None` for the
    /// doc-only cargo run, which nothing enumerated.
    expected: Option<usize>,
    /// The test names discovery listed as matching this harness: a failure
    /// is a `Test` only under one of them. `None` where nothing enumerated.
    listed: Option<&'a [String]>,
    /// Where the run's typed observations go (the run's journal), and no
    /// harness isolation: a directly executed harness is already one stream.
    observe: test_runner::Observe,
}

/// One sweep's cargo environment, plus the `--config` args that must ride its
/// argv.
///
/// The two are returned together because one decision produces both: the
/// `[lints] allow` flags reach the compiler through the environment or through
/// cargo's command line depending on which rustflags layer this build actually
/// reads (see [`crate::rustflags`]), never both.
///
/// A sweep carrying `rustflags` - or package-mode `feature_unification`, on the
/// same fingerprint-thrash grounds - gets its own isolated target dir + matching
/// BROKKR_TEST_BIN_DIR + composed RUSTFLAGS, so running a sim sweep through
/// `brokkr test` builds under its cfg without thrashing the plain sweeps. Both
/// come from `sweep_runtime_env`, so this command and `check` place a lane's
/// artifacts in the same directory rather than each deriving its own. The
/// profile-declared env is merged on top of the project's always-set vars, and
/// wins on collision so a profile can shadow defaults when it really needs to
/// (request 3 / B3: brokkr test was dropping this and a `default_profile` env
/// didn't apply).
fn sweep_env(
    sweep: &ResolvedSweep,
    project: Project,
    project_root: &Path,
    target_dir: &Path,
    profile_dir: &str,
    allow_flags: &[String],
) -> (Vec<(String, String)>, Vec<String>) {
    let (env_allows, allow_args) =
        rustflags::plumbing(project_root, !sweep.rustflags.is_empty(), allow_flags);
    let project_env =
        check_cmd::sweep_runtime_env(sweep, Some(project), target_dir, profile_dir, env_allows);
    (
        check_cmd::merged_env(&sweep.env, project_env.as_slice()),
        allow_args,
    )
}

/// The `[lints] allow` flags for this command's builds.
///
/// `brokkr test` reads the section for the same reason `check`'s test phase
/// does: a lint that the project's `-Dwarnings` turns into a compile error
/// stops this command too, and a suppression that worked in one and not the
/// other is the asymmetry the section exists to close.
fn lint_allow_flags(dev_config: &DevConfig) -> Vec<String> {
    dev_config.lints.as_ref().map_or_else(Vec::new, |l| {
        config::test_phase_allow_flags(&l.allow, &l.allow_exact)
    })
}

/// Resolve one sweep's cargo profile for `brokkr test` to a `debug` bool,
/// in precedence order:
///   1. An explicit `--debug`/`--release` on the CLI (`profile_override`:
///      `Some(true)` / `Some(false)`) - the per-invocation answer always wins.
///   2. The sweep's own `[[check]] profile`, when it names one. A sweep that
///      declares its profile has a reason (a wall-clock contract that only
///      holds optimized, a lane pinned to dev for speed), and a project-wide
///      default must not silently overrule it - that is the whole point of
///      the key: `brokkr test <that test>` runs it the way it is meant to run
///      without the caller having to remember a flag.
///   3. `[test] debug`, the project-wide default, `false` (release) when
///      there is no `[test]` section at all.
fn resolve_debug(
    profile_override: Option<bool>,
    test_cfg: Option<&crate::config::TestConfig>,
    sweep_profile: Option<crate::config::SweepProfile>,
) -> bool {
    if let Some(explicit) = profile_override {
        return explicit;
    }
    if let Some(p) = sweep_profile {
        return p == crate::config::SweepProfile::Dev;
    }
    test_cfg.is_some_and(|t| t.debug)
}

/// Resolve the cargo package name in precedence order:
///   1. Explicit `-p` on the CLI (most specific)
///   2. `[test] default_package` in brokkr.toml (explicit config wins
///      over implicit per-project heuristic)
///   3. `Project::cli_package()` (built-in knowledge for pbfhogg/nidhogg)
///   4. Error with a message pointing the user at options 1/2
///
/// Whatever the source, the spec must name ONE package: cargo accepts globs in
/// `-p`, and a glob selecting several packages would put several packages'
/// harnesses back into one invocation - the shape `run_split` exists to
/// dissolve - and change the feature graph between the prebuild and each run.
///
/// Returns the package and where it came from: the run's selection carries
/// the source as the override's provenance.
fn resolve_package(
    cli_package: Option<&str>,
    dev_config: &DevConfig,
    project: Project,
) -> Result<(String, check_cmd::PackageSource), DevError> {
    let (spec, source) = resolve_package_spec(cli_package, dev_config, project)?;
    Ok((single_package(spec)?, source))
}

/// Refuse a package spec cargo would treat as a glob.
fn single_package(pkg: String) -> Result<String, DevError> {
    if pkg.contains(['*', '?', '[']) {
        return Err(DevError::Config(format!(
            "'brokkr test' runs one package, and `{pkg}` is a glob that cargo may expand to \
             several. Name the package."
        )));
    }
    Ok(pkg)
}

fn resolve_package_spec(
    cli_package: Option<&str>,
    dev_config: &DevConfig,
    project: Project,
) -> Result<(String, check_cmd::PackageSource), DevError> {
    use check_cmd::PackageSource;
    if let Some(p) = cli_package {
        return Ok((p.to_owned(), PackageSource::Cli));
    }
    if let Some(cfg) = &dev_config.test
        && let Some(p) = &cfg.default_package
    {
        return Ok((p.clone(), PackageSource::DefaultPackage));
    }
    if let Some(p) = project.cli_package() {
        return Ok((p.to_owned(), PackageSource::ProjectDefault));
    }
    Err(DevError::Config(format!(
        "'brokkr test' needs a cargo package for '-p'. This project ({project}) has no built-in default. \
         Pass `-p <pkg>` on the command line, or set `[test] default_package = \"...\"` in brokkr.toml."
    )))
}

fn aggregate_exit(outcomes: &[Outcome], pkg: &str, name: &str, searched: &Searched) -> Result<(), DevError> {
    let test_failed = outcomes.contains(&Outcome::Fail);
    let build_failed = outcomes.contains(&Outcome::BuildFailed);
    // `Reported`: every FAIL and BUILD FAILED already printed its footer, so
    // the label only closes the run, and it names what failed: a build-only
    // failure must not close with `test failed`, nor a test failure with a
    // build error (`Build` once rendered it as `build: test failed`).
    if test_failed || build_failed {
        let label = match (test_failed, build_failed) {
            (true, true) => "test failed and build failed",
            (true, false) => "test failed",
            _ => "build failed",
        };
        return Err(DevError::Reported(label.into()));
    }
    let all_no_match = outcomes.iter().all(|o| *o == Outcome::NoMatch);
    if all_no_match {
        // The one place the fact is stated: the closing `[error]` line carries
        // it, with what was searched, so no per-sweep SKIP line repeats it.
        return Err(DevError::Reported(format!(
            "no test matches `{name}` in {pkg}{}",
            searched.describe()
        )));
    }
    // At least one sweep passed; NoMatch in other sweeps is informational
    // (the test was feature-gated out of those sweeps).
    Ok(())
}

/// Decide which sweeps `brokkr test` runs.
///
/// Reuses `check_cmd::decide_active_sweeps` (no CLI features, no
/// `--profile` override - resolution falls through to
/// `[test] default_profile` -> `[[check]]` entries -> legacy
/// `--all-features`), then drops the libtest filters that would
/// fight with the user's `<name>` argument. `env` is preserved (B3:
/// silent profile-env drop fixed by this consolidation).
///
/// Resolve the profile's sweeps and apply a `--sweep` selection. Deduping lane
/// duplicates is deliberately *not* done here: the run's selection does it
/// (`PhaseSelection::for_brokkr_test`), after `--sweep` and with the package
/// known, so a lane-qualified label (`serial/all`) can still reach a sweep a
/// collapse would have discarded, and the dedupe compares what the sweeps
/// would actually execute for that package.
fn resolve_sweeps(
    test_cfg: Option<&crate::config::TestConfig>,
    check_entries: &[crate::config::CheckEntry],
    sweep_filter: Option<&str>,
) -> Result<Vec<ResolvedSweep>, DevError> {
    let sweeps = decide_sweeps(test_cfg, check_entries)?;
    select_sweep(sweeps, sweep_filter)
}

fn decide_sweeps(
    test_cfg: Option<&crate::config::TestConfig>,
    check_entries: &[crate::config::CheckEntry],
) -> Result<Vec<ResolvedSweep>, DevError> {
    let mut sweeps = check_cmd::decide_active_sweeps(check_entries, test_cfg, None, &[], false)?;
    for s in &mut sweeps {
        // The user's `<name>` is the libtest filter. Profile-level
        // `only` / `skip` / `tests` would either narrow it further (rare,
        // surprising) or cause silent zero-match failures; drop them.
        s.libtest_args.clear();
        s.cargo_test_filters.clear();
        s.name_filters.clear();
        s.qualified_skips.clear();
        // Nothing declared applies any more, so nothing can be audited as
        // dead here either - and `brokkr test` runs no coverage phase anyway.
        s.declared_filters.clear();
        // `brokkr test` has no parallel fan-out to promote for, so the only
        // question is what the entry pinned. Resolving it here rather than
        // leaving the field at its default is what stops the key being
        // silently ignored by this command while `check` honours it - the two
        // must agree on what a lane means, or a green `brokkr test` would be
        // evidence about a build `check` never runs.
        s.effective_unification = crate::profile::resolve_unification(s, false)?;
    }
    Ok(sweeps)
}

/// Narrow the resolved sweep set to a single `--sweep <label>`. Passing
/// `None` keeps every sweep. An unknown label is a hard error that lists
/// the labels actually available, so a typo can't silently run nothing.
fn select_sweep(
    sweeps: Vec<ResolvedSweep>,
    filter: Option<&str>,
) -> Result<Vec<ResolvedSweep>, DevError> {
    let Some(label) = filter else {
        return Ok(sweeps);
    };
    let available: Vec<&str> = sweeps.iter().map(|s| s.label.as_str()).collect();
    if let Some(pos) = sweeps.iter().position(|s| s.label == label) {
        // Preserve the matched sweep only; index is valid by construction.
        return Ok(vec![sweeps.into_iter().nth(pos).expect("matched sweep")]);
    }
    Err(DevError::Config(format!(
        "--sweep {label} matches no sweep in the resolved profile; available: {}",
        available.join(", ")
    )))
}

/// Run one `cargo test` invocation. Prints the `[test]` footer and returns
/// the run report. Err only on spawn failure. `shape.announce` gates the
/// "test binaries built in Xs" framing line - false for `-N` repeats
/// after the first, where the build is cached and the line is noise, and
/// for a split run, whose prebuild already said it.
/// `buffered` (repeats only) routes the streamed display into a buffer
/// that is flushed once the outcome is known - and dropped entirely when
/// the failure set was already shown by an earlier run, leaving
/// just the footer.
#[allow(clippy::too_many_arguments)]
fn run_one(
    launch: test_runner::Launch<'_>,
    args: &[&str],
    cwd: &Path,
    state_root: &Path,
    env: &[(&str, &str)],
    shape: &OneRun<'_>,
    repeat_state: &RepeatState,
    buffered: bool,
) -> Result<RunReport, DevError> {
    let tag = shape.tag;
    let target = shape.target;
    let announce = shape.announce;
    let direct = launch != test_runner::Launch::Cargo;
    let sink: Option<LineSink> = buffered.then(LineSink::default);
    let built_tag = tag.to_owned();
    let run = test_runner::streaming_run_libtest(
        launch,
        args,
        cwd,
        state_root,
        env,
        shape.ceilings.clone(),
        &shape.observe,
        make_stdout_forwarder(sink.clone()),
        // A direct run has no cargo compile phase: everything on its stderr
        // is the test talking.
        make_stderr_forwarder(sink.clone(), direct),
        move |elapsed| {
            if announce {
                println!(
                    "[test]    {built_tag}: built in {:.1}s; running tests",
                    elapsed.as_secs_f64()
                );
            }
        },
    )?;

    let stdout_text = String::from_utf8_lossy(&run.captured.stdout);
    let stderr_text = String::from_utf8_lossy(&run.captured.stderr);
    let stdout_lines: Vec<&str> = stdout_text.lines().collect();
    let stderr_lines: Vec<&str> = stderr_text.lines().collect();
    // stderr matters: panics print there, and under --nocapture it's the
    // only place the FAIL footer can recover the message/location from.
    let parsed = cargo_filter::parse_test_output_with_stderr(&stdout_lines, &stderr_lines);

    let has_test_result = stdout_lines.iter().any(|l| l.starts_with("test result:"));
    // Only cargo compiles; a directly run harness printing `error: ...` is a
    // test talking, never a build failure.
    let has_compile_error =
        !direct && stderr_lines.iter().any(|l| stderr_indicates_compile_error(l));

    // Display the test-runtime wall: total minus the cargo build phase
    // (which the `[test] test binaries built in ...s` line already
    // surfaces). Falls back to total if cargo never reported `Finished`.
    let test_wall = run
        .build_elapsed
        .map_or(run.captured.elapsed, |b| run.captured.elapsed.saturating_sub(b));
    let wall = format!("{:.2}s", test_wall.as_secs_f64());

    if let LibtestOutcome::HungTest(hung) = &run.outcome {
        return Ok(report_hung(shape, hung, &wall, cwd, repeat_state, sink));
    }

    if !has_test_result && has_compile_error {
        // One call: `first_sighting` records the signature, so asking twice
        // would answer `true` then `false` and suppress the block it just
        // decided to show.
        let first = repeat_state.first_sighting("build failed");
        flush_sink(sink, !first);
        return Ok(report_build_failure(tag, &wall, first, stderr_text.as_ref()));
    }

    // Every failure this harness reported, not the first: the footer is how a
    // mutation check reads which tests went red, and a list cut to one entry
    // reads as "only this one".
    let mut failures: Vec<Failure> = parsed
        .failures
        .iter()
        .map(|f| Failure {
            kind: failure_kind(f, shape.listed),
            target: target.to_owned(),
            what: f.name.clone(),
            loc: f.location.clone(),
            msg: f.message.clone(),
        })
        .collect();
    // A harness that exited non-zero without finishing its report - or with
    // nothing on the roster to account for the exit - failed at the harness
    // level: a crash, an abort, a panic outside any test. It is a failure of
    // its own, beside whatever tests it had already failed.
    if !run.captured.status.success() && (!parsed.is_complete() || failures.is_empty()) {
        failures.push(harness_failure(
            target,
            &run.captured.status,
            direct,
            &stderr_lines,
            &run.in_flight,
        ));
    }
    // A failing harness is still held to its listing: three matched and one
    // reported failing, in a complete stream, means two went missing - which a
    // mutation check reading "what went red" needs to see beside the failure.
    if !failures.is_empty()
        && let Some(want) = shape.expected
        && parsed.is_complete()
        && parsed.accounted() != want
    {
        failures.push(discovery_mismatch(target, want, parsed.accounted()));
    }
    if !failures.is_empty() {
        // Suppress the streamed block when this exact failure set already
        // printed in full on an earlier run - the footer carries the list.
        let first = repeat_state.first_sighting(&failure_set_key(&failures));
        flush_sink(sink, !first);
        print_fail_footer(tag, shape.test_tag, &wall, &failures);
        return Ok(RunReport { outcome: Outcome::Fail, timed_out: false, failures });
    }

    if let Some(want) = shape.expected
        && let Some(report) = disagrees_with_discovery(tag, target, &wall, want, &parsed)
    {
        flush_sink(sink, false);
        return Ok(report);
    }

    // Zero tests ran in the doc-only run (a directly run harness answered
    // above): the name matched no doctest in this sweep. Nothing prints here;
    // the caller decides whether this is a real error (all sweeps missed) or
    // fine (feature-gated out of this one), and says so once, with the sweeps
    // that did run the name in view.
    //
    // Three guards, each for a way a zero can lie:
    //
    // - Ordered AFTER the unsuccessful-exit check. A run that crashed hard
    //   enough to report no counts - a SIGABRT or stack overflow before the
    //   first `test result:` line, a harness dying in a static initializer -
    //   has zero counts too, and calling that "no tests matched" turned a red
    //   run green: the name was fine, the binary blew up.
    // - `is_complete()`: a stream that stopped mid-run has zeros because
    //   nothing reported them, not because nothing matched.
    // - `accounted()` rather than passed+failed: an invocation that matched
    //   only `#[ignore]`d tests really did match. Currently unreachable, since
    //   this command always passes `--include-ignored`, but the check should
    //   say what it means rather than rely on a flag elsewhere staying put.
    if parsed.is_complete() && parsed.accounted() == 0 {
        flush_sink(sink, false);
        return Ok(RunReport::bare(Outcome::NoMatch));
    }

    // An incomplete stream cannot reach PASS. Requiring completeness for the
    // no-match branch and then falling through to an unconditional PASS was a
    // wrong verdict with a short recipe: emit a start-shaped line, exit 0, and a
    // run that reported no result for anything was declared green. A test can do
    // that by printing and terminating the harness; an abnormal or custom harness
    // gets there without trying.
    //
    // Fail closed. The counts are whatever was observed, so they still go in the
    // message - a partial report is useful, it just is not a pass.
    if let cargo_filter::Completeness::Incomplete { reason } = &parsed.completeness {
        flush_sink(sink, false);
        return Ok(report_incomplete(tag, target, &wall, reason, &parsed));
    }

    flush_sink(sink, false);
    println!("[test]    PASS {tag} ({wall})");
    std::io::stdout().flush().ok();
    Ok(RunReport::bare(Outcome::Pass))
}

/// What a parsed failure is. A `Test` needs libtest's verdict behind its name
/// (`verdict`): the parser falls back to a panic's thread name when none ties
/// to the failed roster, and a test can rename its thread - to `worker`, or to
/// another selected test that then passes. Where discovery listed the harness,
/// the name must also be one it listed. With no listing (a doc-only run) the
/// verdict alone decides.
fn failure_kind(f: &cargo_filter::ParsedTestFailure, listed: Option<&[String]>) -> FailureKind {
    let selectable = listed.is_none_or(|l| l.contains(&f.name));
    if f.verdict && selectable { FailureKind::Test } else { FailureKind::Harness }
}

/// Report a run the watchdog killed for blowing its budget. The hang's
/// diagnosis prints once per signature across `-N` repeats.
fn report_hung(
    shape: &OneRun<'_>,
    hung: &test_runner::HungTest,
    wall: &str,
    cwd: &Path,
    repeat_state: &RepeatState,
    sink: Option<LineSink>,
) -> RunReport {
    let first = repeat_state.first_sighting(&format!("{}|hung {}", shape.target, hung.test));
    flush_sink(sink, !first);
    if first {
        output::error(&test_runner::format_hung_test(hung, cwd));
    }
    let secs = hung.ceiling.as_secs();
    if let Some(test_tag) = hung_test_tag(shape, hung) {
        println!("[test]    FAIL {} - hung test exceeded {secs}s", test_tag(&hung.test, wall));
    } else {
        println!("[test]    FAIL {} ({wall}) - hung test exceeded {secs}s", shape.tag);
    }
    std::io::stdout().flush().ok();
    RunReport::timed_out(shape.target, format!("hung test exceeded {}s", hung.ceiling.as_secs()))
}

/// The footer tag renderer a hung test leads with, when its name is a verified
/// identity: the run held exactly one test, named by the caller
/// (`WallShape::OneTest`), and the kill charged that test. The runner classifies
/// both a tracker expiry and this one-test invocation's wall expiry as
/// `PerTest`, using the caller's resolved name. On a shared harness
/// the name is the tracker's best-known suspect, and a sweep-wall or idle kill
/// has no test to name at all, so those keep the filter-led form.
fn hung_test_tag<'a>(shape: &OneRun<'a>, hung: &test_runner::HungTest) -> Option<&'a TestTag<'a>> {
    let one_test = matches!(shape.ceilings.shape, test_runner::WallShape::OneTest { .. });
    let charged = matches!(hung.reason, test_runner::TimeoutReason::PerTest { .. });
    if one_test && charged { shape.test_tag } else { None }
}

/// The verdict for a directly run harness whose run does not account for
/// exactly the `want` tests discovery listed as matching it, or `None` when it
/// does.
///
/// The harness was chosen because discovery found the name in it, so zero is
/// not a SKIP here: a run that then reports none - or a different number -
/// disagrees with its own listing, and no verdict can rest on that. A stream
/// that never finished reporting says so in its own words.
fn disagrees_with_discovery(
    tag: &str,
    target: &str,
    wall: &str,
    want: usize,
    parsed: &cargo_filter::ParsedTestResults,
) -> Option<RunReport> {
    if let cargo_filter::Completeness::Incomplete { reason } = &parsed.completeness {
        return Some(report_incomplete(tag, target, wall, reason, parsed));
    }
    let got = parsed.accounted();
    if got == want {
        return None;
    }
    let failure = discovery_mismatch(target, want, got);
    println!("[test]    FAIL {tag} ({wall}) - {}", failure.describe());
    std::io::stdout().flush().ok();
    Some(RunReport { outcome: Outcome::Fail, timed_out: false, failures: vec![failure] })
}

/// The failure a harness files when its run accounts for `got` tests where
/// discovery matched `want` in it.
fn discovery_mismatch(target: &str, want: usize, got: usize) -> Failure {
    Failure {
        kind: FailureKind::Harness,
        target: target.to_owned(),
        what: "run disagrees with discovery".to_owned(),
        loc: None,
        msg: Some(format!(
            "discovery listed {} matching here, the run reported {got}",
            output::count(want, "test")
        )),
    }
}

/// End the command because a test blew its time budget.
///
/// Distinct from a failing test, where a `-N` run's whole purpose is to keep
/// going and count the flakes. A timeout means the run is already outside the
/// contract every later measurement assumes, and whatever wedged it - a deadlock,
/// a lock nobody will release, a runaway child - is still there for the next
/// iteration and the next sweep to inherit. The repeat summary still prints, so
/// the iterations that did run are not lost.
fn stop_for_budget(reports: &[RunReport], repeat: u32, label: &str) -> DevError {
    if repeat > 1 {
        for line in format_repeat_summary(reports) {
            println!("{line}");
        }
    }
    // `Reported`: the hung test's FAIL line and diagnosis already printed.
    DevError::Reported(format!(
        "a test exceeded its time budget in sweep '{label}' - stopping"
    ))
}

/// The ceilings for one invocation.
///
/// Both shapes enforce the same cap - every test gets `ceiling` of wall time and
/// no more. They differ only in what a kill can be *called*: with `--exact` on a
/// resolved single test the process runs exactly one test, so the caller knows
/// the name; without it the substring filter can match several tests in one
/// process and the name is the tracker's best-known suspect.
fn ceilings_for(exact: &Option<String>, ceiling: Duration) -> test_runner::Ceilings {
    match exact {
        Some(resolved) => test_runner::Ceilings::one_test(ceiling, resolved),
        None => test_runner::Ceilings::shared_harness_with(ceiling),
    }
}

/// Report a run that failed to build. Compile errors are identical across `-N`
/// repeats by construction (same source, same flags), so the block prints once
/// and later repeats collapse to the footer.
fn report_build_failure(
    tag: &str,
    wall: &str,
    first: bool,
    stderr_text: &str,
) -> RunReport {
    if first {
        let filtered = cargo_filter::filter_test_build_failure(stderr_text);
        if !filtered.is_empty() {
            output::error(&filtered);
        }
    }
    println!("[test]    BUILD FAILED {tag} ({wall})");
    std::io::stdout().flush().ok();
    RunReport::bare(Outcome::BuildFailed)
}

/// Report a run whose stream never finished describing itself.
///
/// Fail, never PASS. Requiring completeness for the no-match branch and then
/// falling through to an unconditional PASS was a wrong verdict with a short
/// recipe: emit a start-shaped line, exit 0, and a run that reported no result
/// for anything was declared green. A test can do that by printing and
/// terminating the harness; an abnormal or custom harness gets there without
/// trying. The observed counts still go in the message, because a partial report
/// is useful - it just is not a pass.
fn report_incomplete(
    tag: &str,
    target: &str,
    wall: &str,
    reason: &str,
    parsed: &cargo_filter::ParsedTestResults,
) -> RunReport {
    println!(
        "[test]    FAIL {tag} ({wall}) - the test stream did not finish reporting: {reason}. \
         Observed {} passed, {} failed, {} ignored. The process exited successfully, so this is a \
         harness that stopped talking rather than a failing test.",
        parsed.passed, parsed.failed, parsed.ignored
    );
    std::io::stdout().flush().ok();
    RunReport {
        outcome: Outcome::Fail,
        timed_out: false,
        failures: vec![Failure {
            kind: FailureKind::Harness,
            target: target.to_owned(),
            what: "incomplete test stream".to_owned(),
            loc: None,
            msg: Some(reason.to_owned()),
        }],
    }
}

/// A harness that failed as a process rather than through a test verdict.
///
/// For a directly run harness (`direct`) the status brokkr holds is the
/// harness's own, so the detail is read from it. Under cargo the status is
/// cargo's (101), so the detail comes from cargo's `process didn't exit
/// successfully: ... (signal: 6, SIGABRT: process abort signal)` line when
/// there is one. The last test the tracker saw start is named as the SUSPECT
/// only: it is read from the stream the test itself writes to, so a lost start
/// record or a record a test printed can move it, and a crash before the first
/// start names nothing.
fn harness_failure(
    target: &str,
    status: &std::process::ExitStatus,
    direct: bool,
    stderr_lines: &[&str],
    in_flight: &[String],
) -> Failure {
    let detail = if direct {
        describe_harness_status(status)
    } else {
        stderr_lines
            .iter()
            .rev()
            .find(|l| l.contains("process didn't exit successfully:"))
            .and_then(|l| {
                let open = l.rfind(" (")?;
                l[open + 2..].strip_suffix(')').map(str::to_owned)
            })
            .unwrap_or_else(|| match status.code() {
                Some(code) => format!("cargo exited with code {code}"),
                None => "cargo was killed by a signal".to_owned(),
            })
    };
    let msg = (!in_flight.is_empty()).then(|| {
        format!(
            "last test seen starting: {} (a suspect - named by the harness's own output)",
            in_flight.join(", ")
        )
    });
    Failure {
        kind: FailureKind::Harness,
        target: target.to_owned(),
        what: format!("test harness failed ({detail})"),
        loc: None,
        msg,
    }
}

/// A directly run harness's exit status in cargo's words (`signal: 6,
/// SIGABRT`, `exit status: 101`), so a crash reads the same whichever way the
/// harness was launched.
fn describe_harness_status(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(sig) = status.signal() {
        let name = match sig {
            libc::SIGABRT => ", SIGABRT",
            libc::SIGSEGV => ", SIGSEGV",
            libc::SIGBUS => ", SIGBUS",
            libc::SIGILL => ", SIGILL",
            libc::SIGFPE => ", SIGFPE",
            libc::SIGKILL => ", SIGKILL",
            libc::SIGTERM => ", SIGTERM",
            _ => "",
        };
        return format!("signal: {sig}{name}");
    }
    match status.code() {
        Some(code) => format!("exit status: {code}"),
        None => "ended without an exit status".to_owned(),
    }
}

/// The FAIL footer: one line for a single failure, a counted header and one
/// line per failure otherwise.
///
/// A lone failing test leads with its own full name (`test_tag`), not the
/// user's filter: that is the identity a reader acts on. Only when it is the
/// sole failure - a test failure beside a harness failure keeps the
/// filter-led form, since the harness failure has no test name to lead with.
fn print_fail_footer(
    tag: &str,
    test_tag: Option<&TestTag<'_>>,
    wall: &str,
    failures: &[Failure],
) {
    if let [only] = failures
        && only.kind == FailureKind::Test
        && let Some(test_tag) = test_tag
    {
        let detail = match (&only.msg, &only.loc) {
            (Some(m), Some(l)) => format!(" - {m} @ {l}"),
            (Some(m), None) => format!(" - {m}"),
            (None, Some(l)) => format!(" - @ {l}"),
            (None, None) => String::new(),
        };
        println!("[test]    FAIL {}{detail}", test_tag(&only.what, wall));
    } else if let [only] = failures {
        println!("[test]    FAIL {tag} ({wall}) - {}", only.describe());
    } else {
        println!("[test]    FAIL {tag} ({wall}) - {} failures:", failures.len());
        for f in failures {
            println!("[test]      {}", f.describe());
        }
    }
    std::io::stdout().flush().ok();
}

/// Flush a repeat-run display buffer to stdout, or drop it when the
/// failure block was already shown by an earlier run. No-op in live
/// (run 1) mode where `sink` is `None`.
fn flush_sink(sink: Option<LineSink>, suppress: bool) {
    let Some(s) = sink else { return };
    if suppress {
        return;
    }
    let lines = s.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
    if lines.is_empty() {
        return;
    }
    let mut out = std::io::stdout().lock();
    for l in &lines {
        writeln!(out, "{l}").ok();
    }
    out.flush().ok();
}

fn make_stdout_forwarder(sink: Option<LineSink>) -> impl FnMut(&str) + Send + 'static {
    let mut cond = StdoutCondenser::new();
    move |line| {
        let lines = cond.next(line);
        if lines.is_empty() {
            return;
        }
        if let Some(s) = &sink {
            if let Ok(mut v) = s.lock() {
                v.extend(lines);
            }
            return;
        }
        let mut out = std::io::stdout().lock();
        for l in &lines {
            writeln!(out, "{l}").ok();
        }
        out.flush().ok();
    }
}

/// Display-side condenser for the streamed test stdout. Pure state
/// machine (returns the lines to print) so the framing rules are unit
/// testable without capturing the process's stdout.
///
/// Rules:
/// - framing lines rejected by `keep_stdout_line` are dropped;
/// - a `failures:` header is held back until a non-blank line follows.
///   Libtest prints the section twice (per-test output blocks, then the
///   name list) and under `--nocapture` the first is always empty -
///   consecutive headers collapse to one and a dangling empty header
///   is dropped entirely;
/// - leading blanks and runs of consecutive blanks collapse to one.
struct StdoutCondenser {
    prev_blank: bool,
    pending_failures: bool,
}

impl StdoutCondenser {
    fn new() -> Self {
        Self {
            // Starts `true` so any blank line before we print anything is
            // eaten - that gets rid of the gap cargo leaves between
            // "Finished ..." and the test output.
            prev_blank: true,
            pending_failures: false,
        }
    }

    fn next(&mut self, line: &str) -> Vec<String> {
        if !keep_stdout_line(line) {
            return Vec::new();
        }
        if line.trim() == "failures:" {
            self.pending_failures = true;
            return Vec::new();
        }
        let is_blank = line.trim().is_empty();
        if self.pending_failures {
            if is_blank {
                return Vec::new();
            }
            self.pending_failures = false;
            self.prev_blank = false;
            return vec!["failures:".to_owned(), line.to_owned()];
        }
        if is_blank && self.prev_blank {
            return Vec::new();
        }
        self.prev_blank = is_blank;
        vec![line.to_owned()]
    }
}

/// `direct` starts the forwarder in the test phase: a directly executed
/// harness has no cargo in front of it, so no compile chatter and no
/// `Running ...` line will ever arrive to switch it over - without this its
/// whole stderr was filtered as compile output.
fn make_stderr_forwarder(
    sink: Option<LineSink>,
    direct: bool,
) -> impl FnMut(&str) + Send + 'static {
    // Cargo emits compile noise (warnings, errors, progress) on stderr before
    // launching the test binary. The test's own eprintln! also lands here
    // once the binary runs. Split on the first "Running tests/..." line:
    // before it, filter aggressively; after it, pass through (it's the test
    // talking) - except further `Running <target> (<path>/deps/...)` lines,
    // which cargo re-emits between every test binary in the package and
    // which carry no signal (one such line per suite, ~10 in piners-runner),
    // and the test-phase noise lines (`note: run with RUST_BACKTRACE`,
    // cargo's `error: test failed, to rerun pass ...` - brokkr *is* the
    // rerun tool).
    let mut in_test_phase = direct;
    let mut in_compile_block = false;
    let mut prev_blank = true;
    move |line| {
        let want = if is_cargo_running_line(line) {
            in_test_phase = true;
            false
        } else if in_test_phase {
            !is_test_phase_noise(line)
        } else {
            keep_stderr_compile_line(line, &mut in_compile_block)
        };
        if want {
            let is_blank = line.trim().is_empty();
            if !(is_blank && prev_blank) {
                prev_blank = is_blank;
                if let Some(s) = &sink {
                    if let Ok(mut v) = s.lock() {
                        v.push(line.to_owned());
                    }
                    return;
                }
                let mut err = std::io::stderr().lock();
                writeln!(err, "{line}").ok();
                err.flush().ok();
            }
        }
    }
}

/// Pure-noise lines in the test phase of stderr: the backtrace hint
/// (brokkr's footer already carries the panic message/location) and
/// cargo's rerun suggestion (brokkr *is* the rerun tool).
fn is_test_phase_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("note: run with `RUST_BACKTRACE=1`")
        || t.starts_with("error: test failed, to rerun pass")
}

/// True for a genuine rustc compile error on stderr. Excludes cargo's
/// test-failure line (`error: test failed, to rerun pass ...`) - that
/// signals a *test* failure/crash, not a build failure, and a hard-crashing
/// test (SIGABRT/stack-overflow) that never prints `test result:` must not be
/// mislabeled `BUILD FAILED`. Cargo emits "test failed" (no "run"), so match
/// that substring, mirroring `is_test_phase_noise`.
fn stderr_indicates_compile_error(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("error[") || (t.starts_with("error:") && !t.contains("test failed"))
}

/// Cargo's per-suite launch line: `Running unittests src/lib.rs
/// (<binary path>)` or `Running tests/bar.rs (<binary path>)`.
///
/// Matched by shape rather than the bare "Running " prefix, so a test's own
/// eprintln! that happens to start with "Running " still passes through. The
/// shape is the *source* target - `unittests <path>.rs` or `<path>.rs` -
/// followed by a parenthesized filesystem path, and deliberately says nothing
/// about where in the target dir that binary lives. It used to require
/// `/deps/`, which silently stopped matching when cargo moved compiled units
/// out of `target/<profile>/deps/` and under `target/<profile>/build/<pkg>/
/// <hash>/out/`. Because this predicate is what flips the stderr forwarder
/// into test-phase mode, a miss meant every line of a whole test run was
/// filtered as if it were still cargo compile chatter: the `Running` lines
/// leaked, `note: run with RUST_BACKTRACE=1` leaked, and a test's own
/// `warning:`-prefixed output was eaten as a compile-warning block.
fn is_cargo_running_line(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("Running ") else {
        return false;
    };
    let Some(inner) = rest.strip_suffix(')') else {
        return false;
    };
    let Some((target, path)) = inner.rsplit_once(" (") else {
        return false;
    };
    let target = target.strip_prefix("unittests ").unwrap_or(target);
    target.ends_with(".rs") && path.contains('/')
}

/// Strip test-harness framing on stdout. The test's own `println!` output,
/// panic messages, and `failures:` sections pass through.
fn keep_stdout_line(line: &str) -> bool {
    if line.starts_with("running ") && line.contains(" test") {
        return false;
    }
    if line.starts_with("test ")
        && (line.ends_with(" ... ok")
            || line.ends_with(" ... FAILED")
            || line.ends_with(" ... ignored"))
    {
        return false;
    }
    if line.starts_with("test result:") {
        return false;
    }
    // Under --nocapture the verdict arrives on its own line after the
    // test's output ("FAILED"/"ok"/"ignored", optional `<X.Xs>` suffix).
    // A test's own bare println!("ok") is indistinguishable and gets
    // dropped from display too - it's still in the captured buffer.
    if test_runner::is_bare_status_line(line) {
        return false;
    }
    true
}

/// Strip cargo's compile-phase chatter on stderr: `Compiling`/`Finished`/
/// `Blocking` progress, `warning:`/`error:` blocks (multi-line, terminated
/// by a blank line), the `N warnings emitted` summary, and rustc's trailing
/// `--explain` hints. Compile errors are still shown via
/// `filter_test_build_failure` in the BUILD FAILED path.
fn keep_stderr_compile_line(line: &str, in_block: &mut bool) -> bool {
    let trimmed = line.trim_start();
    if *in_block {
        if trimmed.is_empty() {
            *in_block = false;
        }
        return false;
    }
    if trimmed.starts_with("warning:")
        || trimmed.starts_with("error:")
        || trimmed.starts_with("error[")
    {
        *in_block = true;
        return false;
    }
    if trimmed.starts_with("Compiling ")
        || trimmed.starts_with("Downloading ")
        || trimmed.starts_with("Checking ")
        || trimmed.starts_with("Finished ")
        || trimmed.starts_with("Blocking ")
    {
        return false;
    }
    if trimmed.contains("generated") && trimmed.contains("warning") {
        return false;
    }
    // Rustc's trailing hints after the error blocks: the `[error]` summary
    // already carries the error codes, so these would leak unprefixed.
    if trimmed.starts_with("Some errors have detailed explanations")
        || trimmed.starts_with("For more information about")
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )]
    use super::*;
    use crate::config::{CheckEntry, SweepProfile, TestConfig};

    /// A one-package selection: package flags as the run's override spells
    /// them.
    fn one_package(pkg: &str) -> check_cmd::Selection {
        check_cmd::Selection::Explicit(vec![pkg.to_owned()])
    }

    /// `brokkr test`'s selection for `pkg`, every sweep resolving to release.
    fn selection_for(sweeps: &[ResolvedSweep], pkg: &str) -> check_cmd::PhaseSelection {
        check_cmd::PhaseSelection::for_brokkr_test(sweeps, pkg, check_cmd::PackageSource::Cli, &|_| false).unwrap()
    }

    /// The sweeps a `brokkr test -p pkg` run would build, in order.
    fn attempted<'a>(sweeps: &'a [ResolvedSweep], pkg: &str) -> Vec<&'a str> {
        let sel = selection_for(sweeps, pkg);
        sweeps
            .iter()
            .zip(sel.entries())
            .filter(|(_, e)| e.attempt().is_some())
            .map(|(s, _)| s.label.as_str())
            .collect()
    }

    #[test]
    fn stdout_filter_strips_test_framing() {
        assert!(!keep_stdout_line("running 1 test"));
        assert!(!keep_stdout_line("running 12 tests"));
        assert!(!keep_stdout_line("test foo ... ok"));
        assert!(!keep_stdout_line("test my_mod::bar ... FAILED"));
        assert!(!keep_stdout_line("test slow_thing ... ignored"));
        assert!(!keep_stdout_line(
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in 0.01s"
        ));
    }

    #[test]
    fn test_phase_noise_strips_backtrace_hint_and_rerun_line() {
        assert!(is_test_phase_noise(
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
        ));
        assert!(is_test_phase_noise(
            "error: test failed, to rerun pass `-p brokkr --bin brokkr`"
        ));
        // Real panic content and other notes survive.
        assert!(!is_test_phase_noise(
            "thread 'foo' panicked at src/x.rs:1:1:"
        ));
        assert!(!is_test_phase_noise("note: something else entirely"));
        assert!(!is_test_phase_noise("error: a genuine test eprintln"));
    }

    #[test]
    fn compile_error_detector_ignores_cargo_test_failure_line() {
        // Cargo's test-failure line is a hard-crash/failure signal, not a
        // build failure - must NOT be classified as a compile error.
        assert!(!stderr_indicates_compile_error(
            "error: test failed, to rerun pass '--bin foo'"
        ));
        // A real rustc error still counts.
        assert!(stderr_indicates_compile_error(
            "error: cannot find value `x` in this scope"
        ));
        assert!(stderr_indicates_compile_error(
            "error[E0425]: cannot find value `x` in this scope"
        ));
    }

    #[test]
    fn repeat_state_first_sighting_only_once_per_signature() {
        let state = RepeatState::default();
        assert!(state.first_sighting("src/a.rs:1:1"));
        assert!(!state.first_sighting("src/a.rs:1:1"));
        // A different signature is its own first sighting.
        assert!(state.first_sighting("src/b.rs:2:2"));
    }

    fn failure(target: &str, what: &str, msg: Option<&str>, loc: Option<&str>) -> Failure {
        Failure {
            kind: FailureKind::Test,
            target: target.to_owned(),
            what: what.to_owned(),
            msg: msg.map(str::to_owned),
            loc: loc.map(str::to_owned),
        }
    }

    fn report(outcome: Outcome, failures: Vec<Failure>) -> RunReport {
        RunReport { outcome, timed_out: false, failures }
    }

    #[test]
    fn repeat_summary_counts_and_groups_by_location() {
        let at = |msg: &str, loc: &str| vec![failure("test:t", "flaky", Some(msg), Some(loc))];
        let reports = vec![
            report(Outcome::Pass, vec![]),
            report(Outcome::Fail, at("rolled 0", "src/a.rs:7:9")),
            report(Outcome::Pass, vec![]),
            // Same location, different message - groups with the first,
            // whose message represents the group.
            report(Outcome::Fail, at("rolled 2", "src/a.rs:7:9")),
            report(Outcome::Fail, at("boom", "src/b.rs:1:1")),
        ];
        let lines = format_repeat_summary(&reports);
        assert_eq!(lines[0], "[test]    summary: 5 runs - 2 PASS, 3 FAIL");
        assert_eq!(lines[1], "[test]      2x [test:t] flaky: rolled 0 @ src/a.rs:7:9");
        assert_eq!(lines[2], "[test]      1x [test:t] flaky: boom @ src/b.rs:1:1");
        assert_eq!(lines.len(), 3);
    }

    /// Runs group on their WHOLE failure set. Keying on the first failure
    /// folded a run that failed A and B into the group of a run that failed A
    /// alone, and the summary never mentioned B.
    #[test]
    fn repeat_summary_groups_on_the_full_failure_set() {
        let a = failure("lib:p", "a", Some("x"), Some("src/a.rs:1:1"));
        let b = failure("test:cli", "b", Some("y"), Some("tests/cli.rs:2:2"));
        let reports = vec![
            report(Outcome::Fail, vec![a.clone()]),
            report(Outcome::Fail, vec![a.clone(), b.clone()]),
            // Same set, reported in the other order: the same group.
            report(Outcome::Fail, vec![b, a]),
        ];
        let lines = format_repeat_summary(&reports);
        assert_eq!(lines[1], "[test]      1x [lib:p] a: x @ src/a.rs:1:1");
        assert_eq!(lines[2], "[test]      2x [lib:p] a: x @ src/a.rs:1:1");
        assert_eq!(lines[3], "[test]         + [test:cli] b: y @ tests/cli.rs:2:2");
        assert_eq!(lines.len(), 4);
    }

    /// The same test name failing in two harnesses is two failures.
    #[test]
    fn a_failure_key_carries_its_harness() {
        let lib = failure("lib:p", "tests::works", None, Some("src/lib.rs:9:9"));
        let cli = failure("test:cli", "tests::works", None, Some("src/lib.rs:9:9"));
        assert_ne!(lib.key(), cli.key());
        assert_ne!(failure_set_key(std::slice::from_ref(&lib)), failure_set_key(&[lib, cli]));
    }

    /// A sweep's iteration fails if any harness failed, and is a SKIP only when
    /// no harness matched - a harness matching nothing is the normal case.
    #[test]
    fn merging_harness_reports_follows_the_worst_outcome() {
        let merged = RunReport::merge(vec![
            report(Outcome::NoMatch, vec![]),
            report(Outcome::Pass, vec![]),
            report(Outcome::NoMatch, vec![]),
        ]);
        assert!(merged.outcome == Outcome::Pass);
        let merged = RunReport::merge(vec![
            report(Outcome::Pass, vec![]),
            report(Outcome::Fail, vec![failure("test:a", "x", None, None)]),
            report(Outcome::BuildFailed, vec![]),
            report(Outcome::Fail, vec![failure("test:b", "y", None, None)]),
        ]);
        assert!(merged.outcome == Outcome::Fail);
        assert_eq!(merged.failures.len(), 2, "every harness's failures survive the merge");
        let merged = RunReport::merge(vec![report(Outcome::NoMatch, vec![])]);
        assert!(merged.outcome == Outcome::NoMatch);
        assert!(RunReport::merge(Vec::new()).outcome == Outcome::NoMatch);
    }

    /// The exit detail is the harness's, from cargo's own line - cargo's 101
    /// says nothing about how the harness died - and the in-flight test is
    /// offered as a suspect, never filed as the failure itself.
    #[test]
    fn a_harness_failure_names_the_signal_and_only_suspects_a_test() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(101 << 8);
        let stderr = [
            "error: test failed, to rerun pass `-p pkg --test cli`",
            "",
            "Caused by:",
            "  process didn't exit successfully: `/t/deps/cli-1 x` (signal: 6, SIGABRT: process abort signal)",
        ];
        let f =
            harness_failure("test:cli", &status, false, &stderr, &["cli::overflows".to_owned()]);
        assert_eq!(f.what, "test harness failed (signal: 6, SIGABRT: process abort signal)");
        let msg = f.msg.expect("a suspect");
        assert!(msg.contains("cli::overflows") && msg.contains("suspect"), "{msg}");

        let bare = harness_failure("test:cli", &status, false, &[], &[]);
        assert_eq!(bare.what, "test harness failed (cargo exited with code 101)");
        assert!(bare.msg.is_none(), "no start seen, no suspect");
    }

    /// A directly run harness's status is its own, so the detail is read
    /// from it - never from cargo lines that a direct run does not have.
    #[test]
    fn a_direct_harness_failure_reads_its_own_status() {
        use std::os::unix::process::ExitStatusExt;
        let aborted = std::process::ExitStatus::from_raw(libc::SIGABRT);
        let f = harness_failure("test:cli", &aborted, true, &[], &[]);
        assert_eq!(f.what, "test harness failed (signal: 6, SIGABRT)");
        let exited = std::process::ExitStatus::from_raw(101 << 8);
        let f = harness_failure("test:cli", &exited, true, &[], &[]);
        assert_eq!(f.what, "test harness failed (exit status: 101)");
    }

    /// Only a single package is ever selected.
    #[test]
    fn a_package_glob_is_refused() {
        assert!(single_package("*".into()).is_err());
        assert!(single_package("pkg-?".into()).is_err());
        assert!(single_package("pkg-[ab]".into()).is_err());
        assert_eq!(single_package("pkg".into()).unwrap(), "pkg");
    }

    /// The prebuild compiles what a named `cargo test -p PKG NAME` compiles -
    /// every test target, not cargo's default set, which would also build
    /// every example.
    #[test]
    fn the_prebuild_selects_the_test_targets() {
        let sweep = ResolvedSweep::default();
        let sel = one_package("pkg");
        let shape = BuildShape { sweep: &sweep, allow_args: &[], selection: &sel, jobs: None, debug: true };
        let pre = prebuild_argv(&shape);
        assert_eq!(
            pre,
            ["test", "--no-run", "--message-format=json-render-diagnostics", "-p", "pkg", "--tests"]
        );
    }

    /// A directly run harness gets the filter first and the focused-run flags;
    /// `--exact` only when the caller resolved a full name.
    #[test]
    fn direct_args_carry_the_filter_and_exact_only_on_request() {
        let plain = direct_libtest_args("some::test", false);
        assert_eq!(
            plain,
            [
                "some::test",
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
                "-Z",
                "unstable-options",
                "--format",
                "json"
            ]
        );
        let exact = direct_libtest_args("some::test", true);
        assert_eq!(exact.last().map(String::as_str), Some("--exact"));
    }

    /// The doc-only run is `cargo test --doc NAME` with the same libtest half.
    #[test]
    fn the_doc_only_run_shares_the_libtest_half() {
        let sweep = ResolvedSweep { doc_only: true, ..ResolvedSweep::default() };
        let sel = one_package("pkg");
        let shape = BuildShape { sweep: &sweep, allow_args: &[], selection: &sel, jobs: None, debug: true };
        let run = test_argv(&shape, "x");
        let sep = run.iter().position(|a| a == "--").expect("separator");
        assert_eq!(&run[..sep], ["test", "-p", "pkg", "--doc", "x"]);
        assert_eq!(&run[sep + 1..], &direct_libtest_args("x", false)[1..]);
    }

    #[test]
    fn repeat_summary_all_pass_has_no_group_lines() {
        let reports = vec![report(Outcome::Pass, vec![]), report(Outcome::Pass, vec![])];
        let lines = format_repeat_summary(&reports);
        assert_eq!(lines, vec!["[test]    summary: 2 runs - 2 PASS, 0 FAIL"]);
    }

    #[test]
    fn repeat_summary_includes_skip_and_build_failed_when_present() {
        let reports = vec![
            report(Outcome::Pass, vec![]),
            report(Outcome::BuildFailed, vec![]),
            report(Outcome::NoMatch, vec![]),
            // Exit-code failure with nothing attributed.
            report(Outcome::Fail, vec![]),
        ];
        let lines = format_repeat_summary(&reports);
        assert_eq!(
            lines[0],
            "[test]    summary: 4 runs - 1 PASS, 1 FAIL, 1 BUILD FAILED, 1 SKIP"
        );
        assert_eq!(lines[1], "[test]      1x unknown failure");
    }

    #[test]
    fn stdout_filter_strips_bare_verdict_lines() {
        // --nocapture puts the verdict on its own line after the test's
        // output; the "test NAME ... FAILED" suffix match never fires.
        assert!(!keep_stdout_line("FAILED"));
        assert!(!keep_stdout_line("ok"));
        assert!(!keep_stdout_line("ignored"));
        assert!(!keep_stdout_line("ok <0.001s>"));
        // Not bare verdicts - real test output survives.
        assert!(keep_stdout_line("FAILED to connect to server"));
        assert!(keep_stdout_line("ok, moving on"));
    }

    fn drive_condenser(lines: &[&str]) -> Vec<String> {
        let mut cond = StdoutCondenser::new();
        lines.iter().flat_map(|l| cond.next(l)).collect()
    }

    #[test]
    fn condenser_collapses_duplicate_failures_headers() {
        // The --nocapture shape: empty output-block section, blank,
        // name-list section. One header survives, glued to the list.
        let out = drive_condenser(&["failures:", "", "failures:", "    my_mod::my_test"]);
        assert_eq!(out, vec!["failures:", "    my_mod::my_test"]);
    }

    #[test]
    fn condenser_drops_dangling_empty_failures_header() {
        // A failures: header with nothing after it (stream ends) is
        // never emitted.
        let out = drive_condenser(&["real output", "failures:", ""]);
        assert_eq!(out, vec!["real output"]);
    }

    #[test]
    fn condenser_collapses_blank_runs_and_leading_blanks() {
        let out = drive_condenser(&["", "", "a", "", "", "b"]);
        assert_eq!(out, vec!["a", "", "b"]);
    }

    /// Libtest's framing is dropped unconditionally now that there is no
    /// flag to keep it: the test's own output is the signal, and the
    /// verdict lines are what the `[test]` footer already says.
    #[test]
    fn condenser_drops_framing_and_bare_verdicts() {
        let out = drive_condenser(&["test result: ok. 1 passed", "FAILED", "real output"]);
        assert_eq!(out, vec!["real output"]);
    }

    #[test]
    fn stdout_filter_keeps_test_output() {
        assert!(keep_stdout_line("hello from test"));
        assert!(keep_stdout_line(""));
        assert!(keep_stdout_line("thread 'foo' panicked at tests/bar.rs:10:5:"));
        assert!(keep_stdout_line("assertion `left == right` failed"));
        assert!(keep_stdout_line("failures:"));
        assert!(keep_stdout_line("---- foo stdout ----"));
        // Messages that start with "test" but aren't framing must survive -
        // a user's println! starting with "test" wouldn't match the exact
        // " ... ok" / "... FAILED" / "... ignored" suffixes.
        assert!(keep_stdout_line("test the things now"));
    }

    #[test]
    fn cargo_running_line_matches_suite_launch_shapes() {
        assert!(is_cargo_running_line(
            "     Running unittests src/bin/bench.rs (/media/folk/Banan/cargo/debug/deps/bench-d5eda320d87aa0a1)"
        ));
        assert!(is_cargo_running_line(
            "     Running tests/montecarlo_threads.rs (/x/target/debug/deps/montecarlo_threads-dcc49cf)"
        ));
    }

    #[test]
    fn cargo_running_line_matches_the_depsless_target_layout() {
        // Cargo no longer puts compiled units under `target/<profile>/deps/`;
        // a `/deps/` substring requirement made this predicate - and with it
        // the whole test-phase stderr filter - dead on such a toolchain.
        assert!(is_cargo_running_line(
            "     Running unittests src/lib.rs (target/debug/build/broadarrow-daemon/87f43d5b0d3f3d44/out/broadarrow_daemon-87f43d5b0d3f3d44)"
        ));
        assert!(is_cargo_running_line(
            "     Running tests/boot_event_stream.rs (target/unify-package/debug/build/broadarrow-daemon/77ac71235dfced6c/out/boot_event_stream-77ac71235dfced6c)"
        ));
    }

    #[test]
    fn cargo_running_line_spares_test_output() {
        // A test's own eprintln! starting with "Running " lacks the
        // `<target>.rs (<path>)` shape and must pass through.
        assert!(!is_cargo_running_line("Running 500 monte carlo paths"));
        assert!(!is_cargo_running_line("Running phase 2 (warmup)"));
        assert!(!is_cargo_running_line("   Compiling brokkr v0.1.0"));
        // Parenthesized, but the pre-paren token is prose, not a source file.
        assert!(!is_cargo_running_line("Running the sweep (a/b/c)"));
        // A source-file-shaped target with no path in the parens.
        assert!(!is_cargo_running_line("Running tests/foo.rs (cached)"));
    }

    #[test]
    fn stderr_filter_strips_compile_progress() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "   Compiling brokkr v0.1.0",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "   Downloading crates ...",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "    Checking serde v1.0.0",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "    Finished `release` profile [optimized] target(s) in 45.13s",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "    Blocking waiting for file lock on build directory",
            &mut in_block
        ));
        assert!(!in_block);
    }

    #[test]
    fn stderr_filter_strips_warning_block() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "warning: unused variable: `x`",
            &mut in_block
        ));
        assert!(in_block);
        assert!(!keep_stderr_compile_line("  --> src/lib.rs:10:5", &mut in_block));
        assert!(!keep_stderr_compile_line("   |", &mut in_block));
        assert!(!keep_stderr_compile_line("10 | let x = 1;", &mut in_block));
        assert!(!keep_stderr_compile_line(
            "   |     ^ help: rename to _x",
            &mut in_block
        ));
        // Blank line terminates the block.
        assert!(!keep_stderr_compile_line("", &mut in_block));
        assert!(!in_block);
        // Normal content after the block passes through again.
        assert!(keep_stderr_compile_line("real test output", &mut in_block));
    }

    #[test]
    fn stderr_filter_strips_error_block() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "error[E0425]: cannot find value `foo`",
            &mut in_block
        ));
        assert!(in_block);
        assert!(!keep_stderr_compile_line("  --> src/lib.rs:1:1", &mut in_block));
        assert!(!keep_stderr_compile_line("", &mut in_block));
        assert!(!in_block);
        // `error:` (no brackets) also triggers the block.
        assert!(!keep_stderr_compile_line(
            "error: aborting due to previous error",
            &mut in_block
        ));
    }

    #[test]
    fn stderr_filter_strips_warning_summary_line() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "warning: `pbfhogg` (lib) generated 3 warnings",
            &mut in_block
        ));
        // The summary line triggers a block because it starts with `warning:`,
        // but the very next blank line closes it so subsequent content flows.
        assert!(in_block);
        assert!(!keep_stderr_compile_line("", &mut in_block));
        assert!(!in_block);
    }

    #[test]
    fn stderr_filter_keeps_non_compile_content() {
        let mut in_block = false;
        assert!(keep_stderr_compile_line(
            "some random line that isn't cargo",
            &mut in_block
        ));
        // Blank lines when not inside a block pass through - a blank line
        // between real output shouldn't be silently swallowed.
        assert!(keep_stderr_compile_line("", &mut in_block));
    }

    #[test]
    fn decide_sweeps_no_config_returns_legacy_default() {
        // No `[test]`, no `[[check]]` - the project hasn't migrated.
        // Single `--all-features` sweep, matching pre-redesign behaviour.
        let sweeps = decide_sweeps(None, &[]).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--all-features"]);
        assert!(sweeps[0].build_packages.is_empty());
    }

    #[test]
    fn decide_sweeps_iterates_check_entries_when_no_default_profile() {
        // `[[check]]` configured, but no default_profile - every entry
        // runs in declaration order.
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["test-hooks".into(), "linux-direct-io".into()],
                no_default_features: false,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
        ];
        let sweeps = decide_sweeps(None, &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(
            sweeps[0].cargo_feature_args,
            vec!["--features", "test-hooks,linux-direct-io"]
        );
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(
            sweeps[1].cargo_feature_args,
            vec!["--no-default-features", "--features", "commands"]
        );
    }

    #[test]
    fn decide_sweeps_uses_default_profile_when_set() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all", "consumer"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["a".into()],
                no_default_features: false,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
        ];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(sweeps[1].build_packages, vec!["pbfhogg-cli"]);
    }

    /// The lanes fixture shared by the dedupe / selection tests: one
    /// `[[check]]` entry (`all`) referenced by two lanes, whose only
    /// difference is a dropped libtest filter - so both lanes share a build
    /// shape.
    fn lanes_cfg() -> (TestConfig, Vec<CheckEntry>) {
        let toml_text = r#"
default_profile = "pre-commit"

[profiles.tier1]
sweeps = ["all"]
skip = ["serial::"]

[profiles.serial]
sweeps = ["all"]
only = ["serial::"]
test_threads = 1

[profiles.pre-commit]
lanes = ["tier1", "serial"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        (test_cfg, entries)
    }

    #[test]
    fn decide_sweeps_keeps_every_lane_undeduped() {
        // decide_sweeps does not dedupe: it hands `run` the full lane list so
        // `--sweep` can see both lane-qualified labels. The collapse is the
        // run's selection's job, after `--sweep`.
        let (test_cfg, entries) = lanes_cfg();
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "tier1/all");
        assert_eq!(sweeps[1].label, "serial/all");
        assert_eq!(resolve_sweeps(Some(&test_cfg), &entries, None).unwrap().len(), 2);
    }

    #[test]
    fn the_selection_collapses_lanes_that_execute_identically() {
        // With the user's <name> as the only filter, the two lanes' runs are
        // identical, so the selection attempts the first and dedupes the
        // second onto it.
        let (test_cfg, entries) = lanes_cfg();
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(attempted(&sweeps, "pkg"), vec!["tier1/all"]);
        let sel = selection_for(&sweeps, "pkg");
        assert!(matches!(sel.entry(1), Some(LaneEntry::Deduped(d)) if d.onto() == 0));
        assert_eq!(
            sweep_plan_line(&sweeps, &sel, "pkg"),
            "[test]    1 sweep for pkg: tier1/all (deduped: serial/all (as tier1/all))"
        );
    }

    #[test]
    fn select_then_dedupe_resolves_lane_qualified_label() {
        // S3-07: `--sweep serial/all` must resolve even though serial/all
        // executes what tier1/all does and would be deduped away. Selecting
        // before deduping is what makes the documented lane-qualified label
        // reachable.
        let (test_cfg, entries) = lanes_cfg();
        let sweeps = resolve_sweeps(Some(&test_cfg), &entries, Some("serial/all")).unwrap();
        assert_eq!(attempted(&sweeps, "pkg"), vec!["serial/all"]);
    }

    #[test]
    fn the_selection_spares_sweeps_differing_only_in_test_exclusions() {
        // S3-08: two lanes with the same build shape but different
        // `test_exclude_packages` are NOT identical `brokkr test` runs - one
        // skips the `-p` target, the other runs it. The selection keeps the
        // permitting one, so declaration order can't decide whether
        // `-p PKG NAME` finds its test.
        let excludes = ResolvedSweep {
            label: "tier1/all".into(),
            cargo_feature_args: vec!["--features".into(), "a".into()],
            test_exclude_packages: vec!["pkg-x".into()],
            ..Default::default()
        };
        let permits = ResolvedSweep {
            label: "serial/all".into(),
            cargo_feature_args: vec!["--features".into(), "a".into()],
            test_exclude_packages: Vec::new(),
            ..Default::default()
        };
        let sweeps = vec![excludes, permits];
        assert_eq!(attempted(&sweeps, "pkg-x"), vec!["serial/all"]);
        let sel = selection_for(&sweeps, "pkg-x");
        assert!(matches!(sel.entry(0), Some(LaneEntry::Excluded(_))));
    }

    /// Regression: a sweep and its doctest twin are two runs. The old dedupe
    /// key was the build shape, which leaves `doc_only` out, so the twin
    /// collapsed onto its sibling and `brokkr test NAME` never searched the
    /// doctests.
    #[test]
    fn a_doctest_twin_is_not_collapsed_onto_its_sibling() {
        let lib = ResolvedSweep { label: "default".into(), ..Default::default() };
        let twin = ResolvedSweep { label: "doctests".into(), doc_only: true, ..Default::default() };
        let sweeps = vec![lib, twin];
        assert_eq!(attempted(&sweeps, "pkg"), vec!["default", "doctests"]);
    }

    #[test]
    fn decide_sweeps_carries_profile_env_through() {
        // B3 regression: a profile that exports `env = { FOO = "1" }`
        // used to round-trip through `brokkr check` but get silently
        // dropped on `brokkr test`. After consolidation, both paths
        // share decide_active_sweeps and env is preserved.
        let toml_text = r#"
default_profile = "platform"

[profiles.platform]
sweeps = ["all"]
include_ignored = true
env = { BROKKR_TEST_PLATFORM = "1", FOO = "bar" }
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(
            sweeps[0].env.get("BROKKR_TEST_PLATFORM").map(String::as_str),
            Some("1")
        );
        assert_eq!(sweeps[0].env.get("FOO").map(String::as_str), Some("bar"));
        // libtest filters dropped (per `brokkr test` design).
        assert!(sweeps[0].libtest_args.is_empty());
        assert!(sweeps[0].cargo_test_filters.is_empty());
        assert!(sweeps[0].name_filters.is_empty());
    }

    #[test]
    fn decide_sweeps_default_profile_filters_dropped() {
        // `brokkr test <name>` uses the user's name as the filter; any
        // `only` / `skip` / `tests` / `include_ignored` / `test_threads`
        // declared by the profile is intentionally dropped (mixing them
        // with `<name>` caused silent zero-match failures).
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["tier2::"]
include_ignored = false
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 1);
        // Sweep struct only carries label / feature_args / build_packages -
        // any libtest filter from the profile is intentionally absent.
        assert_eq!(sweeps[0].label, "all");
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A filter every test name contains is a whole-suite run in disguise; one
    /// that leaves even a single test out is a selection.
    #[test]
    fn a_filter_matching_every_test_is_whole_suite() {
        let listed = names(&[
            "disclose::tests::mutes_a",
            "disclose::tests::mutes_b",
            "market::tests::routes_c",
            "market::tests::routes_d",
            "run_prep_e",
        ]);
        for broad in ["_", "e", ""] {
            assert!(matches_whole_suite(&listed, broad), "{broad:?} matches every test");
        }
        for narrow in ["::", "tests::", "market::", "run_prep_e", "nothing"] {
            assert!(!matches_whole_suite(&listed, narrow), "{narrow:?} is a selection");
        }
    }

    /// Below the floor a match-everything name is a plausible "run this small
    /// crate's tests", so it runs.
    #[test]
    fn a_package_below_the_floor_is_never_whole_suite() {
        let listed = names(&["a::x", "a::y", "a::z", "a::w"]);
        assert!(listed.len() < WHOLE_SUITE_FLOOR);
        assert!(!matches_whole_suite(&listed, "a::"));
        assert!(!matches_whole_suite(&[], "anything"), "an empty package matches nothing");
    }

    /// The pre-build compiles the lane like the test run does: the pinned
    /// unification and the `[lints] allow` `--config` share both ride it, or a
    /// package-mode lane pre-builds under ambient resolution and a denied lint
    /// fails the pre-build before `cargo test` is reached.
    #[test]
    fn pre_build_carries_the_unification_pin_and_the_allow_config() {
        use crate::config::{CargoUnification, EffectiveUnification};
        let sweep = ResolvedSweep {
            packages: vec!["daemon".to_owned()],
            effective_unification: EffectiveUnification::Pinned(CargoUnification::Package),
            ..ResolvedSweep::default()
        };
        let allow = vec!["--config".to_owned(), "build.rustflags=[\"-A\",\"x\"]".to_owned()];
        let args = pre_build_argv(&sweep, "daemon", &allow, false);
        assert!(
            args.iter().any(|a| a == "resolver.feature-unification=\"package\""),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "build.rustflags=[\"-A\",\"x\"]"), "{args:?}");
        assert!(args.iter().any(|a| a == "--release"), "{args:?}");

        // An unpinned sweep with no allows stays byte-identical to before.
        let plain = pre_build_argv(&ResolvedSweep::default(), "bin", &[], true);
        assert_eq!(plain, ["build", "--message-format=json-render-diagnostics", "-p", "bin"]);
    }

    fn ready_sweep<'s>(
        sweep: &'s ResolvedSweep,
        harness: &check_cmd::TestBinary,
        support: &[check_cmd::SupportArtifact],
    ) -> ReadySweep<'s> {
        let hash = test_runner::hash_file(Path::new(&harness.executable)).unwrap();
        let attempt = selection_for(std::slice::from_ref(sweep), "core").attempt(0).unwrap().clone();
        ReadySweep {
            sweep,
            attempt,
            debug: true,
            env: Vec::new(),
            allow_args: Vec::new(),
            focused: Some(FocusedPlan {
                runs: Vec::new(),
                searched: 1,
                exact: None,
                artifacts: vec![check_cmd::PlannedArtifact {
                    resolution: None,
                    unit: check_cmd::BinaryUnit::of(harness),
                    executable: harness.executable.clone(),
                    hash,
                }],
            }),
            exact: None,
            support_fingerprint: check_cmd::support_fingerprint(support).unwrap(),
            runtime_fingerprint: check_cmd::BuildRuntimeIndex::default().fingerprint().unwrap(),
        }
    }

    /// Validation case: preparing every sweep up front must not let one
    /// sweep's build stand in for another's. Two feature sweeps share one
    /// `build_packages` executable path, so the second sweep's preparation
    /// overwrote the first's - and the first sweep's tests then spawned the
    /// second sweep's CLI. Each sweep is held to its prepared record when its
    /// turn comes: a clean rebuild is unchanged, a support binary another
    /// sweep replaced is drift, and so is a harness rebuilt with other content.
    #[test]
    fn a_sweep_is_held_to_its_prepared_record_before_it_runs() {
        let dir = crate::test_scratch::scratch("test_cmd", "sweep_drift");
        let server = dir.join("server");
        std::fs::write(&server, b"first sweep's server").unwrap();
        let harness_path = dir.join("suite-1");
        std::fs::write(&harness_path, b"suite").unwrap();
        let mut harness = check_cmd::test_binary_for_tests("core", "test", "suite");
        harness.executable = harness_path.to_string_lossy().into_owned();
        let support = vec![check_cmd::SupportArtifact {
            package: "server".into(),
            target: "server".into(),
            executable: server.to_string_lossy().into_owned(),
        }];
        let sweep = ResolvedSweep::default();
        let prepared = ready_sweep(&sweep, &harness, &support);
        let rebuild = |_: usize| Ok(Some((vec![harness.clone()], check_cmd::BuildRuntimeIndex::default())));

        assert!(sweep_drift(&prepared, rebuild, &support).unwrap().unwrap().is_empty(), "an untouched sweep");

        // The second sweep's build re-uplifted the shared support binary.
        std::fs::write(&server, b"second sweep's server").unwrap();
        let drift = sweep_drift(&prepared, rebuild, &support).unwrap().unwrap();
        assert!(drift.iter().any(|d| d.contains("support build")), "{drift:?}");
        // Rebuilding restores it, and the sweep verifies again.
        std::fs::write(&server, b"first sweep's server").unwrap();
        assert!(sweep_drift(&prepared, rebuild, &support).unwrap().unwrap().is_empty());

        // A harness rebuilt with other content.
        std::fs::write(&harness_path, b"another sweep's suite").unwrap();
        let drift = sweep_drift(&prepared, rebuild, &support).unwrap().unwrap();
        assert!(drift.iter().any(|d| d.contains("rebuilt with different content")), "{drift:?}");

        // A rebuild that fails is not drift, it is a failed build.
        assert!(sweep_drift(&prepared, |_| Ok(None), &support).unwrap().is_none());

        // A doc-only sweep has no harness to rebuild but still has its support.
        std::fs::write(&harness_path, b"suite").unwrap();
        let mut doc = ready_sweep(&sweep, &harness, &support);
        doc.focused = None;
        assert!(sweep_drift(&doc, |_| Ok(None), &support).unwrap().unwrap().is_empty());
        std::fs::write(&server, b"second sweep's server").unwrap();
        assert!(!sweep_drift(&doc, |_| Ok(None), &support).unwrap().unwrap().is_empty());
    }

    /// Wiring: `run_ready` itself rebuilds and re-verifies each sweep BEFORE
    /// it executes anything, through the real `settle_sweep` and
    /// `recheck_sweep`. The sweep here is doc-only (so the recheck builds
    /// nothing) with a recorded support fingerprint the rebuild no longer
    /// produces - drift - and runs under `--timeout`, so the re-preparation it
    /// triggers ends in the doc-only refusal without cargo. (Only an attempted
    /// sweep is ever prepared, so the refusal is the cargo-free end of a
    /// re-preparation; the excluded-sweep skip it used to end in now happens
    /// before preparation.) If `run_ready` stopped calling `settle_sweep`, the
    /// sweep would go straight to `run_iteration` (a `cargo test --doc` in a
    /// directory with no project) and the error would not be the refusal.
    #[test]
    fn run_ready_rechecks_a_sweep_before_it_runs() {
        let dir = crate::test_scratch::scratch("test_cmd", "run_ready_recheck");
        let server = dir.join("server");
        std::fs::write(&server, b"the planned server").unwrap();
        let support = vec![check_cmd::SupportArtifact {
            package: "server".into(),
            target: "server".into(),
            executable: server.to_string_lossy().into_owned(),
        }];
        let sweep = ResolvedSweep { label: "docs".into(), doc_only: true, ..ResolvedSweep::default() };
        let mut harness = check_cmd::test_binary_for_tests("core", "test", "suite");
        harness.executable = server.to_string_lossy().into_owned();
        let mut prepared = ready_sweep(&sweep, &harness, &support);
        prepared.focused = None;
        let target_dir = dir.join("target");
        let cx = SweepContext {
            project: Project::Brokkr,
            project_root: &dir,
            state_root: &dir,
            test_cfg: None,
            target_dir: &target_dir,
            allow_flags: &[],
            name: "t",
            jobs: None,
            multi: false,
            profile_override: None,
            timeout: Some(30),
        };
        let mut reports = Vec::new();
        let mut notes = Vec::new();
        let err = run_ready(std::slice::from_ref(&prepared), &cx, 1, Duration::from_secs(20), None, &mut reports, &mut notes)
            .unwrap_err()
            .to_string();
        assert!(err.contains("doc-only sweep 'docs'"), "the drifted sweep was re-prepared, not run: {err}");
        assert!(reports.is_empty());
    }

    #[test]
    fn resolve_debug_cli_override_wins_over_config() {
        let debug_cfg = TestConfig {
            debug: true,
            ..Default::default()
        };
        // --release (Some(false)) beats `[test] debug = true`.
        assert!(!resolve_debug(Some(false), Some(&debug_cfg), None));
        // --debug (Some(true)) holds even when config says release.
        assert!(resolve_debug(Some(true), Some(&TestConfig::default()), None));
        // The CLI also beats a sweep's own pinned profile - the
        // per-invocation answer is the most specific one there is.
        assert!(resolve_debug(
            Some(true),
            None,
            Some(SweepProfile::Release)
        ));
        assert!(!resolve_debug(Some(false), None, Some(SweepProfile::Dev)));
    }

    #[test]
    fn a_sweeps_profile_beats_the_project_wide_default() {
        // The case the key exists for: a repo pins the fast dev build
        // project-wide, and one sweep holds a wall-clock contract that only
        // means anything optimized. `brokkr test <that test>` must run it
        // release without the caller remembering `--release`.
        let debug_cfg = TestConfig {
            debug: true,
            ..Default::default()
        };
        assert!(!resolve_debug(
            None,
            Some(&debug_cfg),
            Some(SweepProfile::Release)
        ));
        // And the mirror: brokkr's own default is release, so a sweep that
        // pins dev (for compile speed) gets dev with no flag either.
        assert!(resolve_debug(
            None,
            Some(&TestConfig::default()),
            Some(SweepProfile::Dev)
        ));
    }

    #[test]
    fn resolve_debug_falls_back_to_config_then_release() {
        let debug_cfg = TestConfig {
            debug: true,
            ..Default::default()
        };
        // No CLI flag, no sweep profile: `[test] debug = true` decides.
        assert!(resolve_debug(None, Some(&debug_cfg), None));
        // No CLI flag, config defaults to release.
        assert!(!resolve_debug(None, Some(&TestConfig::default()), None));
        // No CLI flag, no `[test]` section at all -> release.
        assert!(!resolve_debug(None, None, None));
    }

    #[test]
    fn aggregate_exit_fails_on_any_fail() {
        let outcomes = [Outcome::Pass, Outcome::Fail];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default()).is_err());
    }

    #[test]
    fn aggregate_exit_fails_on_any_build_failed() {
        let outcomes = [Outcome::Pass, Outcome::BuildFailed];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default()).is_err());
    }

    #[test]
    fn aggregate_exit_fails_when_all_no_match() {
        let outcomes = [Outcome::NoMatch, Outcome::NoMatch];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default()).is_err());
    }

    #[test]
    fn the_no_match_error_names_what_was_searched() {
        let message = |searched: &Searched| match aggregate_exit(&[Outcome::NoMatch], "pkg", "abc", searched) {
            Err(DevError::Reported(m)) => m,
            other => panic!("expected a Reported error, got {other:?}"),
        };
        let two = Searched {
            harnesses: 44,
            doc_only: 0,
            labels: vec!["default".into(), "strategy-features".into()],
        };
        assert_eq!(
            message(&two),
            "no test matches `abc` in pkg (44 harnesses searched in default and strategy-features)"
        );
        let three = Searched { labels: vec!["a".into(), "b".into(), "c".into()], ..two };
        assert!(message(&three).ends_with("(44 harnesses searched in a, b and c)"));
        // A single sweep has no plan line, so its search must name it too.
        let one = Searched { harnesses: 1, doc_only: 0, labels: vec!["default".into()] };
        assert_eq!(message(&one), "no test matches `abc` in pkg (1 harness searched in default)");
        // A doc-only sweep searches doctests, not countable harnesses.
        let doc = Searched { harnesses: 0, doc_only: 1, labels: vec!["docs".into()] };
        assert_eq!(message(&doc), "no test matches `abc` in pkg (doctests searched in docs)");
        // A lone sweep scoped out of the package searched nothing, and said why itself.
        assert_eq!(message(&Searched::default()), "no test matches `abc` in pkg");
    }

    #[test]
    fn only_a_failing_run_shows_its_invocation() {
        let state = RepeatState::default();
        let sweep = ResolvedSweep::default();
        let sel = one_package("pkg");
        let plan = SweepPlan {
            shape: BuildShape { sweep: &sweep, allow_args: &[], selection: &sel, jobs: None, debug: true },
            name: "n",
            focused: None,
            exact: None,
            env: &[],
            project_root: Path::new("."),
            state_root: Path::new("."),
            ceiling: Duration::from_secs(20),
            sweep_label: None,
            repeat: 1,
            repeat_state: &state,
        };
        // The dedupe set is what decides a console print: a pass never enters
        // it, a failure enters it once.
        show_reproduction(&plan, "bin args (cwd .)", &RunReport::bare(Outcome::Pass));
        assert!(state.first_hint("bin args (cwd .)"), "a pass left no mark");
        show_reproduction(&plan, "other args (cwd .)", &RunReport::bare(Outcome::Fail));
        assert!(!state.first_hint("other args (cwd .)"), "a failure printed (and was recorded)");
        show_reproduction(&plan, "cargo test --doc", &RunReport::bare(Outcome::BuildFailed));
        assert!(!state.first_hint("cargo test --doc"), "a doc-only build failure printed its command");
        show_failed_invocation(&plan, "missing-bin args (cwd .)");
        assert!(!state.first_hint("missing-bin args (cwd .)"), "a runner error printed its command");
    }

    #[test]
    fn hung_test_tag_requires_a_resolved_test_and_a_per_test_reason() {
        use test_runner::{Ceilings, HungTest, TimeoutReason};

        let render = |name: &str, wall: &str| format!("pkg::{name} ({wall})");
        let mut shape = OneRun {
            tag: "pkg::filter",
            test_tag: Some(&render),
            target: "lib",
            ceilings: Ceilings::one_test(Duration::from_secs(20), "tests::hang"),
            announce: false,
            expected: Some(1),
            listed: None,
            observe: test_runner::Observe::default(),
        };
        let mut hung = HungTest {
            reason: TimeoutReason::PerTest { name: "tests::hang".into() },
            test: "tests::hang".into(),
            elapsed: Duration::from_secs(20),
            ceiling: Duration::from_secs(20),
            snapshot_dir: PathBuf::new(),
            cargo_pid: 0,
            test_pids: Vec::new(),
            snapshot_pid: None,
            wchan: None,
            stack: None,
            snapshot_error: None,
        };
        // The runner uses PerTest for both the resolved test's tracker expiry
        // and its invocation's wall expiry, even without a start event.
        assert_eq!(
            hung_test_tag(&shape, &hung).map(|tag| tag(&hung.test, "20.00s")),
            Some("pkg::tests::hang (20.00s)".into())
        );
        shape.ceilings = Ceilings::shared_harness();
        assert!(hung_test_tag(&shape, &hung).is_none(), "a suspect is not a verified name");
        shape.ceilings = Ceilings::one_test(Duration::from_secs(20), "tests::hang");
        hung.reason = TimeoutReason::SweepWall { blamed: vec!["tests::hang".into()] };
        assert!(hung_test_tag(&shape, &hung).is_none());
        hung.reason = TimeoutReason::Idle;
        assert!(hung_test_tag(&shape, &hung).is_none());
        hung.reason = TimeoutReason::PerTest { name: "tests::hang".into() };
        shape.test_tag = None;
        assert!(hung_test_tag(&shape, &hung).is_none(), "no renderer in a doc-only run");
    }

    #[test]
    fn aggregate_exit_label_names_what_failed() {
        let label = |outcomes: &[Outcome]| match aggregate_exit(outcomes, "f", "n", &Searched::default()) {
            Err(DevError::Reported(label)) => label,
            other => panic!("expected a Reported error, got {other:?}"),
        };
        assert_eq!(label(&[Outcome::Pass, Outcome::BuildFailed]), "build failed");
        assert_eq!(label(&[Outcome::Fail, Outcome::NoMatch]), "test failed");
        assert_eq!(
            label(&[Outcome::Fail, Outcome::BuildFailed]),
            "test failed and build failed"
        );
        // The no-match fact is stated once, by the closing label.
        assert!(label(&[Outcome::NoMatch]).starts_with("no test matches `n` in f"));
    }

    #[test]
    fn aggregate_exit_passes_when_any_pass_with_no_match() {
        // The important case: feature-gated test passes in one sweep, SKIPs
        // in the consumer sweep. Exit code should be 0.
        let outcomes = [Outcome::Pass, Outcome::NoMatch];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default()).is_ok());
    }

    #[test]
    fn aggregate_exit_passes_on_all_pass() {
        let outcomes = [Outcome::Pass, Outcome::Pass];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default()).is_ok());
    }

    /// The one exclusion note of `sweep` for `pkg`, or `None` when the
    /// selection attempts it.
    fn exclusion_of(sweep: &ResolvedSweep, pkg: &str) -> Option<check_cmd::AdmissionRule> {
        match selection_for(std::slice::from_ref(sweep), pkg).entry(0) {
            Some(LaneEntry::Excluded(e)) => exclusion_rule(e.notes()),
            _ => None,
        }
    }

    #[test]
    fn a_sweep_with_no_packages_list_admits_any_target() {
        // A sweep with no `packages` list covers every target.
        let sweep = ResolvedSweep::default();
        assert!(exclusion_of(&sweep, "nautilus-hyperliquid").is_none());
        // And the target replaces the sweep's own selection.
        let sel = selection_for(std::slice::from_ref(&sweep), "nautilus-hyperliquid");
        assert_eq!(
            check_cmd::package_args(sel.attempt(0).unwrap().selection()),
            ["-p", "nautilus-hyperliquid"]
        );
    }

    #[test]
    fn a_target_outside_the_packages_list_is_excluded() {
        // The reported case: an ffi/live sweep lists only its member packages;
        // `-p nautilus-hyperliquid` isn't one, so it must SKIP rather than
        // force a build of a feature the package doesn't declare.
        let sweep = ResolvedSweep {
            packages: vec!["nautilus-core".into(), "nautilus-common".into()],
            ..Default::default()
        };
        assert_eq!(
            exclusion_of(&sweep, "nautilus-hyperliquid"),
            Some(check_cmd::AdmissionRule::NotListed)
        );
    }

    #[test]
    fn a_target_inside_the_packages_list_is_attempted() {
        let sweep = ResolvedSweep {
            packages: vec!["nautilus-core".into(), "nautilus-hyperliquid".into()],
            ..Default::default()
        };
        assert_eq!(attempted(std::slice::from_ref(&sweep), "nautilus-hyperliquid").len(), 1);
    }

    #[test]
    fn the_plan_line_keeps_the_two_scope_reasons_apart() {
        let sweeps = vec![
            ResolvedSweep { label: "default".into(), ..Default::default() },
            ResolvedSweep { label: "vm".into(), packages: vec!["other".into()], ..Default::default() },
            ResolvedSweep {
                label: "ffi".into(),
                test_exclude_packages: vec!["pkg".into()],
                ..Default::default()
            },
        ];
        assert_eq!(
            sweep_plan_line(&sweeps, &selection_for(&sweeps, "pkg"), "pkg"),
            "[test]    1 sweep for pkg: default (not this package: vm; excluded by \
             test_exclude_packages: ffi)"
        );
    }

    #[test]
    fn an_excluded_target_is_excluded_by_test_exclude_packages() {
        // An otherwise-workspace-wide sweep carving the target out.
        let sweep = ResolvedSweep {
            test_exclude_packages: vec!["nautilus-pyo3".into()],
            ..Default::default()
        };
        assert_eq!(
            exclusion_of(&sweep, "nautilus-pyo3"),
            Some(check_cmd::AdmissionRule::TestExcluded)
        );
        assert_eq!(
            exclusion_describe(&[check_cmd::AdmissionNote {
                package: "nautilus-pyo3".into(),
                rule: check_cmd::AdmissionRule::TestExcluded
            }]),
            "package excluded from this sweep (test_exclude_packages)"
        );
    }
}
