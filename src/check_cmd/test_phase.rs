/// The run-wide inputs of the test phase.
struct TestPhaseArgs<'a> {
    project: Option<Project>,
    project_root: &'a Path,
    state_root: &'a Path,
    sweeps: &'a [PlannedSweep],
    /// The test phase's selection, aligned with `sweeps`.
    selection: &'a PhaseSelection,
    doctests: bool,
    commands: bool,
    extra_args: &'a [String],
    lints: &'a LintsConfig,
    /// The run is held to a `certifies = "complete"` claim.
    certifying: bool,
}

/// Preserve the distinction between a lane whose tests failed and an
/// operational error. The phase uses that distinction for fail-fast journal
/// records and the final verdict; matching a `DevError` message would make
/// either behavior depend on user-visible text.
enum TestPhaseFailure {
    TestsFailed,
    Error(DevError),
}

impl From<DevError> for TestPhaseFailure {
    fn from(error: DevError) -> Self {
        Self::Error(error)
    }
}

/// Iterate the lanes the test selection attempts, pre-building each sweep's
/// `build_packages` and then running `cargo test` for it. Fails fast on the
/// first sweep that fails (build or test), mirroring how the clippy phase
/// short-circuits on a non-zero status. A "nothing reached" refusal was
/// reported before preparation, so this phase never meets one.
///
/// `prepared` is the plan's per-lane preparation under a complete claim. Every
/// lane journals what it observes; a lane the fail-fast never reaches has
/// that recorded as a termination too, so its planned executions read as
/// unobserved for a stated reason. `stop` receives the termination that
/// decided the phase, when one did.
fn run_test_phase(
    t: &TestPhaseArgs<'_>,
    prepared: Option<&Lanes<Option<PreparedLane>>>,
    mut timings: Option<&mut Vec<TestTiming>>,
    ledger: &mut ReachLedger,
    stop: &mut Option<TerminationSummary>,
) -> Result<(), TestPhaseFailure> {
    let TestPhaseArgs {
        project: _,
        project_root,
        state_root,
        sweeps,
        selection,
        doctests,
        commands,
        extra_args,
        lints,
        certifying,
    } = *t;
    let project = t.project.clone();
    let multi = sweeps.len() > 1;
    let allow_flags = crate::config::check::test_phase_allow_flags(lints);
    announce_test_allows(project_root, sweeps, &allow_flags, &lints.allow_exact);
    // `brokkr check`'s test phase runs `cargo test` in the dev profile
    // unless the sweep's `[[check]] profile` says otherwise, so each
    // sweep's `build_packages` artefacts land in that profile's
    // `<target>/` subdirectory. Tests that spawn the just-rebuilt binary
    // read BROKKR_TEST_BIN_DIR to skip the `cfg!(debug_assertions)`
    // profile guess (which silently lies when a workspace pins
    // `[profile.test]` overrides, and now also when a sweep opts into
    // release).
    let info = build::project_info(Some(project_root))?;
    let owners = OwnerIndex::from_metadata_at(
        &info.workspace_root,
        project_root,
        &info.workspace_members,
        &info.member_manifests,
    );
    let target_dir = info.target_dir;

    let mut sweeps_run = 0usize;
    let mut outcome: Result<(), TestPhaseFailure> = Ok(());
    for (i, sweep) in sweeps.iter().enumerate() {
        // The CLI `-p` set intersected with the sweep's selection when the
        // selection was built: ruled-out packages are dropped with a note, and
        // the lane is excluded only when nothing survives. Logged only: `-p`
        // is this invocation's doing, so `check` announced it once, up front
        // (`announce_package_rules`), rather than once per phase.
        let lane_idx = LaneIdx::new(i);
        let attempt = match selection.entry(lane_idx) {
            Some(LaneEntry::Attempt(a)) => a,
            other => {
                let reason = other.and_then(LaneEntry::skip_reason).unwrap_or_default();
                output::detail(&format!("test {}: skipped ({reason})", sweep.label));
                continue;
            }
        };
        for note in attempt.notes() {
            output::detail(&format!("test {}: {note} (dropped)", sweep.label));
        }
        sweeps_run += 1;
        // Record before the run: the sweep is reached, pass or fail. A sweep
        // the loop never reaches (an earlier one failed fast) stays unreached.
        ledger.reach_test(lane_idx);

        let tap = LaneTap::new(i);
        tap.record(JournalRecord::LaneStarted { lane: i });
        let lane = run_test_lane(
            &LaneArgs {
                project: project.clone(),
                project_root,
                state_root,
                target_dir: &target_dir,
                sweep,
                attempt,
                extra_args,
                allow_flags: &allow_flags,
                doctests,
                multi,
                commands,
                certifying,
            },
            prepared.and_then(|p| p.get(lane_idx)).and_then(Option::as_ref),
            &owners,
            &tap,
            timings.as_deref_mut(),
        );
        tap.record(JournalRecord::LaneFinished { lane: i, passed: matches!(lane, Ok(true)) });
        // A green run never printed this sweep's shape, so a failure leaving
        // the lane - a red test, or any error: a pre-build, a lane refusal, a
        // spawn failure - names it here, once, whatever path it took.
        let failed = match lane {
            Ok(true) => None,
            Ok(false) => Some(TestPhaseFailure::TestsFailed),
            Err(e) => Some(TestPhaseFailure::Error(e)),
        };
        // Every failed lane ends this loop; even an ordinary nextest `Ok(false)`
        // does not advance to another sweep. Timeouts return `Err` so their
        // per-test stop reason survives instead of becoming a generic red run.
        if let Some(e) = failed {
            output::error(&format!(
                "test {}: {}",
                sweep.label,
                describe_sweep(sweep, true, attempt.selection(), attempt.described_features())
            ));
            let attempted =
                |l: usize| selection.entry(LaneIdx::new(l)).is_some_and(|entry| entry.attempt().is_some());
            let decisive = record_phase_stop(i, sweeps.len(), &e, &tap, attempted);
            let label = |l: usize| sweeps.get(l).map(|s| s.label.clone());
            *stop = decisive.as_ref().map(|d| TerminationSummary::of(d, label));
            outcome = Err(e);
            break;
        }
    }

    // Held warnings print either way: a warning from a sweep that passed
    // before a later one failed is still a warning.
    flush_warnings(sweeps_run);
    outcome?;

    print_test_line(sweeps_run, phase_elapsed());
    Ok(())
}

