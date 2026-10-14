// Implementation of the `check` command (clippy + tests).
//
// Both phases iterate the same list of "active sweeps" - each one a
// cargo invocation with a specific feature flag set, optional
// pre-built binary packages, and (for tests) optional libtest
// filters. The list is built once at the top of `cmd_check` from
// whichever of these inputs apply, in priority order:
//
// 1. CLI `--features` / `--no-default-features` flags → a single
//    ad-hoc sweep that ignores `[[check]]` and any profile.
// 2. CLI `--profile <name>` or `[test].default_profile` → the
//    profile's resolved sweeps (each backed by a `[[check]]` entry,
//    plus the profile's libtest filters).
// 3. `[[check]]` entries are configured but no profile applies →
//    every entry runs in declaration order with no libtest filters.
// 4. None of the above → a single `--all-features` sweep, matching
//    `brokkr check`'s pre-`[[check]]` behaviour for projects that
//    haven't migrated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::build;
use crate::cargo_filter;
use crate::cargo_json;
use crate::diagnostic::{Diagnostic, Owner, OwnerIndex};
use crate::config::{
    Certifies, CheckEntry, Claim, DependencyRule, GremlinsConfig, HeaderConfig, LintsConfig,
    ManifestConfig, RustdocConfig, QuarantineEntry, ScriptCheck, SitedAllow,
    Stage, TestConfig, TextlintRule,
};
use crate::rustflags;
use crate::error::DevError;
use crate::output;
use crate::profile::{self, DeclaredFilter, FilterKind, LaneExec, PlannedSweep, Threads};
use crate::project::Project;
use crate::scope;
use crate::rustc_text::{self, BlockKind, Level};
use crate::test_runner;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn cmd_check(
    project: Option<Project>,
    project_root: &Path,
    state_root: &Path,
    check_entries: &[CheckEntry],
    dependency_rules: &[DependencyRule],
    quarantine: &[QuarantineEntry],
    test_cfg: Option<&TestConfig>,
    bin_cfg: Option<&crate::config::BinConfig>,
    gremlins_cfg: Option<&GremlinsConfig>,
    header_cfg: Option<&HeaderConfig>,
    textlint_rules: &[TextlintRule],
    script_checks: &[ScriptCheck],
    manifest_cfg: Option<&ManifestConfig>,
    rustdoc_cfg: Option<&RustdocConfig>,
    lints: &LintsConfig,
    features: &[String],
    no_default_features: bool,
    packages: &[String],
    profile_name: Option<&str>,
    gate: bool,
    force_rust: bool,
    json: bool,
    fix_gremlins: bool,
    timings: bool,
    commands: bool,
    extra_args: &[String],
) -> Result<(), DevError> {
    let started = std::time::Instant::now();
    let _scope = RunScope::begin()?;
    // A graceful `brokkr kill` / Ctrl-C ends the run as `Interrupted` (exit 130,
    // main's scratch cleanup) rather than killing brokkr on the default action
    // mid-phase. Every runner the phases use either polls the flag or is
    // released by the handler killing its registered test group; a wait that
    // does neither still yields to a second signal. See `crate::shutdown`.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let gate_name = resolve_gate_profile(gate, test_cfg)?;
    let profile_name = gate_name.as_deref().or(profile_name);
    let active_sweeps = active_sweeps_resolved(check_entries, test_cfg, profile_name,
        (features, no_default_features), (project_root, packages, extra_args))?;
    // The profile that drove sweep selection, if any. Ad-hoc CLI features
    // bypass profiles entirely (priority 1 in decide_active_sweeps), so an
    // ad-hoc run reports no profile even when [test].default_profile is set.
    let profile_label = if features.is_empty() && !no_default_features {
        effective_profile_name(test_cfg, profile_name)?
    } else {
        None
    };
    let (certifies, skip_phases) = profile_claim(&profile_label, test_cfg);
    let claim = Claim::new(certifies);
    reject_scoped_complete(claim, packages)?;
    reject_extra_args_complete(claim, extra_args)?;

    announce_profile_header(&active_sweeps, &profile_label);
    announce_invocation_shaping(project_root, packages, extra_args);
    announce_adhoc_shaping(
        features,
        no_default_features,
        test_cfg,
        profile_name,
    )?;

    let mut collected_timings: Vec<TestTiming> = Vec::new();
    // What the run reports beyond its verdict: how it stopped, and - under a
    // complete claim - the policy and execution-accounting blocks.
    let mut run_report = RunReport::default();
    // Which sweeps each phase group actually started work on, for the
    // trailer's `sweeps` list: a sweep runs in clippy/rustdoc, in the
    // test phase, or both, and the trailer lists the union. Two flags rather
    // than one because clippy runs first - folding it into the test flag
    // would mark lanes an earlier test-phase fail-fast never reached. Policy coverage never reads this - it reads the
    // plan.
    let mut ledger = ReachLedger::new(&active_sweeps);
    // Doctests off unless `[test] doctests = true` (nextest/CI never runs them).
    let doctests = test_cfg.is_some_and(|c| c.doctests);
    let serial_builds =
        resolve_serial_builds(project_root, test_cfg.is_some_and(|c| c.serial_builds), &active_sweeps);

    // A partial profile may skip phases (validated against Phase::ALL at
    // load time). Announce the omission up front - a narrowed run must
    // never look like a full one in the log.
    announce_skipped_phases(skip_phases);

    // On top of that, a tree whose only uncommitted change is markdown gets
    // the prose-only shortcut: the conventions still apply to prose, but
    // nothing that was just edited can have changed how the code builds.
    // Anything that shapes or demands the build waives the shortcut. A profile
    // covers `--gate` and `certifies` too: both imply one is in effect.
    let shaped =
        no_default_features || !features.is_empty() || !packages.is_empty() || !extra_args.is_empty();
    let waived = force_rust || shaped || profile_label.is_some();
    let prose_skips = prose_only_skips(project_root, waived);
    let skip = |phase: Phase| {
        skip_phases.iter().any(|name| name == phase.as_str()) || prose_skips.contains(&phase)
    };

    // Which lanes each phase is eligible to attempt, with which packages:
    // decided once, here, and read by every phase. A structural error (package
    // mode over a selection naming no packages) fails before anything runs,
    // with no trailer; a "nothing reached" refusal is stored and reported at
    // its phase's boundary.
    // Each planned cargo request also carries the sweep's features projected
    // onto its packages; the metadata snapshot that takes is read at most
    // once, and only when some projection could drop a token.
    let oracle = FeatureOracle::at(project_root);
    let selections = CheckSelections::build(
        &active_sweeps,
        packages,
        &skip,
        rustdoc_cfg.is_some(),
        bin_cfg,
        certifies,
        &oracle,
    )?;
    announce_package_rules(&active_sweeps, &selections);
    announce_feature_drops(&active_sweeps, &[&selections.clippy, &selections.rustdoc, &selections.test]);

    // Run every phase behind one closure so a failure from *any* of them
    // (not just the test phase) still funnels through the summary line below,
    // reporting the same total wall time a passing run does.
    //
    // `failing_phase` names the phase about to run and is cleared on
    // success, so after an Err it names the phase that failed - the one
    // per-phase datum the `--json` summary carries.
    let mut failing_phase: Option<Phase> = None;
    let background = BackgroundScripts::start(project_root, script_checks, skip(Phase::ScriptCheck));
    let run_phases = || -> Result<(), PhaseFailure> {
        run_convention_phases(
            &ConventionPhaseArgs {
                project_root,
                gremlins_cfg,
                header_cfg,
                textlint_rules,
                manifest_cfg,
                script_checks,
                dependency_rules,
                fix_gremlins,
                commands,
            },
            &skip,
            &mut failing_phase,
        )
        .map_err(|error| {
            PhaseFailure::for_phase(failing_phase.unwrap_or(Phase::Gremlins), error)
        })?;

        run_build_phases(
            &BuildPhaseArgs {
                project,
                project_root,
                script_checks,
                state_root,
                active_sweeps: &active_sweeps,
                selections: &selections,
                lints,
                serial_builds,
                quarantine,
                rustdoc_cfg,
                claim,
                doctests,
                commands,
                extra_args,
                background: &background,
            },
            &skip,
            &mut failing_phase,
            &mut ledger,
            timings.then_some(&mut collected_timings),
            &mut run_report,
        )
    };
    let mut outcome = run_phases();
    // Joined on every path, before the verdict and teardown. A background
    // entry sits at the front of the pipeline, so its failure names the
    // failed phase whatever later phase also failed.
    let joined = background.join();
    if background.failed() {
        failing_phase = Some(Phase::ScriptCheck);
    }
    // A `[cargo_env]` failure a tolerant path swallowed (a non-certifying
    // preparation, a universe retry) still fails the run, under the phase it
    // happened in. It was printed when it happened.
    if let Some((_, phase)) = crate::cargo_env::failure() {
        if outcome.is_ok() {
            outcome = Err(phase.map_or_else(
                || PhaseFailure::Printed(DevError::Reported("cargo_env failed".into())),
                PhaseFailure::Findings,
            ));
        }
        if !background.failed() {
            failing_phase = phase.or(failing_phase);
        }
    }
    match joined {
        Err(e) if outcome.is_ok() => outcome = Err(e.into()),
        // The run's own error is the one the summary prints; this one would
        // otherwise go unsaid.
        Err(e) if e.rendering_policy() != crate::error::RenderingPolicy::AlreadyPrinted && !matches!(e, DevError::Interrupted) => output::error(&e.to_string()),
        _ => {}
    }

    if timings {
        emit_timings(&collected_timings, active_sweeps.len() > 1);
    }

    let outcome = outcome.map_err(PhaseFailure::into_dev_error);

    let ran_labels = ledger.reached_labels(&active_sweeps);

    // The summary/trailer scope label: the CLI `-p` set, comma-joined so the
    // `--json` `package` field stays a string.
    let package_label = (!packages.is_empty()).then(|| packages.join(","));

    let stop = run_stop(&outcome, watchdog_fired().is_some(), crate::shutdown::is_shutdown_requested());
    finish_check(FinishInput {
        outcome: &outcome,
        stop,
        certifies,
        profile_label: &profile_label,
        sweep_labels: &ran_labels,
        lints,
        skip_phases,
        prose_only: !prose_skips.is_empty(),
        package: package_label.as_deref(),
        failing_phase,
        report: run_report,
        json,
        started,
    })
}

