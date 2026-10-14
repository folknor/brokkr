//! Test outcome classification and user-facing verdicts.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::cargo_filter;
use crate::error::DevError;
use crate::output;
use crate::test_runner::{self, LibtestOutcome};

use super::display::{
    LineSink, RepeatState, flush_sink, make_stderr_forwarder, make_stdout_forwarder,
    stderr_indicates_compile_error,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
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
pub(super) enum FailureKind {
    Test,
    Harness,
}

/// One failure a run reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) kind: FailureKind,
    /// The test harness it came from (`test:cli_sort`, `lib:pkg`), or empty
    /// when the run was one invocation over the whole package (`--timeout`, a
    /// doc-only sweep).
    pub(super) target: String,
    /// What failed: a test name, or a harness-level failure (a crash, a stream
    /// that stopped reporting, a hang).
    pub(super) what: String,
    pub(super) loc: Option<String>,
    pub(super) msg: Option<String>,
}

impl Failure {
    /// What the `-N` summary groups on: the harness, what failed, and where -
    /// the panic location, falling back to the message. Location first because
    /// a flaky assertion's message often carries the varying value
    /// (`rolled 0`, `rolled 2`) while its location does not move.
    pub(super) fn key(&self) -> String {
        let site = self.loc.as_deref().or(self.msg.as_deref()).unwrap_or("");
        format!("{:?}|{}|{}|{site}", self.kind, self.target, self.what)
    }

    /// The footer form: the harness is already in the run's tag.
    pub(super) fn describe(&self) -> String {
        match (&self.msg, &self.loc) {
            (Some(m), Some(l)) => format!("{}: {m} @ {l}", self.what),
            (Some(m), None) => format!("{}: {m}", self.what),
            (None, Some(l)) => format!("{} @ {l}", self.what),
            (None, None) => self.what.clone(),
        }
    }