/// Record why the test phase stopped at lane `failed`, and return the
/// termination that decided it.
///
/// - A lane whose tests failed stops the phase by fail-fast: every later lane
///   the phase would have attempted is recorded as stopped by it, so its
///   planned executions are unobserved for that stated reason, not for none.
///   A lane the selection excluded expects nothing and was stopped by
///   nothing; recording it would name it as the stop's scope. With no later
///   attempted lane, nothing was cut short and there is no termination.
/// - A stop (interrupt, phase watchdog) is the run's.
/// - Anything else that ends the phase - a blown budget, a refusal - is the
///   run's too, carrying the lane's own decisive termination when it recorded
///   one: a per-test timeout stops brokkr, and everything after it is
///   unobserved because of that timeout.
fn record_phase_stop(
    failed: usize,
    lanes: usize,
    e: &TestPhaseFailure,
    tap: &LaneTap,
    attempted: impl Fn(usize) -> bool,
) -> Option<Termination> {
    let own = tap.decisive_termination();
    match e {
        TestPhaseFailure::TestsFailed => {
            let mut first = None;
            for lane in fail_fast_casualties(failed, lanes, attempted) {
                let t = Termination {
                    scope: TerminationScope::Lane,
                    lane: Some(lane),
                    stream: None,
                    cause: TerminationCause::FailFast,
                    test: None,
                    charged: None,
                };
                journal_append(&JournalRecord::Terminated(t.clone()));
                first.get_or_insert(t);
            }
            own.or(first)
        }
        TestPhaseFailure::Error(other) => record_run_stop(other, own),
    }
}

/// The lanes a fail-fast at `failed` stops: every later one the phase would
/// have attempted.
fn fail_fast_casualties(failed: usize, lanes: usize, attempted: impl Fn(usize) -> bool) -> Vec<usize> {
    (failed + 1..lanes).filter(|&l| attempted(l)).collect()
}

#[cfg(test)]
mod fail_fast_tests {
    use super::fail_fast_casualties;

    /// An excluded lane between the failure and a later attempted one is
    /// stopped by nothing; the attempted one is the stop's first casualty.
    #[test]
    fn fail_fast_spares_lanes_the_selection_excluded() {
        assert_eq!(fail_fast_casualties(0, 4, |l| l != 1), vec![2, 3]);
        assert!(fail_fast_casualties(0, 2, |l| l != 1).is_empty());
        assert!(fail_fast_casualties(1, 2, |_| true).is_empty());
    }
}