/// `check --textlint NAME` / `--script NAME`: run the named `[[textlint]]` rules
/// and `[[script_check]]` entries and nothing else, for a human iterating on one
/// failing gate. Every other phase is skipped, script checks run regardless of
/// their stage, and the run announces itself as a selection - it certifies
/// nothing, which is why clap refuses it beside `--gate`, a profile, `-p`, and
/// `--json` (a trailer a machine could read as a verdict).
///
/// A name matching no entry is an error listing the known names: a typo that
/// selected nothing would otherwise print a green run.
pub(crate) fn cmd_check_selected(
    project_root: &Path,
    textlint_rules: &[TextlintRule],
    script_checks: &[ScriptCheck],
    textlint_names: &[String],
    script_names: &[String],
) -> Result<(), DevError> {
    let rules = select_named(textlint_rules, textlint_names, |r| &r.name, "[[textlint]]")?;
    let checks = select_named(script_checks, script_names, |c| &c.name, "[[script_check]]")?;
    output::run_msg("selected entries only - not a gate; run `brokkr check` before committing");
    // As in `cmd_check`: a graceful kill unwinds as `Interrupted`.
    let _interrupts = crate::shutdown::SigtermGuard::install();

    let started = std::time::Instant::now();
    let textlint = run_textlint(&LazyTree::new(project_root), &rules);
    // Every stage in one pass: the selection names entries, and an entry's stage
    // only orders it around phases this run does not execute.
    let scripts = if checks.is_empty() {
        Ok(())
    } else {
        let mut checks = checks;
        for c in &mut checks {
            c.stage = Stage::PreClippy;
        }
        run_script_checks(project_root, &checks, Stage::PreClippy)
    };
    let elapsed = started.elapsed().as_secs_f64();
    if watchdog_fired().is_some() {
        return Err(DevError::ExitCode(WATCHDOG_EXIT_CODE));
    }
    if crate::shutdown::is_shutdown_requested()
        || matches!(scripts, Err(DevError::Interrupted))
        || matches!(textlint, Err(DevError::Interrupted))
    {
        return Err(DevError::Interrupted);
    }
    // Both halves always run, so every failure is accounted for - not whichever
    // came first, which would hide a script failure behind textlint's.
    let failed: Vec<DevError> = [textlint, scripts].into_iter().filter_map(Result::err).collect();
    if failed.is_empty() {
        output::run_msg(&format!("selection passed ({elapsed:.1}s)"));
        return Ok(());
    }
    selection_failure(&failed)
}

/// How a failed `--textlint`/`--script` selection ends. A `Reported` failure
/// printed its full report above, so its label is not echoed (`finish_check`
/// does the same); any other error carries its only diagnostic in its message,
/// so that is printed here. The exit is plain 1 either way, which `main` does
/// not decorate with an `[error]` line.
fn selection_failure(failed: &[DevError]) -> Result<(), DevError> {
    for e in failed {
        if e.rendering_policy() != crate::error::RenderingPolicy::AlreadyPrinted && !matches!(e, DevError::ExitCode(_)) {
            output::error(&e.to_string());
        }
    }
    Err(DevError::ExitCode(1))
}

/// The entries whose name is in `names`, in config order. Each requested name
/// must match at least one entry.
fn select_named<T: Clone>(
    entries: &[T],
    names: &[String],
    name_of: impl Fn(&T) -> &String,
    section: &str,
) -> Result<Vec<T>, DevError> {
    if let Some(missing) = names.iter().find(|n| !entries.iter().any(|e| name_of(e) == *n)) {
        let known: Vec<&str> = entries.iter().map(|e| name_of(e).as_str()).collect();
        let known = if known.is_empty() { "none defined".to_owned() } else { known.join(", ") };
        return Err(DevError::Config(format!("no {section} entry named {missing:?} (known: {known})")));
    }
    Ok(entries.iter().filter(|e| names.contains(name_of(e))).cloned().collect())
}

/// Log the profile and its sweep set. Run log only: the set is a function of
/// the config, identical run to run, and the verdict line carries the
/// profile and the count of sweeps that actually ran.
fn announce_profile_header(active_sweeps: &[PlannedSweep], profile_label: &Option<String>) {
    let labels: Vec<&str> = active_sweeps.iter().map(|s| s.label.as_str()).collect();
    let n = output::count(active_sweeps.len(), "sweep");
    let joined = labels.join(", ");
    match profile_label {
        Some(name) => output::detail(&format!("profile {name}: {n} ({joined})")),
        None => output::detail(&format!("{n} ({joined})")),
    }
}

/// Name what this invocation changed about the configured gate, up front and
/// on stdout, so a failure that follows reads against the run that actually
/// happened: a `-p` scope, forwarded `-- ...` args (a `--lib` or a `--skip`
/// can narrow the run drastically), and rustflags inherited from the calling
/// environment, which the test builds compose into every sweep. Silent when
/// the invocation changed nothing - the configured shape is not news.
///
/// Ad-hoc `--features`, profile `skip_phases` and the markdown-only shortcut
/// announce themselves on their own lines.
fn announce_invocation_shaping(
    project_root: &Path,
    packages: &[String],
    extra_args: &[String],
) {
    let mut parts: Vec<String> = Vec::new();
    if !packages.is_empty() {
        let scope: Vec<String> = packages.iter().map(|p| format!("-p {p}")).collect();
        parts.push(scope.join(" "));
    }
    if !extra_args.is_empty() {
        const MAX_ARGS: usize = 8;
        const MAX_ARG_CHARS: usize = 80;
        let shown: Vec<String> = extra_args
            .iter()
            .take(MAX_ARGS)
            .map(|arg| {
                let mut chars = arg.chars();
                let mut prefix: String = chars.by_ref().take(MAX_ARG_CHARS).collect();
                if chars.next().is_some() {
                    prefix.push_str("...");
                }
                format!("{prefix:?}")
            })
            .collect();
        let omitted = extra_args.len().saturating_sub(shown.len());
        let suffix = if omitted == 0 {
            String::new()
        } else {
            format!("; {} omitted", output::count(omitted, "arg"))
        };
        parts.push(format!("forwarded `-- {}`{suffix}", shown.join(" ")));
    }
    // The same resolver used by sink selection reports cargo's active source,
    // including a set-but-empty encoded value and any shadowed plain value.
    parts.extend(rustflags::RustflagsEnv::process().invocation_notes_for(project_root));
    if !parts.is_empty() {
        output::run_msg(&format!("invocation: {}", parts.join("; ")));
    }
}

/// Say once, up front, where the sweeps' package rules narrow a CLI `-p`
/// selection - instead of every phase printing its own `skipped` line for the
/// same sweep. Silent with no `-p`, or when every sweep admits every package.
///
/// Rendered from the run's selections, the ones every phase reads, so it
/// cannot disagree with them. Per phase it reports both what the rules
/// admitted (the dropped notes) and the lane's disposition: eligible,
/// excluded, deduped or not applicable. Eligible means planned, never "will
/// run" - an earlier failure or a stop can still end the run first. A phase
/// that does not run is left out. Sweeps group on the rendered text of every
/// phase.
fn announce_package_rules(sweeps: &[PlannedSweep], selections: &CheckSelections) {
    for line in package_rules_lines(sweeps, selections) {
        output::run_msg(&line);
    }
}

/// Say once per lane which configured feature tokens a narrowed run does not
/// carry, and which members route them - a `-p` run, a package-mode
/// resolution or a support build compiles fewer features than the sweep
/// declares, and that must not be silent. Silent when nothing was dropped.
pub(crate) fn announce_feature_drops(sweeps: &[PlannedSweep], phases: &[&PhaseSelection]) {
    for line in feature_drop_lines(sweeps, phases) {
        output::run_msg(&line);
    }
}