    /// The summary form, which spans harnesses and so names the one each
    /// failure came from.
    pub(super) fn describe_qualified(&self) -> String {
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
pub(super) fn failure_set_key(failures: &[Failure]) -> String {
    let mut keys: Vec<String> = failures.iter().map(Failure::key).collect();
    keys.sort();
    keys.dedup();
    keys.join("\n")
}

/// One run's outcome plus every failure it reported - one sweep's iteration,
/// across all of that sweep's test harnesses.
pub(super) struct RunReport {
    /// The run blew a time budget rather than merely failing. Stops the command:
    /// see the `brokkr test` repeat loop.
    pub(super) timed_out: bool,
    pub(super) outcome: Outcome,
    /// Empty for a PASS/SKIP/BUILD FAILED, and for a FAIL nothing could be
    /// attributed to (the summary then says `unknown failure`).
    pub(super) failures: Vec<Failure>,
}

impl RunReport {
    pub(super) fn bare(outcome: Outcome) -> Self {
        Self {
            outcome,
            timed_out: false,
            failures: Vec::new(),
        }
    }

    /// A run that blew a time budget. Fails, and stops the command.
    pub(super) fn timed_out(target: &str, msg: String) -> Self {
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
    pub(super) fn merge(parts: Vec<RunReport>) -> Self {
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

/// The `-N` closing summary: one counts line, then one group per distinct
/// failure SET with its occurrence count - the first-seen run's failures
/// represent it, one per line when there are several.
///
/// Grouped on the whole set, not on the first failure: once every harness runs,
/// one run can fail three tests, and keying on the first of them folded a run
/// that failed A and B into the group of a run that failed A alone - the flake
/// report then said nothing about B.
pub(super) fn format_repeat_summary(reports: &[RunReport]) -> Vec<String> {
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

/// Renders a lone failing test's footer tag from its full name and the wall.
pub(super) type TestTag<'a> = dyn Fn(&str, &str) -> String + 'a;

/// How one invocation is labelled and bounded.
pub(super) struct OneRun<'a> {
    pub(super) tag: &'a str,
    /// The footer tag a single failing test leads with instead of `tag`,
    /// rendered from the run's structured context given the test's full name
    /// and the wall time; `None` where the run has no such context (a doc-only
    /// sweep).
    pub(super) test_tag: Option<&'a TestTag<'a>>,
    /// The harness, for attributing failures; empty for a whole-package run.
    pub(super) target: &'a str,
    pub(super) ceilings: test_runner::Ceilings,
    /// Print the "built in Xs; running tests" framing line (the doc-only
    /// cargo run's; a directly run harness has no build phase).
    pub(super) announce: bool,
    /// For a directly executed harness, how many tests discovery listed as
    /// matching: the run must account for exactly that many. `None` for the
    /// doc-only cargo run, which nothing enumerated.
    pub(super) expected: Option<usize>,
    /// The test names discovery listed as matching this harness: a failure
    /// is a `Test` only under one of them. `None` where nothing enumerated.
    pub(super) listed: Option<&'a [String]>,
    /// Where the run's typed observations go (the run's journal), and no
    /// harness isolation: a directly executed harness is already one stream.
    pub(super) observe: test_runner::Observe,
}

pub(super) fn aggregate_exit(
    outcomes: &[Outcome],
    pkg: &str,
    name: &str,
    searched: &str,
) -> Result<(), DevError> {
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
            "no test matches `{name}` in {pkg}{searched}"
        )));
    }
    // At least one sweep passed; NoMatch in other sweeps is informational
    // (the test was feature-gated out of those sweeps).
    Ok(())
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn run_one(
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
    let parsed = test_runner::parse_test_output_with_stderr(&stdout_lines, &stderr_lines);

    let has_test_result = stdout_lines.iter().any(|l| l.starts_with("test result:"));
    // Only cargo compiles; a directly run harness printing `error: ...` is a
    // test talking, never a build failure.
    let has_compile_error = !direct
        && stderr_lines
            .iter()
            .any(|l| stderr_indicates_compile_error(l));

    // Display the test-runtime wall: total minus the cargo build phase
    // (which the `[test] test binaries built in ...s` line already
    // surfaces). Falls back to total if cargo never reported `Finished`.
    let test_wall = run.build_elapsed.map_or(run.captured.elapsed, |b| {
        run.captured.elapsed.saturating_sub(b)
    });
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
        return Ok(report_build_failure(
            tag,
            &wall,
            first,
            stderr_text.as_ref(),
        ));
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
        return Ok(RunReport {
            outcome: Outcome::Fail,
            timed_out: false,
            failures,
        });
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
    if let test_runner::Completeness::Incomplete { reason } = &parsed.completeness {
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
pub(super) fn failure_kind(
    f: &test_runner::ParsedTestFailure,
    listed: Option<&[String]>,
) -> FailureKind {
    let selectable = listed.is_none_or(|l| l.contains(&f.name));
    if f.verdict && selectable {
        FailureKind::Test
    } else {
        FailureKind::Harness
    }
}

/// Report a run the watchdog killed for blowing its budget. The hang's
/// diagnosis prints once per signature across `-N` repeats.
pub(super) fn report_hung(
    shape: &OneRun<'_>,
    hung: &test_runner::HungTest,
    wall: &str,
    cwd: &Path,
    repeat_state: &RepeatState,
    sink: Option<LineSink>,
) -> RunReport {
    let named = hung.attribution.test_name().unwrap_or("");
    let first = repeat_state.first_sighting(&format!("{}|hung {named}", shape.target));
    flush_sink(sink, !first);
    if first {
        output::error(&test_runner::format_hung_test(hung, cwd));
    }
    let secs = hung.ceiling.as_secs();
    if let Some(test_tag) = hung_test_tag(shape, hung)
        && let Some(name) = hung.attribution.test_name()
    {
        println!(
            "[test]    FAIL {} - hung test exceeded {secs}s",
            test_tag(name, wall)
        );
    } else {
        println!(
            "[test]    FAIL {} ({wall}) - hung test exceeded {secs}s",
            shape.tag
        );
    }
    std::io::stdout().flush().ok();
    RunReport::timed_out(
        shape.target,
        format!("hung test exceeded {}s", hung.ceiling.as_secs()),
    )
}

/// The footer tag renderer a hung test leads with, when its name is a verified
/// identity: the run held exactly one test, named by the caller
/// (`WallShape::OneTest`), and the kill charged that test. The runner attributes
/// a tracker expiry and this one-test invocation's wall expiry alike to the
/// caller's resolved name (`Attribution::Exact`). On a shared harness the name
/// is the tracker's best-known suspect, and an idle kill has no test to name at
/// all, so those keep the filter-led form.
pub(super) fn hung_test_tag<'a>(
    shape: &OneRun<'a>,
    hung: &test_runner::HungTest,
) -> Option<&'a TestTag<'a>> {
    let one_test = matches!(shape.ceilings.shape, test_runner::WallShape::OneTest { .. });
    let charged = matches!(hung.attribution, test_runner::Attribution::Exact { .. });
    if one_test && charged {
        shape.test_tag
    } else {
        None
    }
}

/// The verdict for a directly run harness whose run does not account for
/// exactly the `want` tests discovery listed as matching it, or `None` when it
/// does.
///
/// The harness was chosen because discovery found the name in it, so zero is
/// not a SKIP here: a run that then reports none - or a different number -
/// disagrees with its own listing, and no verdict can rest on that. A stream
/// that never finished reporting says so in its own words.
pub(super) fn disagrees_with_discovery(
    tag: &str,
    target: &str,
    wall: &str,
    want: usize,
    parsed: &test_runner::ParsedTestResults,
) -> Option<RunReport> {
    if let test_runner::Completeness::Incomplete { reason } = &parsed.completeness {
        return Some(report_incomplete(tag, target, wall, reason, parsed));
    }
    let got = parsed.accounted();
    if got == want {
        return None;
    }
    let failure = discovery_mismatch(target, want, got);
    println!("[test]    FAIL {tag} ({wall}) - {}", failure.describe());
    std::io::stdout().flush().ok();
    Some(RunReport {
        outcome: Outcome::Fail,
        timed_out: false,
        failures: vec![failure],
    })
}

/// The failure a harness files when its run accounts for `got` tests where
/// discovery matched `want` in it.
pub(super) fn discovery_mismatch(target: &str, want: usize, got: usize) -> Failure {
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
pub(super) fn stop_for_budget(reports: &[RunReport], repeat: u32, label: &str) -> DevError {
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
pub(super) fn ceilings_for(exact: &Option<String>, ceiling: Duration) -> test_runner::Ceilings {
    match exact {
        Some(resolved) => test_runner::Ceilings::one_test(ceiling, resolved),
        None => test_runner::Ceilings::shared_harness_with(ceiling),
    }
}

/// Report a run that failed to build. Compile errors are identical across `-N`
/// repeats by construction (same source, same flags), so the block prints once
/// and later repeats collapse to the footer.
pub(super) fn report_build_failure(
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
pub(super) fn report_incomplete(
    tag: &str,
    target: &str,
    wall: &str,
    reason: &str,
    parsed: &test_runner::ParsedTestResults,
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
pub(super) fn harness_failure(
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
pub(super) fn describe_harness_status(status: &std::process::ExitStatus) -> String {
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
pub(super) fn print_fail_footer(
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
        println!(
            "[test]    FAIL {tag} ({wall}) - {} failures:",
            failures.len()
        );
        for f in failures {
            println!("[test]      {}", f.describe());
        }
    }
    std::io::stdout().flush().ok();
}