/// What one test lane needs from the phase, bundled so the lane body can be
/// its own function - which is what lets the phase attach the sweep's shape
/// to every way out of it.
struct LaneArgs<'a> {
    project: Option<Project>,
    project_root: &'a Path,
    state_root: &'a Path,
    target_dir: &'a Path,
    sweep: &'a PlannedSweep,
    /// The lane's selection and resolutions: what it builds and runs.
    attempt: &'a Attempt,
    extra_args: &'a [String],
    allow_flags: &'a [String],
    doctests: bool,
    multi: bool,
    commands: bool,
    certifying: bool,
}

/// A lane's `build_packages` pre-builds, run again before a lane that builds.
fn build_lane_support(
    a: &LaneArgs<'_>,
    sweep: &PlannedSweep,
    env: &LaneEnv,
    owners: &OwnerIndex,
) -> Result<Vec<SupportArtifact>, DevError> {
    let mut support: Vec<SupportArtifact> = Vec::new();
    for req in a.attempt.support() {
        support.extend(run_sweep_pre_build(
            a.project_root,
            sweep,
            req,
            &env.project_env,
            &env.allow_args,
            owners,
            a.commands,
        )?);
    }
    Ok(support)
}

/// Run one sweep's test lane: its pre-builds (unless it runs its prepared
/// snapshot, or verifies its own `cargo test` build at the shim), then
/// whichever harness it names. `Ok(false)` is a failure already reported.
///
/// `prepared` is the lane's plan: its artifacts are verified against it before
/// anything runs, and the lane executes exactly what it lists. Where the
/// up-front preparation failed, the lane reports that failure once (the
/// lanes that need a selection) or runs without an inventory (serial); it is
/// never prepared again. Only an inventory gone stale (artifacts replaced by
/// another lane's build) re-prepares.
#[allow(clippy::too_many_lines)]
fn run_test_lane(
    a: &LaneArgs<'_>,
    prepared: Option<&PreparedLane>,
    owners: &OwnerIndex,
    tap: &LaneTap,
    timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let sweep = a.sweep;
    refuse_target_sweep_args(sweep, &ForwardedArgs::parse(a.extra_args))?;
    // A lane whose up-front preparation failed has no inventory and is not
    // prepared a second time: preparing lists (executes) every binary, so a
    // retry would run each listing twice in one invocation. A lane that needs a
    // selection to execute reports the recorded failure; a serial lane runs
    // through cargo, as it does with any other lane without an inventory.
    let prepared = match prepared {
        Some(p) if p.prepare_failed.is_some() => {
            if let Some(f) = &p.prepare_failed
                && matches!(lane_kind(sweep), LaneKind::Parallel | LaneKind::Isolated | LaneKind::Nextest)
            {
                for line in &f.held {
                    output::error(line);
                }
                return Err(DevError::Reported(f.message.clone()));
            }
            None
        }
        other => other,
    };
    let inputs = LaneInputs {
        project: a.project.clone(),
        project_root: a.project_root,
        state_root: a.state_root,
        target_dir: a.target_dir,
        allow_flags: a.allow_flags,
        owners,
        commands: a.commands,
        certifying: a.certifying,
        doctests: a.doctests,
        // Running a lane enumerates no universe; that is the prepare phase's.
        universe_selectors: &NO_UNIVERSE_SELECTORS,
    };
    // Per-sweep: a sweep carrying `rustflags` runs in its own isolated
    // target dir with a matching BROKKR_TEST_BIN_DIR + RUSTFLAGS, so a
    // global cfg (e.g. `--cfg madsim`) never thrashes the plain sweeps. The
    // lint allows reach the pre-build and the test run alike - a pre-build
    // compiling the same crate under the unsuppressed lint fails before
    // `cargo test` is ever reached.
    let env = match prepared {
        Some(p) => p.env.clone(),
        None => lane_env(&inputs, sweep)?,
    };
    let kind = lane_kind(sweep);
    let needs_selection = matches!(kind, LaneKind::Parallel | LaneKind::Isolated | LaneKind::Nextest);
    // Decided once per lane: the route below and every run of the lane read
    // the same answer, so a lane routed to verify at the shim gets the shim.
    let shim = matches!(kind, LaneKind::Serial | LaneKind::DocOnly)
        .then(|| SerialShim::decide(a.project_root, sweep, a.extra_args, &env.project_env));
    let snapshot_holds = |p: &PreparedLane| p.snapshot.as_ref().is_some_and(|s| snapshot_drift(s).is_empty());

    // A lane that executes its binaries directly runs its prepared snapshot:
    // when every executable it runs or hands its tests still has the content
    // the plan listed, it runs as planned, with no cargo invocation. A build
    // here would only ask whether building again changes anything - and a
    // project whose build scripts rerun on every build answers yes, which is
    // drift this lane never had. Any mismatch takes the rebuild-and-compare
    // path below.
    let unchanged = prepared.is_some_and(|p| needs_selection && p.verify && p.unavailable.is_none() && snapshot_holds(p));
    // A serial or doc-only lane runs `cargo test`, which builds before it runs
    // anything, so it is the build that executes that gets verified: its own
    // artifact stream is held to the plan when it finishes, and the shim holds
    // every harness until it has been (`GateCheck`). No build of its own
    // happens here - except its `build_packages` support builds when the
    // snapshot shows something the plan listed changed, since `cargo test`
    // never rebuilds a support package outside its selection.
    let gated = shim == Some(SerialShim::Attach)
        && prepared.is_some_and(|p| p.verify && p.unavailable.is_none() && p.snapshot.is_some());
    let mut gated_support: Option<Vec<SupportArtifact>> = None;
    let own;
    let prepared = match prepared {
        Some(p) if unchanged => Some(p),
        Some(p) if gated => {
            gated_support =
                Some(if snapshot_holds(p) { p.support.clone() } else { build_lane_support(a, sweep, &env, owners)? });
            Some(p)
        }
        // A serial lane whose harnesses cannot be attributed: it runs through
        // cargo, as it always has, with no plan to hold it to.
        Some(p) if p.unavailable.is_some() => {
            build_lane_support(a, sweep, &env, owners)?;
            None
        }
        Some(p) if !p.verify => {
            build_lane_support(a, sweep, &env, owners)?;
            Some(p)
        }
        Some(p) => {
            // Something the plan listed changed since: build the lane again
            // (re-uplifting the support binaries first) and prove it is about
            // to run what was enumerated.
            let support = build_lane_support(a, sweep, &env, owners)?;
            let drift = lane_drift_of(&inputs, sweep, p, &support)?;
            if drift.is_empty() {
                Some(p)
            } else if a.certifying {
                // Never a silent re-plan: a plan that follows the artifacts
                // around certifies whatever happens to be on disk.
                return Err(drift_error(&sweep.label, &drift));
            } else {
                // Nothing is certified here, so the lane is not refused for a
                // plan that went stale - an earlier lane's build can replace
                // an artifact in place, and before there was an inventory
                // every lane prepared just before it ran. It runs what it
                // builds now, and the record says its inventory no longer
                // describes it: its observations are not read against a
                // selection it did not run.
                let reason = format!("artifacts changed since the plan - {}", drift.join("; "));
                tap.record(JournalRecord::LaneSuperseded { lane: tap.lane(), reason: reason.clone() });
                output::detail(&format!(
                    "test {}: {reason}; the lane runs what it builds now, and its inventory is unavailable",
                    sweep.label
                ));
                if needs_selection {
                    own = prepare_lane(&inputs, sweep, a.attempt, a.extra_args)?;
                    Some(&own)
                } else {
                    None
                }
            }
        }
        None => {
            build_lane_support(a, sweep, &env, owners)?;
            if needs_selection {
                own = prepare_lane(&inputs, sweep, a.attempt, a.extra_args)?;
                Some(&own)
            } else {
                None
            }
        }
    };

    match (kind, prepared) {
        (LaneKind::Nextest, Some(p)) => run_nextest_sweep(sweep, a.attempt, p, tap, a.commands),
        (LaneKind::Parallel, Some(p)) => run_parallel_sweep(
            a.project_root,
            a.state_root,
            sweep,
            a.attempt,
            match sweep.lane_exec() {
                LaneExec::Parallel { budget } => budget,
                _ => 1,
            },
            p,
            tap,
            a.commands,
            timings,
        ),
        (LaneKind::Isolated, Some(p)) => run_isolated_sweep(
            a.project_root,
            a.state_root,
            sweep,
            a.attempt,
            p,
            tap,
            a.commands,
            timings,
        ),
        _ => run_sequential_resolutions(
            a.project_root,
            a.state_root,
            sweep,
            a.attempt,
            a.extra_args,
            &env,
            owners,
            (a.doctests, a.multi, a.commands, a.certifying),
            timings,
            prepared,
            gated_support.as_deref(),
            &shim.unwrap_or_else(|| SerialShim::decide(a.project_root, sweep, a.extra_args, &env.project_env)),
            tap,
        ),
    }
}