/// The lines of [`announce_package_rules`]; empty when it is silent.
fn package_rules_lines(sweeps: &[PlannedSweep], selections: &CheckSelections) -> Vec<String> {
    let phases = [&selections.clippy, &selections.rustdoc, &selections.test];
    let narrowing = phases.iter().any(|p| !p.overrides().is_empty());
    if !narrowing {
        return Vec::new();
    }
    let label = |i: LaneIdx| sweeps.get(i.get()).map_or("?", |s| s.label.as_str());
    let render = |entry: &LaneEntry| -> String {
        let not = |notes: &[AdmissionNote]| format!("not admitted - {}", join_notes(notes));
        match entry {
            LaneEntry::Disabled(_) => String::new(),
            LaneEntry::Attempt(a) if a.notes().is_empty() => "eligible".to_owned(),
            LaneEntry::Attempt(a) => {
                let kept = a.selection().packages().unwrap_or_default();
                let flags: Vec<String> = kept.iter().map(|p| format!("-p {p}")).collect();
                format!("eligible with {} ({})", flags.join(" "), not(a.notes()))
            }
            LaneEntry::Excluded(e) => format!("excluded ({})", not(e.notes())),
            LaneEntry::Deduped(d) if d.notes().is_empty() => format!("deduped onto {}", label(d.onto())),
            LaneEntry::Deduped(d) => {
                let kept = d.selection().packages().unwrap_or_default();
                let flags: Vec<String> = kept.iter().map(|p| format!("-p {p}")).collect();
                format!("deduped onto {} with {} ({})", label(d.onto()), flags.join(" "), not(d.notes()))
            }
            LaneEntry::NotApplicable(n) => format!("not applicable ({})", n.reason()),
        }
    };
    let mut groups: Vec<(String, Vec<&str>)> = Vec::new();
    for (i, s) in sweeps.iter().enumerate() {
        let entries: Vec<(&str, &LaneEntry)> = phases
            .iter()
            .filter_map(|p| if p.is_enabled() { p.entry(LaneIdx::new(i)).map(|e| (p.phase().name(), e)) } else { None })
            .collect();
        // Listed only where the rules actually narrowed something.
        if entries.iter().all(|(_, e)| e.notes().is_empty()) {
            continue;
        }
        let rendered: Vec<(&str, String)> = entries.iter().map(|&(n, e)| (n, render(e))).collect();
        let text = join_phase_clauses(rendered);
        match groups.iter_mut().find(|(k, _)| *k == text) {
            Some((_, labels)) => labels.push(&s.label),
            None => groups.push((text, vec![&s.label])),
        }
    }
    if groups.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        "package rules narrow this selection (per phase; eligible means planned - an earlier failure can still stop the run first):"
            .to_owned(),
    ];
    for (text, labels) in &groups {
        lines.push(format!("  {}: {text}", labels.join(", ")));
    }
    lines
}

/// One sweep's per-phase renderings as one text: the bare rendering when every
/// phase says the same, else `phase: text` clauses joined by ` | `, adjacent
/// phases saying the same thing sharing one clause.
fn join_phase_clauses(rendered: Vec<(&str, String)>) -> String {
    if rendered.windows(2).all(|w| w[0].1 == w[1].1) {
        return rendered.into_iter().next().map(|(_, t)| t).unwrap_or_default();
    }
    let mut clauses: Vec<(Vec<&str>, String)> = Vec::new();
    for (name, t) in rendered {
        if let Some((names, last)) = clauses.last_mut()
            && *last == t
        {
            names.push(name);
            continue;
        }
        clauses.push((vec![name], t));
    }
    clauses.iter().map(|(n, t)| format!("{}: {t}", n.join(", "))).collect::<Vec<_>>().join(" | ")
}

/// Name what an ad-hoc CLI-features run inherited, and from where.
///
/// An ad-hoc run reports no profile in the header (it certifies nothing and
/// claims no `certifies`), which left the run's filters unstated: the only
/// way to tell "these tests failed" from "these tests were never meant to
/// run here" was to stash the diff and run again. One line closes that.
///
/// Silent on the non-ad-hoc path - the profile header already says it. Not
/// gated on `--commands`: that flag adds the cargo lines and takes nothing away,
/// and this line is the only statement of which test filters an ad-hoc run
/// inherited.
fn announce_adhoc_shaping(
    features: &[String],
    no_default_features: bool,
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
) -> Result<(), DevError> {
    if features.is_empty() && !no_default_features {
        return Ok(());
    }
    let shaping = match (test_cfg, effective_profile_name(test_cfg, profile_name)?) {
        (Some(cfg), Some(name)) => Some((name.clone(), profile::run_shaping(cfg, &name)?)),
        _ => None,
    };
    let detail = match &shaping {
        Some((name, s)) if !s.is_empty() => format!("run shaping from profile {name}"),
        Some((name, _)) => format!("profile {name} shapes nothing - no test filters applied"),
        None => "no profile - no test filters applied".to_owned(),
    };
    output::run_msg(&format!(
        "ad-hoc features: sweep selection overridden, {detail}"
    ));
    Ok(())
}

/// A narrowed run must never look like a full one in the log: name the
/// skipped phases up front, not only in the trailer.
fn announce_skipped_phases(skip_phases: &[String]) {
    if !skip_phases.is_empty() {
        output::run_msg(&format!(
            "skipping phases: {} (certifies partial)",
            skip_phases.join(", ")
        ));
    }
}

/// Decide the phases to skip because the working tree holds documentation
/// edits and nothing else. Empty means "run everything", which is the answer
/// whenever the shortcut cannot be justified.
///
/// `build_requested` is the caller's waiver: `--force-rust`, `--gate`, a profile, or
/// any flag that shapes the build. A shortcut that could quietly skip the
/// certifying run would be worse than no shortcut at all - `--gate` has to mean
/// the same thing on every tree it is ever run on.
fn prose_only_skips(project_root: &Path, build_requested: bool) -> Vec<Phase> {
    if build_requested || scope::dirt(project_root) != scope::Dirt::ProseOnly {
        return Vec::new();
    }
    let skipped: Vec<Phase> = Phase::ALL
        .iter()
        .copied()
        .filter(|phase| phase.skippable() && !phase.reads_prose())
        .collect();
    let kept = Phase::ALL
        .iter()
        .copied()
        .filter(|phase| phase.reads_prose())
        .map(Phase::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    output::run_msg(&format!(
        "markdown-only tree: running {kept} (--force-rust to check the build too)"
    ));
    skipped
}

/// A captured `cargo` launched by `check`/`test`/`clippy`: the cargo spawn
/// profile, naming the phase in flight so a `[cargo_env]` failure is
/// attributed to it.
fn run_check_cargo(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<output::CapturedOutput, DevError> {
    output::run_cargo(args, cwd, env, current_phase())
}

/// Point the verdict's failing phase at `phase` and restart the watchdog's
/// phase clock for it.
fn start_phase(failing_phase: &mut Option<Phase>, phase: Phase) {
    begin_phase(failing_phase, phase);
}

/// Everything the seven convention phases (gremlins through dependency
/// rules) need, bundled so `cmd_check` stays readable.
struct ConventionPhaseArgs<'a> {
    project_root: &'a Path,
    gremlins_cfg: Option<&'a GremlinsConfig>,
    header_cfg: Option<&'a HeaderConfig>,
    textlint_rules: &'a [TextlintRule],
    manifest_cfg: Option<&'a ManifestConfig>,
    script_checks: &'a [ScriptCheck],
    dependency_rules: &'a [DependencyRule],
    fix_gremlins: bool,
    commands: bool,
}

/// Run the convention phases in order, honouring `skip_phases` and
/// keeping `failing_phase` pointed at the phase in flight.
fn run_convention_phases(
    a: &ConventionPhaseArgs<'_>,
    skip: &dyn Fn(Phase) -> bool,
    failing_phase: &mut Option<Phase>,
) -> Result<(), DevError> {
    // The seven phases share one green line: each is seconds at most and
    // "ok" plus its counts is all a pass has to say.
    let started = std::time::Instant::now();
    conventions_open();
    let result = run_convention_phases_inner(a, skip, failing_phase);
    conventions_close(result.is_ok(), started.elapsed());
    result
}

fn run_convention_phases_inner(
    a: &ConventionPhaseArgs<'_>,
    skip: &dyn Fn(Phase) -> bool,
    failing_phase: &mut Option<Phase>,
) -> Result<(), DevError> {
    // One working-tree snapshot for every file scanner below.
    let tree = LazyTree::new(a.project_root);

    if skip(Phase::Gremlins) {
        audit_skipped_scopes(Phase::Gremlins, &tree, a, failing_phase)?;
    } else {
        start_phase(failing_phase, Phase::Gremlins);
        run_gremlins(&tree, a.gremlins_cfg, a.fix_gremlins)?;
    }

    if skip(Phase::Header) {
        audit_skipped_scopes(Phase::Header, &tree, a, failing_phase)?;
    } else {
        start_phase(failing_phase, Phase::Header);
        run_header(&tree, a.header_cfg)?;
    }

    if skip(Phase::Textlint) {
        audit_skipped_scopes(Phase::Textlint, &tree, a, failing_phase)?;
    } else {
        start_phase(failing_phase, Phase::Textlint);
        run_textlint(&tree, a.textlint_rules)?;
    }

    if skip(Phase::Manifest) {
        audit_skipped_scopes(Phase::Manifest, &tree, a, failing_phase)?;
    } else {
        start_phase(failing_phase, Phase::Manifest);
        run_manifest(&tree, a.manifest_cfg)?;
    }

    if !skip(Phase::ScriptCheck) {
        start_phase(failing_phase, Phase::ScriptCheck);
        run_script_checks(a.project_root, a.script_checks, Stage::PreClippy)?;
    }

    if !skip(Phase::DependencyRules) {
        start_phase(failing_phase, Phase::DependencyRules);
        run_dependency_rules(a.project_root, a.dependency_rules, a.commands)?;
    }

    if !skip(Phase::PublishCycle) {
        start_phase(failing_phase, Phase::PublishCycle);
        run_publish_cycle(a.project_root, a.commands)?;
    }
    Ok(())
}

fn audit_skipped_scopes(
    phase: Phase,
    tree: &LazyTree<'_>,
    args: &ConventionPhaseArgs<'_>,
    failing_phase: &mut Option<Phase>,
) -> Result<(), DevError> {
    let configured = match phase {
        Phase::Gremlins => args
            .gremlins_cfg
            .is_some_and(|config| !config.disable && !config.exclude.is_empty()),
        Phase::Header => args.header_cfg.is_some(),
        Phase::Textlint => !args.textlint_rules.is_empty(),
        Phase::Manifest => args.manifest_cfg.is_some(),
        _ => false,
    };
    if !configured {
        return Ok(());
    }
    // Bound the scope lookup as this phase without announcing the skipped scan as a run.
    *failing_phase = Some(phase);
    enter_phase(phase);
    let result = conventions::warn_skipped_scopes(
        phase,
        tree,
        args.gremlins_cfg,
        args.header_cfg,
        args.textlint_rules,
        args.manifest_cfg,
    );
    if result.is_ok() {
        *failing_phase = None;
    }
    result
}

struct BuildPhaseArgs<'a> {
    project: Option<Project>,
    project_root: &'a Path,
    /// The full entry list; the build phases run the `pre-test` and
    /// `post-test` slices of it around the test phase.
    script_checks: &'a [ScriptCheck],
    /// Config-dir root where brokkr's own `.brokkr` state lives. Equals
    /// `project_root` except under the one-level-up foreign-checkout layout,
    /// where cargo runs in `project_root` (the code tree) but hung-test
    /// snapshots must stay out of the foreign repo.
    state_root: &'a Path,
    active_sweeps: &'a [PlannedSweep],
    /// Which lanes each phase may attempt, with which packages - aligned with
    /// `active_sweeps` - and the install-feature phase's package set.
    selections: &'a CheckSelections,
    /// `[lints]`: `allow`, suppressed via `-A` on every sweep, and the sited
    /// `allow_exact` list, filtered at JSON ingestion.
    lints: &'a LintsConfig,
    /// One cargo chain for every build phase (`[test] serial_builds`, or a
    /// configured cargo build dir), resolved once for the run.
    serial_builds: bool,
    quarantine: &'a [QuarantineEntry],
    /// `[rustdoc]`; `None` leaves the rustdoc phase inert.
    rustdoc_cfg: Option<&'a RustdocConfig>,
    claim: Claim,
    doctests: bool,
    commands: bool,
    extra_args: &'a [String],
    /// Joined before the prepare phase lists anything.
    background: &'a BackgroundScripts,
}

