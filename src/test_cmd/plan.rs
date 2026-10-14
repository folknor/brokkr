//! Sweep selection, cargo build shapes, and harness discovery.

use std::path::{Path, PathBuf};

use crate::cargo_filter;
use crate::check_cmd::{self, LaneEntry};
use crate::config::{self, DevConfig};
use crate::error::DevError;
use crate::output;
use crate::profile::PlannedSweep;
use crate::project::Project;
use crate::rustflags;
use crate::test_runner;

use super::focused;
use super::verdict::{Outcome, RunReport};

/// The whole run's shape, printed up front: the sweeps the run is eligible to
/// go through, so a PASS in the first one is visibly not the end of the
/// command. Rendered from the run's selection.
///
/// It is also the only place a scope-excluded or deduped sweep is named -
/// those print no SKIP line of their own - so it keeps the reasons apart: a
/// sweep whose `packages` list leaves the package out, one that excludes it by
/// `test_exclude_packages`, and one that would run exactly what an earlier
/// sweep runs.
pub(super) fn sweep_plan_line(
    sweeps: &[PlannedSweep],
    selection: &check_cmd::PhaseSelection,
    pkg: &str,
) -> String {
    let mut eligible: Vec<&str> = Vec::new();
    let mut not_listed: Vec<&str> = Vec::new();
    let mut excluded: Vec<&str> = Vec::new();
    let mut deduped: Vec<String> = Vec::new();
    for (s, entry) in sweeps.iter().zip(selection.entries().iter()) {
        match entry {
            LaneEntry::Attempt(_) => eligible.push(&s.label),
            LaneEntry::Excluded(e) => match exclusion_rule(e.notes()) {
                Some(check_cmd::AdmissionRule::TestExcluded) => excluded.push(&s.label),
                _ => not_listed.push(&s.label),
            },
            LaneEntry::Deduped(d) => {
                let onto = sweeps.get(d.onto().get()).map_or("?", |o| o.label.as_str());
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
        out.push(format!(
            "excluded by test_exclude_packages: {}",
            excluded.join(", ")
        ));
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
pub(super) fn exclusion_rule(
    notes: &[check_cmd::AdmissionNote],
) -> Option<check_cmd::AdmissionRule> {
    notes.first().map(|n| n.rule)
}

/// The lone-sweep SKIP line's wording for an exclusion.
pub(super) fn exclusion_describe(notes: &[check_cmd::AdmissionNote]) -> &'static str {
    match exclusion_rule(notes) {
        Some(check_cmd::AdmissionRule::TestExcluded) => {
            "package excluded from this sweep (test_exclude_packages)"
        }
        _ => "package not in this sweep's packages list",
    }
}

/// What one sweep came to before any test ran.
pub(super) enum SweepOutcome<'s> {
    /// Built, discovered and ready to run.
    Ready(Box<ReadySweep<'s>>),
    /// Nothing to run: skipped, or its build failed.
    Done(RunReport),
}

/// One sweep, built and discovered: everything its runs share, owned, so
/// every sweep can be prepared before the first one runs.
pub(super) struct ReadySweep<'s> {
    pub(super) sweep: &'s PlannedSweep,
    /// The sweep's attempt in the run's selection: its package selection.
    pub(super) attempt: check_cmd::Attempt,
    pub(super) debug: bool,
    pub(super) env: Vec<(String, String)>,
    pub(super) allow_args: Vec<String>,
    /// The harnesses holding a match, run directly; `None` for a doc-only
    /// sweep's single `cargo test --doc`.
    pub(super) focused: Option<FocusedPlan>,
    /// The resolved full name `--timeout` runs with `--exact`.
    pub(super) exact: Option<String>,
    pub(super) support_fingerprint: Vec<String>,
    pub(super) runtime_fingerprint: Vec<String>,
}

/// Build one attempted sweep's support binaries and test harnesses and
/// discover the harnesses holding a match. Only an attempt reaches here: a
/// sweep the selection excludes or dedupes builds nothing.
pub(super) fn prepare_sweep<'s>(
    sweep: &'s PlannedSweep,
    attempt: &check_cmd::Attempt,
    cx: &SweepContext<'_>,
) -> Result<SweepOutcome<'s>, DevError> {
    let (project_root, name) = (cx.project_root, cx.name);
    let (profile_override, timeout) = (cx.profile_override, cx.timeout);
    // The sweep's pre-build, enumeration and first run share one phase clock,
    // as a `check` test phase's builds and runs do.
    check_cmd::enter_phase(check_cmd::Phase::Test);

    let debug = resolve_debug(profile_override, cx.test_cfg, sweep.profile);
    let profile_dir = if debug { "debug" } else { "release" };

    let (env_owned, allow_args) = sweep_env(
        sweep,
        cx.project,
        project_root,
        cx.target_dir,
        profile_dir,
        cx.allow_flags,
    );
    let env_refs: Vec<(&str, &str)> = env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // Pre-build any binary packages declared by this sweep. Skipped
    // when build_packages is empty (fallback path). Failure here
    // short-circuits the sweep with a BuildFailed outcome so the
    // aggregator marks it as failed.
    let mut support: Vec<check_cmd::SupportArtifact> = Vec::new();
    for req in attempt.support() {
        match run_pre_build(project_root, sweep, req, &env_refs, &allow_args, debug)? {
            Some(artifacts) => support.extend(artifacts),
            None => return Ok(SweepOutcome::Done(RunReport::bare(Outcome::BuildFailed))),
        }
    }

    // A doc-only sweep is one rustdoc run, unchanged: ordinary discovery
    // cannot see doctests. Every other sweep prebuilds, has each harness
    // list itself, and runs only the harnesses holding a match - see
    // `plan_focused`.
    let shape = BuildShape {
        sweep,
        allow_args: &allow_args,
        run: single_run(sweep, attempt)?,
        jobs: cx.jobs,
        debug,
    };
    let (focused, runtime_fingerprint) = if sweep.lane_exec.is_doc_only() {
        if timeout.is_some() {
            return Err(doc_only_timeout_refusal(sweep));
        }
        (None, Vec::new())
    } else {
        // Lines that belong to one sweep say which, when several run.
        let label = cx.multi.then_some(sweep.label.as_str());
        let Some((binaries, index)) =
            prebuild_targets(&shape, &env_refs, project_root, false, label)?
        else {
            return Ok(SweepOutcome::Done(RunReport::bare(Outcome::BuildFailed)));
        };
        let fingerprint = index.fingerprint()?;
        let plan = plan_focused(
            &shape,
            name,
            timeout.is_some(),
            &binaries,
            index,
            &env_refs,
            project_root,
            label,
        )?;
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
pub(super) struct SweepContext<'a> {
    pub(super) project: Project,
    pub(super) project_root: &'a Path,
    pub(super) state_root: &'a Path,
    pub(super) test_cfg: Option<&'a crate::config::TestConfig>,
    pub(super) target_dir: &'a Path,
    pub(super) allow_flags: &'a [String],
    pub(super) name: &'a str,
    pub(super) jobs: Option<u32>,
    pub(super) multi: bool,
    /// `--debug`/`--release`, which a sweep's own profile pin outranks.
    pub(super) profile_override: Option<bool>,
    /// `--timeout`, the one test's raised cap.
    pub(super) timeout: Option<u64>,
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
/// A sweep whose search found no match, held until it is known whether another
/// sweep did.
pub(super) struct NoMatchNote {
    pub(super) label: String,
    pub(super) detail: String,
}

impl NoMatchNote {
    pub(super) fn of(r: &ReadySweep<'_>) -> Self {
        let what = match &r.focused {
            Some(f) => format!(
                "no test matched in any of {}",
                output::count(f.searched, "searched harness")
            ),
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
pub(super) struct Searched {
    pub(super) harnesses: usize,
    /// Sweeps that ran only `cargo test --doc`: their search is not countable.
    pub(super) doc_only: usize,
    pub(super) labels: Vec<String>,
}

impl Searched {
    pub(super) fn of(ready: &[ReadySweep<'_>]) -> Self {
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
    pub(super) fn describe(&self) -> String {
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
pub(super) struct FocusedPlan {
    pub(super) runs: Vec<HarnessRun>,
    /// How many harnesses were listed, for the no-match note and the closing
    /// error when none matched.
    pub(super) searched: usize,
    /// Under `--timeout`, the one full test name the run is invoked with.
    pub(super) exact: Option<String>,
    /// Every harness the prebuild produced - searched, matched or not, and
    /// the `harness = false` ones excluded from the search - by path and
    /// content: the artifact set a rebuild before the run is held to.
    pub(super) artifacts: Vec<check_cmd::PlannedArtifact>,
}

/// One harness to execute directly, with the launch envelope cargo would
/// have given it.
pub(super) struct HarnessRun {
    pub(super) label: String,
    pub(super) program: String,
    pub(super) cwd: PathBuf,
    pub(super) env: Vec<(String, String)>,
    /// How many of its discovered tests matched - what its run must report.
    pub(super) matched: usize,
    /// The matched test names: the executions this harness is expected to
    /// report, which the run's inventory records.
    pub(super) names: Vec<String>,
    pub(super) unit: check_cmd::BinaryUnit,
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn plan_focused(
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
        let names = focused::discover(b, &runtime, env, project_root)
            .inspect_err(|_| {
                output::error(&format!(
                    "test {}: discovery failed for {}",
                    shape.sweep.label,
                    b.label()
                ));
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
            let m: Vec<String> = names
                .into_iter()
                .filter(|t| focused::matches(t, name, false))
                .collect();
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
        let cwd = if cwd.as_os_str() == "." {
            project_root.to_path_buf()
        } else {
            cwd
        };
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
        artifacts.push(check_cmd::PlannedArtifact::new(
            None,
            check_cmd::BinaryUnit::of(b),
            b.executable.clone(),
            hash,
        ));
    }
    Ok(FocusedPlan {
        runs,
        searched,
        exact,
        artifacts,
    })
}

/// `<label>: ` when several sweeps run (a line that belongs to one of them says
/// which), nothing when there is only one.
pub(super) fn sweep_prefix(label: Option<&str>) -> String {
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
pub(super) fn direct_libtest_args(filter: &str, exact: bool) -> Vec<String> {
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
pub(super) fn doc_only_timeout_refusal(sweep: &PlannedSweep) -> DevError {
    DevError::Config(format!(
        "--timeout cannot apply within doc-only sweep '{}': brokkr does not enumerate doctests \
         yet, so the single-test precondition cannot be checked. Drop --timeout, or use --sweep \
         to run a non-doc sweep.",
        sweep.label
    ))
}

/// Build one cargo package with the sweep's feature flags before
/// running tests. Returns the executables it produced (the support binaries a
/// run's inventory hashes) on build success, `Ok(None)` on build failure
/// (already reported), `Err` on spawn failure.
pub(super) fn run_pre_build(
    project_root: &Path,
    sweep: &PlannedSweep,
    req: &check_cmd::SupportRequest,
    env: &[(&str, &str)],
    allow_args: &[String],
    debug: bool,
) -> Result<Option<Vec<check_cmd::SupportArtifact>>, DevError> {
    let package = req.package();
    let args = pre_build_argv(sweep, req, allow_args, debug);
    let line = format!("cargo {} (sweep build: {})", args.join(" "), sweep.label);
    output::detail(&line);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // The command goes to the run log on success; a build that fails or errors
    // out names what it ran on the console, beside its diagnostics.
    let captured = cargo_with_deadline(&arg_refs, project_root, env, "sweep pre-build")
        .inspect_err(|_| {
            output::run_msg(&line);
        })?;

    if captured.success() {
        return Ok(Some(check_cmd::support_artifacts(
            &String::from_utf8_lossy(&captured.stdout),
            package,
        )));
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
    println!("[test]    BUILD FAILED {package} (sweep: {})", sweep.label);
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
pub(super) fn cargo_with_deadline(
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
        output::SpawnProfile::Cargo {
            phase: crate::check_cmd::current_phase(),
        },
    )?;
    if run.killed_on_deadline() {
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
pub(super) fn pre_build_argv(
    sweep: &PlannedSweep,
    req: &check_cmd::SupportRequest,
    allow_args: &[String],
    debug: bool,
) -> Vec<String> {
    // `json-render-diagnostics`: the artifact stream on stdout names the
    // support executables the run's inventory hashes, while compiler
    // diagnostics are still rendered as text on stderr for the failure path.
    // The message format does not enter cargo's fingerprint.
    let mut args: Vec<String> = vec![
        "build".into(),
        "--message-format=json-render-diagnostics".into(),
    ];
    args.extend(sweep.unification_args());
    args.extend(allow_args.iter().cloned());
    if !debug {
        args.push("--release".into());
    }
    // The sweep's features projected onto the support package alone.
    args.extend(req.feature_args().iter().cloned());
    // Its own explicit single-package selection, never the run's: a support
    // package is usually not the package under test.
    args.extend(req.package_args());
    args
}

/// The one request a `brokkr test` sweep runs as. The resolved package always
/// overrides the sweep's selection, so there is exactly one resolution under
/// every unification mode (package mode over one package is one run).
pub(super) fn single_run<'a>(
    sweep: &PlannedSweep,
    attempt: &'a check_cmd::Attempt,
) -> Result<&'a check_cmd::ResolutionRun, DevError> {
    match attempt.runs() {
        [one] => Ok(one),
        runs => Err(DevError::Build(format!(
            "sweep '{}' planned {} cargo resolutions for one package - a brokkr bug, please report the \
             `[[check]]` entry",
            sweep.label,
            runs.len()
        ))),
    }
}

/// Everything that shapes a sweep's build, shared by its prebuild and every
/// run that follows it, so the two cannot name different graphs.
pub(super) struct BuildShape<'a> {
    pub(super) sweep: &'a PlannedSweep,
    pub(super) allow_args: &'a [String],
    /// The sweep's request in this run: always the resolved package, as an
    /// override of the sweep's own selection, with the sweep's features
    /// projected onto it.
    pub(super) run: &'a check_cmd::ResolutionRun,
    pub(super) jobs: Option<u32>,
    pub(super) debug: bool,
}

impl BuildShape<'_> {
    /// The one package this run tests, for messages.
    pub(super) fn pkg(&self) -> &str {
        self.run
            .selection
            .packages()
            .and_then(<[String]>::first)
            .map_or("", String::as_str)
    }
}

/// The cargo half every invocation of a sweep shares: subcommand, resolution
/// pin, lint allows, profile, features, `-j`, and the one package.
///
/// `allow_args` goes before the selection: `--config` is a cargo option, and
/// everything past the `--` split belongs to libtest.
pub(super) fn cargo_head(shape: &BuildShape<'_>, leading: &[&str]) -> Vec<String> {
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
    args.extend(shape.run.feature_args().iter().cloned());
    if let Some(j) = shape.jobs {
        args.push("-j".into());
        args.push(j.to_string());
    }
    args.extend(shape.run.package_args());
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
pub(super) fn prebuild_argv(shape: &BuildShape<'_>) -> Vec<String> {
    let mut args = cargo_head(
        shape,
        &[
            "test",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ],
    );
    args.push("--tests".into());
    args
}

/// The `cargo test --doc` argv for one doc-only sweep's run of `name`. The
/// libtest half is [`direct_libtest_args`]'s, minus the filter, which cargo
/// takes positionally.
pub(super) fn test_argv(shape: &BuildShape<'_>, name: &str) -> Vec<String> {
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
pub(super) fn prebuild_targets(
    shape: &BuildShape<'_>,
    env: &[(&str, &str)],
    project_root: &Path,
    quiet: bool,
    label: Option<&str>,
) -> Result<Option<(Vec<check_cmd::TestBinary>, check_cmd::BuildRuntimeIndex)>, DevError> {
    let args = prebuild_argv(shape);
    output::detail(&format!(
        "cargo {} (sweep: {})",
        args.join(" "),
        shape.sweep.label
    ));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    // The command is routine on success and goes to the run log; a build that
    // fails or errors out names what it ran on the console, beside its
    // diagnostics - the rule `brokkr check` follows.
    let announce = || {
        output::run_msg(&format!(
            "cargo {} (sweep: {})",
            args.join(" "),
            shape.sweep.label
        ));
    };
    let captured =
        cargo_with_deadline(&arg_refs, project_root, env, "test build").inspect_err(|_| {
            announce();
        })?;
    if !captured.success() {
        announce();
        let stderr = String::from_utf8_lossy(&captured.stderr);
        let filtered = cargo_filter::filter_test_build_failure(&stderr);
        if !filtered.is_empty() {
            output::error(&filtered);
        }
        println!(
            "[test]    BUILD FAILED {} (sweep: {})",
            shape.pkg(),
            shape.sweep.label
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
pub(super) const WHOLE_SUITE_FLOOR: usize = 5;

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
pub(super) fn matches_whole_suite(listed: &[String], name: &str) -> bool {
    listed.len() >= WHOLE_SUITE_FLOOR && listed.iter().all(|t| t.contains(name))
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
pub(super) fn sweep_env(
    sweep: &PlannedSweep,
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
pub(super) fn lint_allow_flags(dev_config: &DevConfig) -> Vec<String> {
    dev_config
        .lints
        .as_ref()
        .map_or_else(Vec::new, config::check::test_phase_allow_flags)
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
pub(super) fn resolve_debug(
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
pub(super) fn resolve_package(
    cli_package: Option<&str>,
    dev_config: &DevConfig,
    project: Project,
) -> Result<(String, check_cmd::PackageSource), DevError> {
    let (spec, source) = resolve_package_spec(cli_package, dev_config, project)?;
    Ok((single_package(spec)?, source))
}

/// Refuse a package spec cargo would treat as a glob.
pub(super) fn single_package(pkg: String) -> Result<String, DevError> {
    if pkg.contains(['*', '?', '[']) {
        return Err(DevError::Config(format!(
            "'brokkr test' runs one package, and `{pkg}` is a glob that cargo may expand to \
             several. Name the package."
        )));
    }
    Ok(pkg)
}

pub(super) fn resolve_package_spec(
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
pub(super) fn resolve_sweeps(
    test_cfg: Option<&crate::config::TestConfig>,
    check_entries: &[crate::config::CheckEntry],
    sweep_filter: Option<&str>,
) -> Result<Vec<PlannedSweep>, DevError> {
    let sweeps = decide_sweeps(test_cfg, check_entries)?;
    select_sweep(sweeps, sweep_filter)
}

pub(super) fn decide_sweeps(
    test_cfg: Option<&crate::config::TestConfig>,
    check_entries: &[crate::config::CheckEntry],
) -> Result<Vec<PlannedSweep>, DevError> {
    let sweeps = check_cmd::decide_active_sweeps(check_entries, test_cfg, None, &[], false)?;
    sweeps
        .into_iter()
        .map(|mut s| {
            // The user's `<name>` is the libtest filter. Profile-level
            // `only` / `skip` / `tests` would either narrow it further (rare,
            // surprising) or cause silent zero-match failures; drop them. With
            // nothing declared applying, nothing can be audited as dead here
            // either - and `brokkr test` runs no coverage phase anyway.
            s.filters = crate::profile::LaneFilters::default();
            // `brokkr test` has no parallel fan-out to promote for, so the
            // only question is what the entry pinned. Planning the sweep here
            // is what stops the key being silently ignored by this command
            // while `check` honours it - the two must agree on what a lane
            // means, or a green `brokkr test` would be evidence about a build
            // `check` never runs.
            PlannedSweep::resolve(s, false)
        })
        .collect()
}

/// Narrow the resolved sweep set to a single `--sweep <label>`. Passing
/// `None` keeps every sweep. An unknown label is a hard error that lists
/// the labels actually available, so a typo can't silently run nothing.
pub(super) fn select_sweep(
    sweeps: Vec<PlannedSweep>,
    filter: Option<&str>,
) -> Result<Vec<PlannedSweep>, DevError> {
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
