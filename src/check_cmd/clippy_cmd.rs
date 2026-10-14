#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_clippy(
    project_root: &Path,
    check_entries: &[CheckEntry],
    packages: &[String],
    all_features: bool,
    features: &[String],
    no_default_features: bool,
    sweep_name: Option<&str>,
    lib_only: bool,
    env_overrides: &[(String, String)],
    lints: &LintsConfig,
) -> Result<(), DevError> {
    let started = std::time::Instant::now();
    // `check`'s clippy ceiling applies here too: the ceilings exist because of
    // an observed 1h15m clippy hang, and this runner drives the same pipeline.
    // The caller already holds the lock, so a wait behind another command is
    // not charged. Declared before the guard, so the guard drops first.
    let _ceiling = CheckWatchdog::arm_for("brokkr clippy", None)?;
    enter_phase(Phase::Clippy);
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let mut declaration = build_clippy_sweep(
        check_entries,
        packages,
        all_features,
        features,
        no_default_features,
        sweep_name,
        env_overrides,
    )?;
    // Applied after construction rather than inside `build_clippy_sweep`: the
    // target selector is orthogonal to where the sweep's shape came from, so
    // `--lib` composes with `--sweep NAME` the same way it does with ad-hoc
    // `-p`, without the borrowed entry having to know about it.
    declaration.lib_only = lib_only;
    // The same resolution point `check` uses, so `--sweep NAME` replays the
    // entry's graph and not just its flags: a `feature_unification =
    // "package"` entry lints once per package under the package pin, and a
    // parallel `auto` entry that `check` promotes to a workspace pin is
    // promoted here too. Without this the sweep kept its unresolved `Ambient`
    // default and linted one ambient invocation - a lint surface no gate
    // build compiles. An ad-hoc sweep is never parallel, so it resolves to
    // exactly what it was. The CLI scope is empty on purpose: ad-hoc `-p`
    // lives in `sweep.packages`, where it already disqualifies promotion.
    let sweep = resolve_invocation_sweeps(vec![declaration], project_root, &[], &ForwardedArgs::parse(&[]))?
        .pop()
        .expect("one declaration plans one sweep");
    // The same serial-builds rule `check` resolves: a configured cargo build
    // dir makes one chain of the probe's runs too.
    let serial_builds = resolve_serial_builds(project_root, false, std::slice::from_ref(&sweep));

    // One sweep -> run_clippy_phase runs `multi = false`, so output carries no
    // sweep-label tags. The selection has no CLI override: ad-hoc `-p` is
    // already in sweep.packages, so the entry is the sweep's own Explicit (or
    // Bare) selection. `Reporter::Clippy` carries the investigative output
    // policy for finding out what a given target shape
    // actually does, so the full cargo line is the point - but said once,
    // after the run (the command line when green, the failing-command pair
    // when not), not streamed before a failure that then repeats it.
    // The investigative runner has no `--json` trailer, so nothing records
    // which sweeps were reached.
    //
    // `narrowed`: one probe shape cannot testify that a sited allow is stale
    // unless it covers everything `check`'s sweeps together would - see
    // `probe_is_narrowed`.
    let sweeps = std::slice::from_ref(&sweep);
    let oracle = FeatureOracle::at(project_root);
    let selection = PhaseSelection::for_check(SelectionPhase::Clippy, sweeps, &[], &oracle)?;
    let outcome = run_clippy_phase(
        project_root,
        sweeps,
        &selection,
        lints,
        Reporter::Clippy,
        &mut |_| {},
        probe_is_narrowed(&sweep),
        serial_builds,
    );
    // The watchdog already said why it killed the run; whatever the kill
    // surfaced as is its echo. Checked whatever the outcome: a ceiling that
    // fired just as the last child exited can leave an `Ok` behind it, and a
    // run the watchdog killed is not "clean".
    if watchdog_fired().is_some() {
        return Err(DevError::ExitCode(WATCHDOG_EXIT_CODE));
    }
    match outcome {
        Ok(()) => {
            output::result_msg(&format!("clippy clean in {}", fmt_wall(started.elapsed())));
            Ok(())
        }
        // A *rendered* clippy failure: the phase already printed the diagnostics,
        // so add the summary and exit 1 without main echoing a second line.
        //
        // Matched on the phase's own verdict, not on any `Build`: the phase
        // also returns `Build` from `project_info` ("cargo metadata failed:
        // ...", "no Cargo.toml ..."), and those carry their whole diagnostic
        // in the message - swallowing them printed `clippy failed in 0.0s`
        // with no cause in a non-Rust directory. The verdict is a
        // `DevError::Reported`, the variant for "detail already printed".
        Err(PhaseFailure::Findings(Phase::Clippy)) => {
            output::error(&format!("clippy failed in {}", fmt_wall(started.elapsed())));
            Err(DevError::ExitCode(1))
        }
        // Anything else (no Cargo.toml, cargo metadata failure, cargo missing,
        // spawn failure, cooperative interrupt) is NOT an already-rendered lint
        // result - propagate the real cause so main reports it, instead of
        // masking it behind "clippy failed".
        Err(other) => Err(other.into_dev_error()),
    }
}