/// What a run reports beyond its verdict, for the `--json` trailer.
#[derive(Default)]
struct RunReport {
    /// How the run stopped, when something stopped it.
    termination: Option<TerminationSummary>,
    /// Under a complete claim: the policy, execution-accounting and doctest
    /// blocks, present even when the run failed.
    accounting: Option<AccountingBlocks>,
    /// What the run left unresolved. Built from the plan and journal of ANY
    /// run that has an inventory; reported when the run failed.
    continuation: Option<ContinuationReport>,
}

/// The prepared profile and where its record lives on disk.
struct Prepared {
    prep: ProfilePrep,
    /// `None` when the plan could not be persisted; the audit then reports a
    /// record that does not exist rather than one it never read.
    paths: Option<AccountingPaths>,
    /// The test phase started. A run that stopped in preparation executed
    /// nothing, so its plan's executions are unobserved because nothing ran,
    /// not because a stop interrupted them - there is nothing to continue.
    test_phase_ran: bool,
}

/// Run clippy, the test phase and - under a `complete` claim - the `prepare`
/// phase before the tests and the coverage audit after them. Threads the
/// reach ledger (which sweeps each phase group started work on), the
/// `failing_phase` pointer and the run report the summary carries even on a
/// failing run.
#[allow(clippy::too_many_arguments)]
fn run_build_phases(
    a: &BuildPhaseArgs<'_>,
    skip: &dyn Fn(Phase) -> bool,
    failing_phase: &mut Option<Phase>,
    ledger: &mut ReachLedger,
    collected_timings: Option<&mut Vec<TestTiming>>,
    run_report: &mut RunReport,
) -> Result<(), PhaseFailure> {
    // A refusal here prints through `finish_check`, like every error that is
    // not `DevError::Reported`.
    verify_doc_only_rules(a)?;
    run_diagnostic_phases(a, skip, failing_phase, ledger)?;

    if !skip(Phase::ScriptCheck) {
        start_phase(failing_phase, Phase::ScriptCheck);
        run_script_checks(a.project_root, a.script_checks, Stage::PreTest)?;
    }

    let certifying = a.claim.requires_attribution();
    let mut test_failure: Option<PhaseFailure> = None;
    let mut prepared: Option<Prepared> = None;
    if !skip(Phase::Test) {
        // A `-p` that rules every lane out of the test phase is refused
        // before preparation, under the test phase's name: a run that will
        // test nothing compiles nothing for it.
        if a.selections.test.refusal().is_some() {
            start_phase(failing_phase, Phase::Test);
            a.selections.test.check_refusal()?;
        }
        // Every run prepares: the execution inventory is what a stop is
        // reported against, certifying or not. Only the certifying claim
        // makes a preparation failure fatal.
        start_phase(failing_phase, Phase::Prepare);
        let p = run_prepare_phase(a, certifying)?;
        let proceed = if certifying {
            p.prep.error.is_none() && p.prep.plan.complete && p.paths.is_some()
        } else {
            // A lane without an inventory still runs. Only a stop ends the
            // run here.
            !crate::shutdown::is_shutdown_requested()
        };
        if !proceed {
            // Nothing runs on an incomplete plan under a claim: a test phase
            // that ran anyway would produce a record nothing could certify.
            // The audit below still reports what the plan knows.
            test_failure = Some(PhaseFailure::Findings(Phase::Prepare));
        }
        prepared = Some(p);
        if test_failure.is_none() {
            if let Some(p) = prepared.as_mut() {
                p.test_phase_ran = true;
            }
            start_phase(failing_phase, Phase::Test);
            let lanes = prepared.as_ref().map(|p| &p.prep.lanes);
            test_failure = run_test_phase(
                &TestPhaseArgs {
                    project: a.project.clone(),
                    project_root: a.project_root,
                    state_root: a.state_root,
                    sweeps: a.active_sweeps,
                    selection: &a.selections.test,
                    doctests: a.doctests,
                    commands: a.commands,
                    extra_args: a.extra_args,
                    lints: a.lints,
                    certifying,
                },
                lanes,
                collected_timings,
                ledger,
                &mut run_report.termination,
            )
            .err()
            .map(|error| match error {
                // Red test outcomes have already printed their failure list;
                // operational errors still need the ordinary phase rendering.
                TestPhaseFailure::TestsFailed => {
                    PhaseFailure::Printed(DevError::Reported("tests failed".into()))
                }
                TestPhaseFailure::Error(other) => other.into(),
            });
        }
        // The test phase is over, however it ended: the journal says so. A
        // journal without this line was cut short.
        if prepared.as_ref().is_some_and(|p| p.paths.is_some()) {
            journal_close();
        }
    }
    finish_build_phases(a, skip, failing_phase, prepared, run_report, test_failure)
}

/// The `prepare` phase: plan the whole invocation, persist the plan, open the
/// journal. A lane that cannot be prepared is reported here, in full, and
/// the plan comes back incomplete rather than not at all.
///
/// Under a certifying claim that is a failure (the markers print as errors and
/// the test phase does not run). Otherwise it is a lane without an inventory:
/// the markers go to the run log, since a lane that is unavailable by design
/// would otherwise warn on every run, and the lane still runs.
fn run_prepare_phase(a: &BuildPhaseArgs<'_>, certifying: bool) -> Result<Prepared, DevError> {
    let info = build::project_info(Some(a.project_root))?;
    let owners = OwnerIndex::from_metadata_at(
        &info.workspace_root,
        a.project_root,
        &info.workspace_members,
        &info.member_manifests,
    );
    let target_dir = info.target_dir;
    let allow_flags = crate::config::check::test_phase_allow_flags(a.lints);
    let inputs = LaneInputs {
        project: a.project.clone(),
        project_root: a.project_root,
        state_root: a.state_root,
        target_dir: &target_dir,
        allow_flags: &allow_flags,
        owners: &owners,
        commands: a.commands,
        certifying,
    };
    // Two steps: every lane built (target-dir chains, concurrent), then every
    // lane listed one at a time with nothing compiling. The background
    // script checks may compile, so they are joined in between.
    let builds = build_profile(&inputs, a.active_sweeps, &a.selections.test, a.extra_args, a.serial_builds);
    a.background.join()?;
    let mut preparer = CargoPreparer::new(inputs, a.extra_args, builds);
    let prep = prepare_profile(a.active_sweeps, &a.selections.test, a.doctests, certifying, &mut preparer);
    for marker in &prep.plan.incomplete {
        if certifying {
            output::error(&format!("prepare: {marker}"));
        } else {
            output::detail(&format!("prepare: {marker}"));
        }
    }
    let paths = match accounting_open(a.state_root, &prep.plan) {
        Ok(p) => {
            output::detail(&format!(
                "prepare: run {}, plan {}, journal {}",
                prep.plan.run_id,
                p.plan.display(),
                p.journal.display()
            ));
            Some(p)
        }
        Err(e) => {
            // The run goes on without a record, and says so: its stop, if it
            // has one, cannot be reported from it.
            output::error(&format!("prepare: the plan could not be persisted: {e}"));
            None
        }
    };
    if prep.error.is_none() && prep.plan.complete {
        let executions: usize = prep.plan.lanes.iter().map(|l| l.inventory.executions().len()).sum();
        let line = format!(
            "prepare: {}, {} in {}",
            output::count(prep.plan.lanes.len(), "lane"),
            output::count(executions, "expected execution"),
            fmt_wall(phase_elapsed())
        );
        // A certifying run states what it planned; any other run's inventory
        // is bookkeeping, for the log.
        if certifying {
            output::run_msg(&line);
        } else {
            output::detail(&line);
        }
    }
    Ok(Prepared { prep, paths, test_phase_ran: false })
}

/// The two per-build-shape diagnostic phases: clippy, then rustdoc. Rustdoc
/// comes second because it reuses clippy's compiled dependencies, and before
/// the tests because a doc link is cheaper to fail on than a test suite.
fn run_diagnostic_phases(
    a: &BuildPhaseArgs<'_>,
    skip: &dyn Fn(Phase) -> bool,
    failing_phase: &mut Option<Phase>,
    ledger: &mut ReachLedger,
) -> Result<(), PhaseFailure> {
    if !skip(Phase::Clippy) {
        start_phase(failing_phase, Phase::Clippy);
        run_clippy_phase(
            a.project_root,
            a.active_sweeps,
            &a.selections.clippy,
            a.lints,
            Reporter::Check { commands: a.commands },
            &mut |i| ledger.reach_diagnostics(LaneIdx::new(i)),
            false,
            a.serial_builds,
        )?;
    }

    if !skip(Phase::Rustdoc) {
        start_phase(failing_phase, Phase::Rustdoc);
        run_rustdoc_phase(
            a.project_root,
            a.rustdoc_cfg,
            a.active_sweeps,
            &a.selections.rustdoc,
            a.lints,
            Reporter::Check { commands: a.commands },
            &mut |i| ledger.reach_diagnostics(LaneIdx::new(i)),
            a.serial_builds,
        )?;
    }
    Ok(())
}

/// Everything after the test phase: the coverage audit under a complete
/// claim, the test verdict, the post-test script checks and install-feature.
fn finish_build_phases(
    a: &BuildPhaseArgs<'_>,
    skip: &dyn Fn(Phase) -> bool,
    failing_phase: &mut Option<Phase>,
    prepared: Option<Prepared>,
    run_report: &mut RunReport,
    test_failure: Option<PhaseFailure>,
) -> Result<(), PhaseFailure> {
    // The audit runs only under a complete claim - it is what the claim buys -
    // and it runs on every outcome of the test phase, a watchdog kill
    // included: it is pure reconciliation of the plan and the journal, so it
    // needs no process the shutdown flag would refuse, and the worksheet is
    // most needed exactly on the unhealthy runs.
    if let Some(prepared) = prepared {
        if a.claim.enumerates_universe() {
            // Stays on the phase that failed - the audit only contributes its
            // findings and counts there.
            if test_failure.is_none() {
                start_phase(failing_phase, Phase::Coverage);
            }
            let audit = audit_coverage(&prepared, a.active_sweeps, a.quarantine, test_failure.is_none());
            // Counts first, verdict second: the summary carries them even when
            // the audit is what failed.
            if run_report.termination.is_none() {
                run_report.termination = audit.termination;
            }
            run_report.accounting = Some(audit.blocks);
            run_report.continuation = audit.continuation;
            audit.result?;
        } else {
            // No claim to audit, but the inventory and the journal are as real:
            // what a stop left unresolved is read from them the same way.
            run_report.continuation = continuation_of(&prepared);
        }
    }

    if let Some(e) = test_failure {
        return Err(e);
    }

    // Only past a green test phase: the test phase fails fast, so a post-test
    // gate on a failing run would be judging a tree whose later lanes never
    // ran. Unlike the coverage audit above, which is deliberately best-effort
    // there, a script-check has no partial-run reading - it just lies.
    //
    // "Green" means the phase ran and passed, not merely that it left no
    // error: a skipped test phase (`skip_phases = ["test"]`, or the
    // markdown-only shortcut) also leaves `test_failure` empty, and a
    // post-test check run there judges a test phase that never happened.
    // Named, not silent - a narrowed run must never look like a full one.
    if !skip(Phase::ScriptCheck) {
        if skip(Phase::Test) {
            let held = a.script_checks.iter().filter(|c| c.stage == Stage::PostTest).count();
            if held > 0 {
                output::run_msg(&format!(
                    "script-check: {} skipped (the test phase did not run)",
                    output::count(held, "post-test check")
                ));
            }
        } else {
            start_phase(failing_phase, Phase::ScriptCheck);
            run_script_checks(a.project_root, a.script_checks, Stage::PostTest)?;
        }
    }

    // Last on purpose: package-mode resolution can compile duplicate variants
    // of shared dependencies, making this the most expensive phase on a cold
    // store, and the repo's ordering rule is cheap-fails-first.
    if !skip(Phase::InstallFeature) {
        start_phase(failing_phase, Phase::InstallFeature);
        run_install_feature_phase(
            a.project_root,
            &a.selections.install,
            &crate::config::check::test_phase_allow_flags(a.lints),
            a.commands,
        )?;
    }

    *failing_phase = None;
    Ok(())
}

/// The doc-only rules that need resolved sweeps, the CLI args, or the tree -
/// checked before anything compiles:
///
/// - A run containing a doc-only sweep refuses forwarded cargo target
///   selectors: `--doc` is exclusive in cargo, and stripping the selector for
///   one sweep while honouring it for the rest would silently change scope.
/// - A doc-only sweep never takes the process-isolated or `test_threads`
///   paths - the first enumerates test binaries it does not have, the second
///   is untested against doctest JSON events - so profile-level run shaping
///   reaching one is refused.
/// - Under `certifies = "complete"`, a doc-only sweep may inherit no filters
///   from its profile: doctests cannot be enumerated, so an inherited
///   `skip`/`only` could never be audited for liveness and a dead one would
///   silently reshape the doctest run under a complete claim. Compose the
///   doc twin as its own filterless lane.
/// - Under `certifies = "complete"` with a doctest obligation (`[test]
///   doctests = true`, or a doc-only sweep standing in for the doctests
///   quarantine), some referenced sweep must be a WORKSPACE-SHAPED doctest
///   carrier: `packages` empty, and either `test_exclude_packages` non-empty
///   (which forces an explicit `--workspace`, the exclusion reported like
///   every other declared narrowing) or the bare selection confirmed to be
///   the whole workspace. A package-scoped doc twin would prove "some
///   doctests ran" while the rest of the workspace's doctests silently
///   stopped - the audit cannot see doctests, so this structural rule is the
///   only guard.
fn verify_doc_only_rules(a: &BuildPhaseArgs<'_>) -> Result<(), DevError> {
    // Every configured doc-only sweep, for the structural carrier rule below.
    let is_doc_only = |s: &PlannedSweep| matches!(s.lane_exec(), LaneExec::DocOnly { .. });
    let doc_sweeps: Vec<&PlannedSweep> =
        a.active_sweeps.iter().filter(|s| is_doc_only(s)).collect();
    // The execution-compatibility rules judge only the doc-only lanes this
    // invocation will attempt: a lane the Test selection excludes (an
    // out-of-scope `-p`) or a disabled Test phase never runs `--doc`, so it
    // cannot conflict with a forwarded selector or a run shape.
    let attempted: Vec<&PlannedSweep> = a
        .active_sweeps
        .iter()
        .enumerate()
        .filter(|(i, s)| is_doc_only(s) && a.selections.test.attempt(LaneIdx::new(*i)).is_some())
        .map(|(_, s)| s)
        .collect();
    if !attempted.is_empty() {
        let (selectors, _) = ForwardedArgs::parse(a.extra_args).partition_targets();
        if !selectors.is_empty() {
            return Err(DevError::Config(format!(
                "sweep '{}' is doc-only (`cargo test --doc`, an exclusive selector), and the \
                 forwarded args carry the target selector(s) {}. Drop them, or run a profile \
                 without the doc-only sweep.",
                attempted[0].label,
                selectors.join(" ")
            )));
        }
        for s in &attempted {
            // Isolation cannot reach a doc-only lane at all (`LaneExec`
            // resolution refuses the combination); delegated threads can.
            if s.lane_exec().threads().is_some_and(Threads::is_cargo_mediated) {
                return Err(DevError::Config(format!(
                    "sweep '{}' is doc-only but its profile sets `isolation = \"process\"` or \
                     `test_threads`; a doc-only sweep runs serially through cargo's own `--doc` \
                     invocation. Put the doc twin in its own lane without run shaping.",
                    s.label
                )));
            }
        }
    }
    if !a.claim.enumerates_universe() {
        return Ok(());
    }
    for s in &doc_sweeps {
        let filters = s.lane_filters();
        if !filters.filters.is_empty() || !filters.targets.is_empty() || filters.include_ignored
        {
            return Err(DevError::Config(format!(
                "sweep '{}' is doc-only and inherits `skip`/`only`/`tests` filters from its \
                 profile, under a \"complete\" claim. Doctests cannot be enumerated, so those \
                 filters could never be audited for liveness. Compose the doc twin as its own \
                 lane without filters.",
                s.label
            )));
        }
    }
    if a.doctests || !doc_sweeps.is_empty() {
        let whole =
            build::project_info(Some(a.project_root))?.bare_selection_is_whole_workspace;
        let carrier = a.active_sweeps.iter().any(|s| {
            let carries = lane_runs_doctests(s, a.doctests);
            let workspace_shaped =
                s.packages.is_empty() && (!s.test_exclude_packages.is_empty() || whole);
            carries && workspace_shaped
        });
        if !carrier {
            return Err(DevError::Config(
                "this \"complete\" profile claims doctest coverage but no referenced sweep \
                 carries workspace doctests: every doctest-capable lane is package-scoped or \
                 narrower than the workspace. Add a workspace-shaped `doc_only = true` \
                 [[check]] entry to the profile (its `packages` empty; `test_exclude_packages` \
                 is allowed and reported), or a serial libtest sweep of that shape."
                    .into(),
            ));
        }
    }
    Ok(())
}

/// What the audit produced: the summary blocks, how the run stopped as the
/// journal records it, and the audit's own verdict.
struct AuditOutcome {
    blocks: AccountingBlocks,
    termination: Option<TerminationSummary>,
    /// What the run left unresolved, when anything.
    continuation: Option<ContinuationReport>,
    result: Result<(), PhaseFailure>,
}

/// Read a prepared run's journal back and reconcile the plan against it.
/// Pure: spawns nothing, builds nothing, arms no deadline.
fn reconcile_prepared(prepared: &Prepared) -> (Vec<JournalRecord>, Reconciliation) {
    reconcile_run(&prepared.prep.plan, prepared.paths.as_ref().map(|p| p.journal.as_path()))
}

/// The continuation report of a run no claim audits. `None` when the run left
/// nothing unresolved - or left no record to continue from.
fn continuation_of(prepared: &Prepared) -> Option<ContinuationReport> {
    prepared.paths.as_ref()?;
    if !prepared.test_phase_ran {
        return None;
    }
    let (records, recon) = reconcile_prepared(prepared);
    build_continuation(&prepared.prep.plan, &recon, &records)
}

/// The coverage audit for a complete claim: pure reconciliation of the plan
/// and the journal it reads back from disk. Spawns nothing, builds nothing,
/// arms no deadline - so it runs after a watchdog kill, where the shutdown
/// flag refuses every new process.
///
/// On a green test phase the audit can fail the run two ways: policy (an
/// orphan, a stale quarantine entry, a dead filter) and accounting (an
/// execution not passed, an anomaly, a journal cut short - the green
/// invariant). On a failed test phase the failure is the run's verdict and
/// the audit contributes its worksheet and counts.
fn audit_coverage(
    prepared: &Prepared,
    sweeps: &[PlannedSweep],
    quarantine: &[QuarantineEntry],
    tests_green: bool,
) -> AuditOutcome {
    let plan = &prepared.prep.plan;
    let (records, recon) = reconcile_prepared(prepared);
    let (policy, policy_failed) = report_policy(plan, sweeps, quarantine);
    let accounting = ExecutionAccounting::of(&recon);
    let continuation = if prepared.test_phase_ran {
        prepared.paths.as_ref().and_then(|_| build_continuation(plan, &recon, &records))
    } else {
        None
    };
    // A failed run's continuation block opens with the unresolved count and
    // the expected total; the counts line would restate them.
    let restated = !tests_green
        && recon.anomalies.is_empty()
        && continuation.as_ref().is_some_and(|c| !c.candidates.is_empty());
    if prepared.test_phase_ran {
        report_accounting(plan, &recon, &accounting, tests_green, restated);
    } else {
        // Preparation refused the run: nothing was attempted, so reconciling
        // would call every planned execution "unobserved" and every doctest
        // carrier unfinished - failures the run never had. The preparation
        // markers above are the verdict; this says what was held back.
        output::error(&format!(
            "accounting: the test phase did not run (preparation failed) - {} not attempted",
            output::count(accounting.expected_executions, "expected execution")
        ));
    }

    let label = |lane: usize| plan.lane(LaneIdx::new(lane)).map(|l| l.label.clone());
    let termination = recon.first_termination.as_ref().map(|t| TerminationSummary::of(t, label));
    let result = if !tests_green {
        Ok(())
    } else if policy_failed || !recon.green() {
        Err(PhaseFailure::Findings(Phase::Coverage))
    } else {
        Ok(())
    };
    AuditOutcome {
        blocks: AccountingBlocks {
            policy_coverage: policy,
            execution_accounting: accounting,
            doctests: DoctestAccounting {
                inventory: "unavailable",
                accounting: "unknown",
                observed: recon.doctests.clone(),
            },
        },
        termination,
        // The same reconciliation the verdict used, so the list of what is
        // unresolved can never disagree with the counts above it.
        continuation,
        result,
    }
}

/// The accounting worksheet: one line of counts, every anomaly, and - when a
/// green test phase still fails the invariant, which is a hard failure that
/// must explain itself - every execution that did not pass.
fn report_accounting(
    plan: &AccountingPlan,
    recon: &Reconciliation,
    counts: &ExecutionAccounting,
    tests_green: bool,
    restated: bool,
) {
    let line = format!(
        "accounting: {} - {} passed, {} failed, {} timed out, {} interrupted, {} ignored, {} \
         unobserved, {} anomalies ({})",
        output::count(counts.expected_executions, "expected execution"),
        counts.passed,
        counts.failed,
        counts.timed_out,
        counts.interrupted,
        counts.ignored,
        counts.unobserved,
        counts.anomalies,
        counts.status
    );
    if recon.green() {
        output::run_msg(&line);
        return;
    }
    // `restated`: the continuation block prints the counts that matter
    // (unresolved of expected); the full worksheet line goes to the run log.
    if restated {
        output::detail(&line);
    } else {
        output::error(&line);
    }
    for a in &recon.anomalies {
        let lane = a
            .lane
            .and_then(|l| plan.lane(l))
            .map_or_else(String::new, |l| format!(" [{}]", l.label));
        output::error(&format!("anomaly {:?}{lane}: {}", a.kind, a.detail));
    }
    if !recon.journal_closed {
        output::error("accounting: the journal has no closing record - the run's record was cut short");
    }
    for e in &recon.journal_errors {
        output::error(&format!("accounting: {e}"));
    }
    for s in &recon.truncated_streams {
        output::error(&format!("accounting: {s} - its record may be short, so it cannot be whole"));
    }
    for c in recon.doctests.carriers.iter().filter(|c| !c.completed) {
        output::error(&format!(
            "doctests: carrier {} is not known to have completed ({} rustdoc stream{} seen) - \
             a planned carrier must run its doctests to the end",
            c.lane,
            c.streams,
            if c.streams == 1 { "" } else { "s" }
        ));
    }
    if tests_green {
        for x in recon.accounted.iter().filter(|x| x.outcome != Outcome::Passed) {
            let lane = plan.lane(x.id.lane).map_or("?", |l| l.label.as_str());
            let detail = x.detail.map_or_else(String::new, |d| format!(" ({})", d.as_str()));
            output::error(&format!(
                "{}{detail}: {lane}/{}/{}",
                x.outcome.as_str(),
                x.id.pair.unit.id(),
                x.id.pair.test
            ));
        }
    }
}

/// The resolved profile's claim and skip-phase list. `(None, [])` for
/// ad-hoc, legacy, and profile-less runs.
fn profile_claim<'a>(
    profile_label: &Option<String>,
    test_cfg: Option<&'a TestConfig>,
) -> (Option<Certifies>, &'a [String]) {
    let def = profile_label
        .as_ref()
        .and_then(|name| test_cfg.and_then(|c| c.profiles.get(name.as_str())));
    (
        def.and_then(|d| d.certifies),
        def.and_then(|d| d.skip_phases.as_deref()).unwrap_or(&[]),
    )
}

/// Resolve `--gate` through `[test].gate_profile`, which load-time
/// validation guarantees names a `certifies = "complete"` profile.
fn resolve_gate_profile(
    gate: bool,
    test_cfg: Option<&TestConfig>,
) -> Result<Option<String>, DevError> {
    if !gate {
        return Ok(None);
    }
    match test_cfg.and_then(|c| c.gate_profile.clone()) {
        Some(name) => Ok(Some(name)),
        None => Err(DevError::Config(
            "`brokkr check --gate` requires `[test] gate_profile = \"<name>\"` \
             in brokkr.toml, naming a profile with `certifies = \"complete\"`."
                .into(),
        )),
    }
}

/// The ordinary (non-parallel, non-isolated) test lane: one `cargo test` per
/// cargo RESOLUTION.
///
/// A package-mode sweep tests each of its packages alone, because a batched
/// multi-`-p` run resolves a graph none of them install under. Every other mode
/// yields a single unscoped resolution, so this is one iteration and the argv
/// is what it always was.
///
/// Under a complete profile `prepared` is the lane's plan: each resolution's
/// harnesses are then held to it through the strict shim.
#[allow(clippy::too_many_arguments)]
fn run_sequential_resolutions(
    project_root: &Path,
    state_root: &Path,
    sweep: &PlannedSweep,
    attempt: &Attempt,
    extra_args: &[String],
    env: &LaneEnv,
    owners: &OwnerIndex,
    // (doctests, multi, commands, certifying)
    flags: (bool, bool, bool, bool),
    mut timings: Option<&mut Vec<TestTiming>>,
    prepared: Option<&PreparedLane>,
    tap: &LaneTap,
) -> Result<bool, DevError> {
    let mut all_passed = true;
    for run in attempt.runs() {
        let resolution = run.resolution.clone();
        let plan = prepared.map(|p| SerialPlan {
            resolution: resolution.clone(),
            units: p.units_by_path(&resolution),
            hashes: p.hashes_by_path(&resolution),
            rustdoc: lane_runs_doctests(sweep, flags.0),
            strict: flags.3,
            expected: p
                .resolutions
                .iter()
                .filter(|r| r.resolution == resolution)
                .flat_map(|r| r.binaries.iter())
                .filter(|b| b.executed(p.include_ignored).next().is_some())
                .map(|b| (b.binary.executable.clone(), b.unit.clone()))
                .collect(),
        });
        let passed = run_one_test_sweep(
            project_root,
            state_root,
            sweep,
            run,
            extra_args,
            &env.project_env,
            owners,
            &env.allow_args,
            flags.0,
            flags.1,
            flags.2,
            timings.as_deref_mut(),
            &SerialObserve { tap, plan },
        )?;
        // Keep going rather than returning on the first red package: the phase
        // reports every failure it can reach in one run, and stopping here
        // would hide a second package's failures behind the first, exactly as
        // `--no-fail-fast` exists to prevent one binary hiding another.
        all_passed &= passed;
    }
    Ok(all_passed)
}

/// Sweep selection followed immediately by unification resolution - one step,
/// because only a `PlannedSweep` may reach a phase: a bare declaration carries
/// no unification answer, and a phase would build cargo commands, and hash a
/// build shape, without one.
fn active_sweeps_resolved(
    check_entries: &[CheckEntry],
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
    // (ad-hoc `--features`, `--no-default-features`)
    shaping: (&[String], bool),
    // (project root, CLI `-p` scope, forwarded args)
    invocation: (&Path, &[String], &[String]),
) -> Result<Vec<PlannedSweep>, DevError> {
    let sweeps =
        decide_active_sweeps(check_entries, test_cfg, profile_name, shaping.0, shaping.1)?;
    // The selection module is the one resolution point for promotion,
    // unification and the forwarded-selector refusals (`selection.rs`).
    resolve_invocation_sweeps(sweeps, invocation.0, invocation.1, &ForwardedArgs::parse(invocation.2))
}

/// The certifies permission table governs CLI flags too: `-p` scopes the
/// build, and a scoped green is not comparable to the full green (feature
/// unification changes with the package set), so a
/// complete profile rejects it before anything compiles.
fn reject_scoped_complete(
    claim: Claim,
    packages: &[String],
) -> Result<(), DevError> {
    if claim.refuses_scoping() && !packages.is_empty() {
        return Err(DevError::Config(
            "package scoping (`-p`) is rejected under `certifies = \"complete\"`: \
             a scoped build's green is not comparable to the full build's. Use a \
             partial profile (or a profile without `certifies`) for scoped runs."
                .into(),
        ));
    }
    Ok(())
}

/// Trailing `brokkr check -- …` args can change the test invocation without
/// becoming part of the declared selection. Libtest filters are reflected in
/// the lane listing, but cargo-side selectors are not, so a complete claim
/// rejects all forwarded args before its plan can disagree with execution.
/// Reject trailing args under a complete claim, exactly like
/// `-p`: this closes the hole for both `--gate` and a `complete`
/// `default_profile`, and for the ordinary-lane profiles that
/// `run_isolated_sweep`'s per-sweep guard never covers.
fn reject_extra_args_complete(
    claim: Claim,
    extra_args: &[String],
) -> Result<(), DevError> {
    if claim.refuses_trailing_args() && !extra_args.is_empty() {
        return Err(DevError::Config(
            "trailing `-- …` test args are rejected under `certifies = \
             \"complete\"`: they narrow the test run (a libtest `--skip`, a \
             cargo `--lib`) but not the plan built from the sweeps' own \
             filters, so the plan would expect tests that never ran. Use a \
             partial profile for ad-hoc narrowing, or fold the selection into \
             a `[[check]]` entry."
                .into(),
        ));
    }
    Ok(())
}

/// Assemble the `cargo clippy` argv for one sweep. Always
/// `--message-format=json` so the lint code is populated on every diagnostic
/// (cargo's pretty stderr only annotates the first occurrence per crate),
/// `--keep-going` so a failing unit doesn't hide lints queued behind it, and
/// `--cap-lints=warn` so a deny-level lint still yields `.rmeta` and every
/// downstream crate is checked - brokkr recovers the intent by treating any
/// surfaced diagnostic as a hard failure at the call site. `allow` is the
/// `[lints] allow` list, emitted as `-A <lint>` so the suppressed lints
/// never reach the diagnostic stream (no carve-outs in the fail decision).
#[cfg(test)]
mod select_named_tests {
    #![allow(clippy::unwrap_used)]
    use super::select_named;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn keeps_config_order_and_only_named_entries() {
        let entries = names(&["a", "b", "c"]);
        let got = select_named(&entries, &names(&["c", "a"]), |e| e, "[[x]]").unwrap();
        assert_eq!(got, names(&["a", "c"]));
    }

    #[test]
    fn an_unknown_name_is_an_error_listing_the_known_ones() {
        let entries = names(&["a", "b"]);
        let err = select_named(&entries, &names(&["a", "zz"]), |e| e, "[[x]]").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"zz\""), "{msg}");
        assert!(msg.contains("a, b"), "{msg}");
    }

    #[test]
    fn a_failed_selection_exits_one_without_an_error_echo_of_its_reports() {
        use super::{DevError, selection_failure};
        let reported = [DevError::Reported("textlint failed".into()), DevError::Reported("script-check failed".into())];
        assert!(matches!(selection_failure(&reported), Err(DevError::ExitCode(1))));
        let mixed = [DevError::Reported("textlint failed".into()), DevError::Config("scan failed".into())];
        assert!(matches!(selection_failure(&mixed), Err(DevError::ExitCode(1))));
    }

    #[test]
    fn no_entries_at_all_says_so() {
        let err = select_named(&Vec::<String>::new(), &names(&["a"]), |e| e, "[[x]]").unwrap_err();
        assert!(err.to_string().contains("none defined"));
    }
}

#[cfg(test)]
mod ran_labels_tests {
    #![allow(clippy::unwrap_used)]

    use super::{LaneIdx, ReachLedger};
    use crate::profile::{PlannedSweep, SweepDecl};

    fn sweep(label: &str) -> PlannedSweep {
        PlannedSweep::from(SweepDecl {
            label: label.into(),
            ..Default::default()
        })
    }

    fn ledger(sweeps: &[PlannedSweep], diagnostics: &[bool], test: &[bool]) -> ReachLedger {
        let mut l = ReachLedger::new(sweeps);
        for (i, d) in diagnostics.iter().enumerate() {
            if *d {
                l.reach_diagnostics(LaneIdx::new(i));
            }
        }
        for (i, t) in test.iter().enumerate() {
            if *t {
                l.reach_test(LaneIdx::new(i));
            }
        }
        l
    }

    #[test]
    fn omits_sweeps_skipped_in_both_phases() {
        // Profile `tier1` selects [default, ffi, live]; a `-p` scope
        // rules ffi/live out of both clippy and the test phase, so only
        // `default` ran. The trailer must not list ffi/live as green.
        let sweeps = [sweep("default"), sweep("ffi"), sweep("live")];
        let l = ledger(&sweeps, &[true, false, false], &[true, false, false]);
        assert_eq!(l.reached_labels(&sweeps), vec!["default"]);
    }

    #[test]
    fn unions_clippy_only_and_test_only_lanes() {
        // A lane can run in one phase but not the other: `a` clippy-checked
        // with its tests skipped, `b` deduped in clippy but its tests still
        // ran, `c` reached by neither. The honest set is the union of the two.
        let sweeps = [sweep("a"), sweep("b"), sweep("c")];
        let l = ledger(&sweeps, &[true, false, false], &[false, true, false]);
        assert_eq!(l.reached_labels(&sweeps), vec!["a", "b"]);
    }

    #[test]
    fn all_green_reports_every_sweep_in_order() {
        let sweeps = [sweep("default"), sweep("ffi")];
        let l = ledger(&sweeps, &[true, true], &[true, true]);
        assert_eq!(l.reached_labels(&sweeps), vec!["default", "ffi"]);
    }

    #[test]
    fn nothing_ran_reports_empty() {
        // A failure in an early convention phase (e.g. gremlins) returns
        // before any sweep runs, so the trailer honestly lists none.
        let sweeps = [sweep("default"), sweep("ffi")];
        assert!(ledger(&sweeps, &[false, false], &[false, false]).reached_labels(&sweeps).is_empty());
    }
}


#[cfg(test)]
mod package_rules_tests {
    #![allow(clippy::unwrap_used)]

    use super::{package_rules_lines, CheckSelections, FeatureOracle};
    use crate::profile::{PlannedSweep, SweepDecl};

    fn sweep(label: &str, packages: &[&str], excluded: &[&str]) -> PlannedSweep {
        PlannedSweep::from(SweepDecl {
            label: label.into(),
            packages: packages.iter().map(|p| (*p).to_owned()).collect(),
            test_exclude_packages: excluded.iter().map(|p| (*p).to_owned()).collect(),
            ..Default::default()
        })
    }

    /// The announcement for a run with CLI `-p` set `cli`, every phase but
    /// rustdoc enabled (no `[rustdoc]` table).
    fn lines(sweeps: &[PlannedSweep], cli: &[&str]) -> Vec<String> {
        let cli: Vec<String> = cli.iter().map(|p| (*p).to_owned()).collect();
        let oracle = FeatureOracle::unavailable();
        let selections = CheckSelections::build(sweeps, &cli, &|_| false, false, None, None, &oracle).unwrap();
        package_rules_lines(sweeps, &selections)
    }

    #[test]
    fn silent_without_a_cli_selection_or_any_narrowing() {
        let sweeps = vec![sweep("default", &[], &[]), sweep("vm", &["other"], &[])];
        assert!(lines(&sweeps, &[]).is_empty());
        assert!(lines(&sweeps[..1], &["a"]).is_empty());
    }

    #[test]
    fn sweeps_with_one_outcome_share_a_line() {
        let sweeps = vec![
            sweep("default", &[], &[]),
            sweep("vm", &["other"], &[]),
            sweep("runner", &["other"], &[]),
        ];
        assert_eq!(
            lines(&sweeps, &["a"])[1..],
            ["  vm, runner: excluded (not admitted - -p a is not in this sweep's packages list)"]
        );
    }

    #[test]
    fn phases_are_named_apart_when_they_differ() {
        let sweeps = vec![sweep("default", &[], &["a"])];
        assert_eq!(
            lines(&sweeps, &["a", "b"])[1..],
            ["  default: clippy: eligible | test: eligible with -p b (not admitted - -p a is in \
              this sweep's test_exclude_packages)"]
        );
    }

    #[test]
    fn a_deduped_lane_names_the_lane_it_folds_onto() {
        let sweeps = vec![sweep("tier1/ffi", &["a", "b"], &[]), sweep("tier2/ffi", &["a", "b"], &[])];
        assert_eq!(
            lines(&sweeps, &["a", "x"])[1..],
            [
                "  tier1/ffi: eligible with -p a (not admitted - -p x is not in this sweep's packages list)",
                "  tier2/ffi: clippy: deduped onto tier1/ffi with -p a (not admitted - -p x is not in this \
                 sweep's packages list) | test: eligible with -p a (not admitted - -p x is not in this sweep's \
                 packages list)",
            ]
        );
    }
}

#[cfg(test)]
mod complete_rejection_tests {
    #![allow(clippy::unwrap_used)]

    use super::{Claim, reject_extra_args_complete, reject_scoped_complete};
    use crate::config::Certifies;

    #[test]
    fn complete_rejects_trailing_args() {
        // `--gate -- -- --skip expensive_` (or `-- --lib`) narrows the
        // real run but not the plan, so the skipped pairs would stay expected
        // with nothing running them. Rejected before anything compiles.
        let args = vec!["--".to_owned(), "--skip".to_owned(), "expensive_".to_owned()];
        let err = reject_extra_args_complete(Claim::new(Some(Certifies::Complete)), &args)
            .unwrap_err()
            .to_string();
        assert!(err.contains("trailing"), "got: {err}");
        assert!(err.contains("complete"), "got: {err}");
    }

    #[test]
    fn complete_allows_no_trailing_args() {
        reject_extra_args_complete(Claim::new(Some(Certifies::Complete)), &[]).unwrap();
    }

    #[test]
    fn partial_and_legacy_allow_trailing_args() {
        let args = vec!["--lib".to_owned()];
        reject_extra_args_complete(Claim::new(Some(Certifies::Partial)), &args).unwrap();
        reject_extra_args_complete(Claim::new(None), &args).unwrap();
    }

    #[test]
    fn scoped_complete_still_rejected() {
        // Sanity: the sibling `-p` guard is unchanged.
        let pkgs = vec!["pkg".to_owned()];
        assert!(reject_scoped_complete(Claim::new(Some(Certifies::Complete)), &pkgs).is_err());
        reject_scoped_complete(Claim::new(Some(Certifies::Complete)), &[]).unwrap();
        reject_scoped_complete(Claim::new(Some(Certifies::Partial)), &pkgs).unwrap();
    }
}


#[cfg(test)]
mod continuation_gate_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A prepared run whose lane expects two tests and whose journal holds
    /// `records`.
    fn prepared(name: &str, ran: bool, records: &[JournalRecord]) -> Prepared {
        let dir = crate::test_scratch::scratch("phase", name);
        let journal = dir.join("journal.jsonl");
        let body: String = records.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
        std::fs::write(&journal, body).unwrap();
        let b = test_binary_for_tests("core", "test", "suite");
        let unit = BinaryUnit::of(&b);
        let pair = |t: &str| PairId {
            shape: "s".into(),
            resolution: Resolution::Combined,
            unit: unit.clone(),
            test: t.into(),
        };
        let lane = LaneRecord {
            inventory: Inventory::Prepared {
                executions: vec![pair("a"), pair("b")],
                ignored_selected: Vec::new(),
                outside_claim: Vec::new(),
                artifacts: Vec::new(),
                unlisted: Vec::new(),
            },
            ..LaneRecord::empty(LaneIdx::new(0), "default".into(), LaneKind::Serial, "s".into())
        };
        let plan = AccountingPlan { run_id: "1700000000000-1".into(), complete: true, lanes: vec![lane], ..AccountingPlan::default() };
        let sweeps = [PlannedSweep::from(crate::profile::SweepDecl::default())];
        Prepared {
            prep: ProfilePrep { plan, lanes: Lanes::from_sweeps(&sweeps, |_, _| None), error: None },
            paths: Some(AccountingPaths { plan: dir.join("plan.json"), journal }),
            test_phase_ran: ran,
        }
    }

    /// A run that stopped in preparation executed nothing: its executions are
    /// unobserved because nothing ran, so it has no continuation. One whose
    /// test phase started and was killed does.
    #[test]
    fn only_a_run_whose_test_phase_started_has_a_continuation() {
        let killed = [JournalRecord::LaneStarted { lane: 0 }, JournalRecord::Terminated(Termination {
            scope: TerminationScope::Run,
            lane: None,
            stream: None,
            cause: TerminationCause::PhaseDeadline,
            test: None,
            charged: None,
        })];
        assert!(continuation_of(&prepared("gate_ran", true, &killed)).is_some());
        assert!(continuation_of(&prepared("gate_not_ran", false, &killed)).is_none());
    }

    /// A run that persisted no plan has no record to continue from.
    #[test]
    fn a_run_without_a_persisted_plan_has_no_continuation() {
        let mut p = prepared("gate_no_plan", true, &[]);
        p.paths = None;
        assert!(continuation_of(&p).is_none());
    }

    /// A sweep labelled after its phase names the phase once.
    #[test]
    fn a_sweep_labelled_after_its_phase_does_not_stutter() {
        assert_eq!(phase_sweep_tag(Phase::Clippy, "clippy"), "clippy");
        assert_eq!(phase_sweep_tag(Phase::Clippy, "default"), "clippy default");
    }
}

