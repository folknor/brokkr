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
use crate::config::{
    Certifies, CheckEntry, DependencyRule, GremlinsConfig, HeaderConfig,
    ManifestConfig, RustdocConfig, NON_SKIPPABLE_PHASES, PHASE_NAMES, QuarantineEntry, ScriptCheck, SitedAllow,
    Stage, TestConfig, TextlintRule,
};
use crate::rustflags;
use crate::error::DevError;
use crate::output;
use crate::profile::{self, DeclaredFilter, FilterKind, LaneExec, ResolvedSweep, Threads};
use crate::project::Project;
use crate::scope;
use crate::rustc_text::{self, BlockKind, Level};
use crate::test_runner::{self, LibtestOutcome};

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
    clippy_allow: &[String],
    clippy_allow_exact: &[SitedAllow],
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
    let _scope = RunScope::begin();
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
    reject_scoped_complete(certifies, packages)?;
    reject_extra_args_complete(certifies, extra_args)?;

    announce_profile_header(&active_sweeps, &profile_label);
    announce_invocation_shaping(packages, extra_args);
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
    // trailer's `sweeps` list (S3-33): a sweep runs in clippy/rustdoc, in the
    // test phase, or both, and the trailer lists the union. Two flags rather
    // than one because clippy runs first - folding it into the test flag
    // would mark lanes an earlier test-phase fail-fast never reached, the
    // exact bug S3-18 fixed. Policy coverage never reads this - it reads the
    // plan.
    let mut ledger = ReachLedger::new(active_sweeps.len());
    // Doctests off unless `[test] doctests = true` (nextest/CI never runs them).
    let doctests = test_cfg.is_some_and(|c| c.doctests);
    set_serial_builds(project_root, test_cfg.is_some_and(|c| c.serial_builds), &active_sweeps);

    // A partial profile may skip phases (validated against PHASE_NAMES at
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
    let skip =
        |phase: &str| skip_phases.iter().any(|s| s == phase) || prose_skips.contains(&phase);

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
    let mut failing_phase: Option<&'static str> = None;
    let background = BackgroundScripts::start(project_root, script_checks, skip("script_check"));
    let mut run_phases = || -> Result<(), DevError> {
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
        )?;

        run_build_phases(
            &BuildPhaseArgs {
                project,
                project_root,
                script_checks,
                state_root,
                active_sweeps: &active_sweeps,
                selections: &selections,
                clippy_allow,
                clippy_allow_exact,
                quarantine,
                rustdoc_cfg,
                certifies,
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
        failing_phase = Some("script_check");
    }
    // A `[cargo_env]` failure a tolerant path swallowed (a non-certifying
    // preparation, a universe retry) still fails the run, under the phase it
    // happened in. It was printed when it happened.
    if let Some((_, phase)) = crate::cargo_env::failure() {
        if outcome.is_ok() {
            outcome = Err(DevError::Reported("cargo_env failed".into()));
        }
        if !background.failed() {
            failing_phase = phase.or(failing_phase);
        }
    }
    match joined {
        Err(e) if outcome.is_ok() => outcome = Err(e),
        // The run's own error is the one the summary prints; this one would
        // otherwise go unsaid.
        Err(e) if !e.already_printed() && !matches!(e, DevError::Interrupted) => output::error(&e.to_string()),
        _ => {}
    }

    if timings {
        emit_timings(&collected_timings, active_sweeps.len() > 1);
    }

    let ran_labels = ledger.reached_labels(&active_sweeps);

    // The summary/trailer scope label: the CLI `-p` set, comma-joined so the
    // `--json` `package` field stays a string.
    let package_label = (!packages.is_empty()).then(|| packages.join(","));

    let stop = run_stop(&outcome, watchdog_fired().is_some(), crate::shutdown::is_shutdown_requested());
    finish_check(
        &outcome,
        stop,
        certifies,
        &profile_label,
        &ran_labels,
        (clippy_allow, clippy_allow_exact),
        skip_phases,
        !prose_skips.is_empty(),
        package_label.as_deref(),
        failing_phase,
        run_report,
        json,
        started,
    )
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
    let textlint = run_textlint(project_root, &rules);
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
        if !e.already_printed() && !matches!(e, DevError::ExitCode(_)) {
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
fn announce_profile_header(active_sweeps: &[ResolvedSweep], profile_label: &Option<String>) {
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
fn announce_invocation_shaping(packages: &[String], extra_args: &[String]) {
    let mut parts: Vec<String> = Vec::new();
    if !packages.is_empty() {
        let scope: Vec<String> = packages.iter().map(|p| format!("-p {p}")).collect();
        parts.push(scope.join(" "));
    }
    if !extra_args.is_empty() {
        parts.push(format!("forwarded `-- {}`", extra_args.join(" ")));
    }
    // Presence, not content: cargo reads a set-but-empty variable as a live
    // (empty) flag source that shadows every config-file rustflags table, so
    // the project's own `-Dwarnings`/linker flags silently drop out. That is
    // exactly the kind of shaping this line exists to name.
    for var in ["CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"] {
        match std::env::var_os(var) {
            Some(v) if v.to_string_lossy().trim().is_empty() => parts.push(format!(
                "{var} set but empty in the environment (cargo ignores config rustflags)"
            )),
            Some(_) => parts.push(format!("{var} inherited from the environment")),
            None => {}
        }
    }
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
fn announce_package_rules(sweeps: &[ResolvedSweep], selections: &CheckSelections) {
    for line in package_rules_lines(sweeps, selections) {
        output::run_msg(&line);
    }
}

/// Say once per lane which configured feature tokens a narrowed run does not
/// carry, and which members route them - a `-p` run, a package-mode
/// resolution or a support build compiles fewer features than the sweep
/// declares, and that must not be silent. Silent when nothing was dropped.
pub(crate) fn announce_feature_drops(sweeps: &[ResolvedSweep], phases: &[&PhaseSelection]) {
    for line in feature_drop_lines(sweeps, phases) {
        output::run_msg(&line);
    }
}

/// The lines of [`announce_package_rules`]; empty when it is silent.
fn package_rules_lines(sweeps: &[ResolvedSweep], selections: &CheckSelections) -> Vec<String> {
    let phases = [&selections.clippy, &selections.rustdoc, &selections.test];
    let narrowing = phases.iter().any(|p| !p.overrides().is_empty());
    if !narrowing {
        return Vec::new();
    }
    let label = |i: usize| sweeps.get(i).map_or("?", |s| s.label.as_str());
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
            .filter_map(|p| if p.is_enabled() { p.entry(i).map(|e| (p.phase().name(), e)) } else { None })
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

/// The phases the prose-only shortcut keeps. Everything else in
/// [`PHASE_NAMES`] is skipped when nothing but markdown is uncommitted.
///
/// These three are the ones that read prose. `header` and `manifest` are not
/// here: they police source files and manifests, which the run has just
/// established nobody touched.
const PROSE_PHASES: [&str; 3] = ["gremlins", "textlint", "script_check"];

/// Decide the phases to skip because the working tree holds documentation
/// edits and nothing else. Empty means "run everything", which is the answer
/// whenever the shortcut cannot be justified.
///
/// `build_requested` is the caller's waiver: `--force-rust`, `--gate`, a profile, or
/// any flag that shapes the build. A shortcut that could quietly skip the
/// certifying run would be worse than no shortcut at all - `--gate` has to mean
/// the same thing on every tree it is ever run on.
fn prose_only_skips(project_root: &Path, build_requested: bool) -> Vec<&'static str> {
    if build_requested || scope::dirt(project_root) != scope::Dirt::ProseOnly {
        return Vec::new();
    }
    let skipped: Vec<&'static str> = PHASE_NAMES
        .iter()
        .copied()
        .filter(|p| !PROSE_PHASES.contains(p) && !NON_SKIPPABLE_PHASES.contains(p))
        .collect();
    output::run_msg(&format!(
        "markdown-only tree: running {} (--force-rust to check the build too)",
        PROSE_PHASES.join(", ")
    ));
    skipped
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
    skip: &dyn Fn(&str) -> bool,
    failing_phase: &mut Option<&'static str>,
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
    skip: &dyn Fn(&str) -> bool,
    failing_phase: &mut Option<&'static str>,
) -> Result<(), DevError> {
    if !skip("gremlins") {
        begin_phase(failing_phase, "gremlins");
        run_gremlins(a.project_root, a.gremlins_cfg, a.fix_gremlins)?;
    }

    if !skip("header") {
        begin_phase(failing_phase, "header");
        run_header(a.project_root, a.header_cfg)?;
    }

    if !skip("textlint") {
        begin_phase(failing_phase, "textlint");
        run_textlint(a.project_root, a.textlint_rules)?;
    }

    if !skip("manifest") {
        begin_phase(failing_phase, "manifest");
        run_manifest(a.project_root, a.manifest_cfg)?;
    }

    if !skip("script_check") {
        begin_phase(failing_phase, "script_check");
        run_script_checks(a.project_root, a.script_checks, Stage::PreClippy)?;
    }

    if !skip("dependency_rules") {
        begin_phase(failing_phase, "dependency_rules");
        run_dependency_rules(a.project_root, a.dependency_rules, a.commands)?;
    }

    if !skip("publish_cycle") {
        begin_phase(failing_phase, "publish_cycle");
        run_publish_cycle(a.project_root, a.commands)?;
    }
    Ok(())
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
    active_sweeps: &'a [ResolvedSweep],
    /// Which lanes each phase may attempt, with which packages - aligned with
    /// `active_sweeps` - and the install-feature phase's package set.
    selections: &'a CheckSelections,
    /// The `[clippy] allow` lint list, suppressed via `-A` on every sweep.
    clippy_allow: &'a [String],
    /// The `[clippy] allow_exact` sited list, filtered at JSON ingestion.
    clippy_allow_exact: &'a [SitedAllow],
    quarantine: &'a [QuarantineEntry],
    /// `[rustdoc]`; `None` leaves the rustdoc phase inert.
    rustdoc_cfg: Option<&'a RustdocConfig>,
    certifies: Option<Certifies>,
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
    skip: &dyn Fn(&str) -> bool,
    failing_phase: &mut Option<&'static str>,
    ledger: &mut ReachLedger,
    collected_timings: Option<&mut Vec<TestTiming>>,
    run_report: &mut RunReport,
) -> Result<(), DevError> {
    // A refusal here prints through `finish_check`, like every error that is
    // not `DevError::Reported`.
    verify_doc_only_rules(a)?;
    run_diagnostic_phases(a, skip, failing_phase, ledger)?;

    if !skip("script_check") {
        begin_phase(failing_phase, "script_check");
        run_script_checks(a.project_root, a.script_checks, Stage::PreTest)?;
    }

    let certifying = a.certifies == Some(Certifies::Complete);
    let mut test_failure: Option<DevError> = None;
    let mut prepared: Option<Prepared> = None;
    if !skip("test") {
        // A `-p` that rules every lane out of the test phase is refused
        // before preparation, under the test phase's name: a run that will
        // test nothing compiles nothing for it.
        if a.selections.test.refusal().is_some() {
            begin_phase(failing_phase, "test");
            a.selections.test.check_refusal()?;
        }
        // Every run prepares: the execution inventory is what a stop is
        // reported against, certifying or not. Only the certifying claim
        // makes a preparation failure fatal.
        begin_phase(failing_phase, "prepare");
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
            test_failure = Some(DevError::Reported("preparation failed".into()));
        }
        prepared = Some(p);
        if test_failure.is_none() {
            if let Some(p) = prepared.as_mut() {
                p.test_phase_ran = true;
            }
            begin_phase(failing_phase, "test");
            let lanes = prepared.as_ref().map(|p| p.prep.lanes.as_slice());
            test_failure = run_test_phase(
                &TestPhaseArgs {
                    project: a.project,
                    project_root: a.project_root,
                    state_root: a.state_root,
                    sweeps: a.active_sweeps,
                    selection: &a.selections.test,
                    doctests: a.doctests,
                    commands: a.commands,
                    extra_args: a.extra_args,
                    allow: a.clippy_allow,
                    allow_exact: a.clippy_allow_exact,
                    certifying,
                },
                lanes,
                collected_timings,
                ledger,
                &mut run_report.termination,
            )
            .err();
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
    let target_dir = build::project_info(Some(a.project_root))?.target_dir;
    let allow_flags = crate::config::test_phase_allow_flags(a.clippy_allow, a.clippy_allow_exact);
    let inputs = LaneInputs {
        project: a.project,
        project_root: a.project_root,
        state_root: a.state_root,
        target_dir: &target_dir,
        allow_flags: &allow_flags,
        commands: a.commands,
        certifying,
    };
    // Two steps: every lane built (target-dir chains, concurrent), then every
    // lane listed one at a time with nothing compiling. The background
    // script checks may compile, so they are joined in between.
    let builds = build_profile(&inputs, a.active_sweeps, &a.selections.test, a.extra_args);
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
        let executions: usize = prep.plan.lanes.iter().map(|l| l.executions.len()).sum();
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
    skip: &dyn Fn(&str) -> bool,
    failing_phase: &mut Option<&'static str>,
    ledger: &mut ReachLedger,
) -> Result<(), DevError> {
    if !skip("clippy") {
        begin_phase(failing_phase, "clippy");
        run_clippy_phase(
            a.project_root,
            a.active_sweeps,
            &a.selections.clippy,
            a.clippy_allow,
            a.clippy_allow_exact,
            a.commands,
            &mut |i| ledger.reach_diagnostics(i),
            false,
            false,
        )?;
    }

    if !skip("rustdoc") {
        begin_phase(failing_phase, "rustdoc");
        run_rustdoc_phase(
            a.project_root,
            a.rustdoc_cfg,
            a.active_sweeps,
            &a.selections.rustdoc,
            a.clippy_allow,
            a.clippy_allow_exact,
            a.commands,
            &mut |i| ledger.reach_diagnostics(i),
        )?;
    }
    Ok(())
}

/// Everything after the test phase: the coverage audit under a complete
/// claim, the test verdict, the post-test script checks and install-feature.
fn finish_build_phases(
    a: &BuildPhaseArgs<'_>,
    skip: &dyn Fn(&str) -> bool,
    failing_phase: &mut Option<&'static str>,
    prepared: Option<Prepared>,
    run_report: &mut RunReport,
    test_failure: Option<DevError>,
) -> Result<(), DevError> {
    // The audit runs only under a complete claim - it is what the claim buys -
    // and it runs on every outcome of the test phase, a watchdog kill
    // included: it is pure reconciliation of the plan and the journal, so it
    // needs no process the shutdown flag would refuse, and the worksheet is
    // most needed exactly on the unhealthy runs.
    if let Some(prepared) = prepared {
        if a.certifies == Some(Certifies::Complete) {
            // Stays on the phase that failed - the audit only contributes its
            // findings and counts there.
            if test_failure.is_none() {
                begin_phase(failing_phase, "coverage");
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
        return Err(already_reported(e, TESTS_FAILED));
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
    if !skip("script_check") {
        if skip("test") {
            let held = a.script_checks.iter().filter(|c| c.stage == Stage::PostTest).count();
            if held > 0 {
                output::run_msg(&format!(
                    "script-check: {} skipped (the test phase did not run)",
                    output::count(held, "post-test check")
                ));
            }
        } else {
            begin_phase(failing_phase, "script_check");
            run_script_checks(a.project_root, a.script_checks, Stage::PostTest)?;
        }
    }

    // Last on purpose: package-mode resolution can compile duplicate variants
    // of shared dependencies, making this the most expensive phase on a cold
    // store, and the repo's ordering rule is cheap-fails-first.
    if !skip("install_feature") {
        begin_phase(failing_phase, "install_feature");
        run_install_feature_phase(
            a.project_root,
            &a.selections.install,
            &crate::config::test_phase_allow_flags(a.clippy_allow, a.clippy_allow_exact),
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
    let is_doc_only = |s: &ResolvedSweep| matches!(s.lane_exec(), LaneExec::DocOnly { .. });
    let doc_sweeps: Vec<&ResolvedSweep> =
        a.active_sweeps.iter().filter(|s| is_doc_only(s)).collect();
    // The execution-compatibility rules judge only the doc-only lanes this
    // invocation will attempt: a lane the Test selection excludes (an
    // out-of-scope `-p`) or a disabled Test phase never runs `--doc`, so it
    // cannot conflict with a forwarded selector or a run shape.
    let attempted: Vec<&ResolvedSweep> = a
        .active_sweeps
        .iter()
        .enumerate()
        .filter(|(i, s)| is_doc_only(s) && a.selections.test.attempt(*i).is_some())
        .map(|(_, s)| s)
        .collect();
    if !attempted.is_empty() {
        let (cargo_extra, _) = split_extra_args(a.extra_args);
        let (selectors, _) = partition_target_selectors(cargo_extra);
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
    if a.certifies != Some(Certifies::Complete) {
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
    result: Result<(), DevError>,
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
    sweeps: &[ResolvedSweep],
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

    let label = |lane: usize| plan.lane(lane).map(|l| l.label.clone());
    let termination = recon.first_termination.as_ref().map(|t| TerminationSummary::of(t, label));
    let result = if !tests_green {
        Ok(())
    } else if policy_failed {
        Err(DevError::Reported("coverage failed".into()))
    } else if !recon.green() {
        Err(DevError::Reported("execution accounting violated the green invariant".into()))
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
    sweep: &ResolvedSweep,
    attempt: &Attempt,
    extra_args: &[String],
    env: &LaneEnv,
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
/// because a `ResolvedSweep` whose `effective_unification` has not been filled
/// in yet is not safe to hand to a phase: it would build cargo commands, and
/// hash a build shape, from the unresolved default.
fn active_sweeps_resolved(
    check_entries: &[CheckEntry],
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
    // (ad-hoc `--features`, `--no-default-features`)
    shaping: (&[String], bool),
    // (project root, CLI `-p` scope, forwarded args)
    invocation: (&Path, &[String], &[String]),
) -> Result<Vec<ResolvedSweep>, DevError> {
    let mut sweeps =
        decide_active_sweeps(check_entries, test_cfg, profile_name, shaping.0, shaping.1)?;
    resolve_sweep_unification(&mut sweeps, invocation.0, invocation.1, invocation.2)?;
    Ok(sweeps)
}

/// Turn every sweep's configured `feature_unification` policy into the answer
/// this invocation uses, once, before any phase builds a cargo command.
///
/// One resolution point is the whole design. Clippy, the pre-build, the test
/// run, the parallel prebuild, each per-binary re-entry and the coverage
/// enumeration must all agree on the graph they are talking about; deciding it
/// separately in each is how an audit ends up describing a build that never
/// ran. Storing the answer on the sweep also puts it in `build_shape_key`,
/// which is what stops a promoted and an unpromoted lane deduping into one.
///
/// The `cargo metadata` call that answers "is the bare selection the whole
/// workspace" is paid only when some sweep could actually be promoted - the
/// ordinary run does not gain a subprocess for a question it never asks.
fn resolve_sweep_unification(
    sweeps: &mut [ResolvedSweep],
    project_root: &Path,
    packages: &[String],
    extra_args: &[String],
) -> Result<(), DevError> {
    let cli_scope: Vec<&str> = packages.iter().map(String::as_str).collect();
    let (cargo_extra, _) = split_extra_args(extra_args);
    // Only a parallel lane is ever promoted, and only one whose selection is
    // unscoped in every channel. Everything else resolves without asking.
    let any_candidate = sweeps.iter().any(|s| {
        matches!(s.lane_exec(), LaneExec::Parallel { .. })
            && unify_workspace(s, &cli_scope, true, cargo_extra)
    });
    let whole_workspace = if any_candidate {
        build::project_info(Some(project_root))?.bare_selection_is_whole_workspace
    } else {
        false
    };

    for sweep in sweeps.iter_mut() {
        let eligible = matches!(sweep.lane_exec(), LaneExec::Parallel { .. })
            && unify_workspace(sweep, &cli_scope, whole_workspace, cargo_extra);
        sweep.effective_unification = profile::resolve_unification(sweep, eligible)?;
        // Refused here rather than at each phase: the forwarded args are one
        // invocation-wide input, so one refusal covers clippy, the pre-build,
        // the test run and the audit, and none of them can be reached with a
        // selection brokkr did not plan.
        reject_forwarded_selectors(sweep, cargo_extra)?;
    }
    Ok(())
}

/// The certifies permission table governs CLI flags too: `-p` scopes the
/// build, and a scoped green is not comparable to the full green (feature
/// unification changes with the package set - the B41 hazard), so a
/// complete profile rejects it before anything compiles.
fn reject_scoped_complete(
    certifies: Option<Certifies>,
    packages: &[String],
) -> Result<(), DevError> {
    if certifies == Some(Certifies::Complete) && !packages.is_empty() {
        return Err(DevError::Config(
            "package scoping (`-p`) is rejected under `certifies = \"complete\"`: \
             a scoped build's green is not comparable to the full build's. Use a \
             partial profile (or a profile without `certifies`) for scoped runs."
                .into(),
        ));
    }
    Ok(())
}

/// Trailing `brokkr check -- …` args narrow the real test run - a libtest
/// `--skip` drops tests, a cargo `--lib` drops integration binaries - but
/// the plan takes each lane's selection from the sweep's own filters alone,
/// without them. Those narrowed-away pairs would be selected and expected
/// while nothing ran them, so the run could never be accounted - and before
/// the plan existed, the audit certified `complete` over exactly those tests.
/// Reject trailing args under a complete claim, exactly like
/// `-p`: this closes the hole for both `--gate` and a `complete`
/// `default_profile`, and for the ordinary-lane profiles that
/// `run_isolated_sweep`'s per-sweep guard never covers.
fn reject_extra_args_complete(
    certifies: Option<Certifies>,
    extra_args: &[String],
) -> Result<(), DevError> {
    if certifies == Some(Certifies::Complete) && !extra_args.is_empty() {
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

/// The test phase's "failures already listed" sentinel. A `Build` rather than
/// a `Reported` until the build phases finish, because the phase loop tells a
/// failing lane (fail-fast for the lanes after it) from every other way out
/// by it; [`already_reported`] converts it after.
const TESTS_FAILED: &str = "tests failed";

/// Mark a phase's printed-detail sentinel as [`DevError::Reported`], leaving
/// every other error (whose message is its only diagnostic) untouched.
fn already_reported(e: DevError, sentinel: &str) -> DevError {
    match e {
        DevError::Build(m) if m == sentinel => DevError::Reported(m),
        other => other,
    }
}

/// Assemble the `cargo clippy` argv for one sweep. Always
/// `--message-format=json` so the lint code is populated on every diagnostic
/// (cargo's pretty stderr only annotates the first occurrence per crate),
/// `--keep-going` so a failing unit doesn't hide lints queued behind it, and
/// `--cap-lints=warn` so a deny-level lint still yields `.rmeta` and every
/// downstream crate is checked - brokkr recovers the intent by treating any
/// surfaced diagnostic as a hard failure at the call site. `allow` is the
/// `[clippy] allow` list, emitted as `-A <lint>` so the suppressed lints
/// never reach the diagnostic stream (no carve-outs in the fail decision).
fn clippy_args(sweep: &ResolvedSweep, run: &ResolutionRun, allow: &[String]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "clippy".into(),
        "--keep-going".into(),
        // `--all-targets` on every gate sweep; `--lib` only under the
        // investigative `brokkr clippy --lib`, which exists to reproduce the
        // lint surface of a plain lib build. See `ResolvedSweep::lib_only`.
        if sweep.lib_only {
            "--lib".into()
        } else {
            "--all-targets".into()
        },
        "--message-format=json".into(),
    ];
    // A sweep pinned to a profile is linted in it: `cfg(debug_assertions)`
    // decides which code exists, so linting a release sweep in dev checks
    // source the release build never compiles - and misses the source it
    // does.
    args.extend(sweep_profile_args(sweep));
    // Same reason as the profile above, one level out: package and workspace
    // resolution enable different features, so they compile different source
    // and present different lint surfaces. Linting a package-mode sweep under
    // ambient resolution checks code its own build never compiles - and misses
    // the `#[cfg(feature)]` arms that only its resolution turns on, which is
    // exactly the defect class the mode exists to catch.
    args.extend(sweep.unification_args());
    // `-p <pkg>` scoping is also what makes `--features` valid in a virtual
    // workspace, where cargo rejects features at the root. The features are
    // the run's projection: only the tokens its packages route.
    args.extend(run.cargo_args());
    args.push("--".into());
    args.push("--cap-lints=warn".into());
    // Cargo's own lints are allowed at ingestion instead (`code_allowed`):
    // rustc knows no `cargo` lint tool.
    for lint in allow.iter().filter(|l| !crate::config::is_cargo_lint(l)) {
        args.push("-A".into());
        args.push(lint.clone());
    }
    args
}

/// One `cargo clippy` run: one sweep, one cargo resolution.
///
/// Split out because package mode makes this a loop body rather than a
/// straight line - a package-mode sweep lints once per package, for the same
/// reason it tests once per package. A batched multi-`-p` clippy resolves a
/// graph that is none of the graphs the lane builds, so its lint surface would
/// belong to no real compile.
fn run_one_clippy(
    project_root: &Path,
    sweep: &ResolvedSweep,
    run: &ResolutionRun,
    allow: &[String],
    meta_target_dir: Option<&Path>,
    commands: bool,
) -> Result<SweepResult, DevError> {
    let args = clippy_args(sweep, run, allow);
    let result = run_one_diagnostic_cargo("clippy", project_root, sweep, &args, run, meta_target_dir, commands);
    // No SweepResult reaches the normal reporter on a spawn error, interrupt
    // or captured-run deadline. Preserve the investigative command there too.
    if result.is_err() && !commands && !report_active() {
        output::error(&format!(
            "{}: {}",
            phase_sweep_tag("clippy", &sweep.label),
            describe_sweep(sweep, false, &run.selection, run.feature_args())
        ));
        output::error(&format!("failing command: cargo {}", args.join(" ")));
    }
    result
}

/// `cargo doc` argv for one sweep resolution. The selection half is clippy's -
/// profile, unification, packages, features - because rustdoc resolves `cfg`
/// exactly as the build does, so documenting a sweep under any other shape
/// judges doc comments on code that shape never compiles.
///
/// No `--cap-lints`, unlike clippy: that flag exists there to finish the graph
/// past a denied lint, and `--keep-going` already does that for doc. No `-A`
/// either - rustdoc takes lint flags only through rustdocflags, where every
/// change re-fingerprints the doc units - so `[lints] allow` is applied at
/// ingestion instead ([`code_allowed`]). The lint-only `--check` flags do go
/// through rustdocflags, added in [`run_one_diagnostic_cargo`]
/// (`crate::rustdoc_check`).
fn doc_args(sweep: &ResolvedSweep, run: &ResolutionRun, cfg: &RustdocConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "doc".into(),
        "--no-deps".into(),
        "--keep-going".into(),
        "--message-format=json".into(),
    ];
    if cfg.document_private_items {
        args.push("--document-private-items".into());
    }
    args.extend(sweep_profile_args(sweep));
    args.extend(sweep.unification_args());
    args.extend(run.cargo_args());
    args
}

/// One cargo run whose stdout is a `--message-format=json` diagnostic stream:
/// the sweep's env, and its isolated target dir when it has one, so the run
/// reuses the artifacts its test phase builds rather than rebuilding beside
/// them. `phase` names the run in the log line.
fn run_one_diagnostic_cargo(
    phase: &str,
    project_root: &Path,
    sweep: &ResolvedSweep,
    args: &[String],
    run: &ResolutionRun,
    meta_target_dir: Option<&Path>,
    commands: bool,
) -> Result<SweepResult, DevError> {
    let selection = &run.selection;
    // Apply the sweep's env to the clippy build too, so a build-affecting
    // var (codegen toggle, etc.) is set consistently across every phase -
    // clippy, the test pre-build, and the test run - not just the tests.
    // A sweep with `rustflags` (or package-mode unification) clippy-checks
    // under the same cfg + isolated target dir as its tests, so the gate's
    // lints match its build. Plain sweeps contribute nothing here, and for
    // those `meta_target_dir` is `None` (computed by the caller).
    //
    // No lint allows in the env here: clippy passes the same `-A` set on its
    // own argv (`clippy_args`), and a second copy in RUSTFLAGS would only
    // change the build fingerprint away from the test phase's, costing a
    // rebuild for no change in what is suppressed.
    let brokkr_env = meta_target_dir.map_or_else(Vec::new, |dir| sweep_cargo_env(sweep, dir, &[]));
    // Composed through `merged_env`, the test phase's rule, so every phase
    // agrees on who wins a collision: the sweep's env. Config loading refuses
    // the colliding keys (`RUSTFLAGS`, `CARGO_TARGET_DIR`, ...) in `[[check]]`
    // and profile env, so a collision only arises from `brokkr clippy --env`,
    // which is documented to win over every other source - appending brokkr's
    // pair after it used to silently undo the override.
    let mut env_owned = merged_env(&sweep.env, &brokkr_env);
    let mut args = args.to_vec();
    // rustdoc's lint-only mode when the toolchain has it: the phase reads the
    // diagnostics and never the rendered site (`rustdoc_check`). An env
    // placement is shown on the command line, since it changes what ran.
    let mut env_prefix = String::new();
    let doc_check = phase == "rustdoc" && crate::rustdoc_check::supported(project_root, &env_owned);
    if doc_check {
        match crate::rustdoc_check::place(&env_owned) {
            crate::rustdoc_check::Placement::Env { key, value } => {
                env_prefix = format!("{key}={value:?} ");
                env_owned.retain(|(k, _)| k != key);
                env_owned.push((key.to_owned(), value));
            }
            crate::rustdoc_check::Placement::Config(extra) => args.extend(extra),
        }
    }
    let shape = describe_sweep(sweep, false, selection, run.feature_args());
    let command = format!("{env_prefix}cargo {}", args.join(" "));
    let line_shape = format!("{}: {shape}", phase_sweep_tag(phase, &sweep.label));
    if report_active() {
        announce_sweep(&line_shape, Some(&command), commands);
    } else {
        // Investigative clippy echoes the command after the run and the shape
        // on failure. It has no log; avoid silently dropping log-only writes
        // or printing that same command twice.
        output::status(&line_shape);
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_refs: Vec<(&str, &str)> = env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let started = std::time::SystemTime::now();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;
    let stdout = String::from_utf8_lossy(&captured.stdout).into_owned();
    if doc_check {
        // The sweep's isolated target dir when it has one (its env points
        // cargo there), else the one cargo metadata reports. A lookup failure
        // only costs the stamp: the next run re-documents.
        let target_dir = match meta_target_dir {
            Some(dir) => Some(dir.to_path_buf()),
            None => build::project_info(Some(project_root)).ok().map(|i| i.target_dir),
        };
        if let Some(dir) = target_dir {
            crate::rustdoc_check::stamp_outputs(&stdout, &dir.join("doc"), started);
        }
    }
    Ok(SweepResult {
        label: sweep.label.clone(),
        shape,
        command,
        stdout,
        stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
        success: captured.status.success(),
        selected: selection.packages().map(<[String]>::to_vec),
        manifest: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_clippy_phase(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    selection: &PhaseSelection,
    allow: &[String],
    allow_exact: &[SitedAllow],
    commands: bool,
    reach: &mut dyn FnMut(usize),
    // The caller's word that this run lints less than the gate's whole
    // surface, beyond what a CLI `-p` already says (`brokkr clippy`'s probe
    // shapes). Only silences the stale-allow report.
    narrowed: bool,
    // `brokkr clippy`: the cargo command is printed once, after the run -
    // on a green run as the command line, on a failure as the failing-command
    // pair `report_diagnostic_phase` prints - never streamed beforehand too.
    // The command carries the blanket `-A` flags, so their own line is
    // log-only.
    echo_after: bool,
) -> Result<(), DevError> {
    // Inside `check` the verdict line names every suppression once, for all
    // the phases it narrows; `brokkr clippy` (and `--commands`, which asks
    // for the long form) prints them here.
    announce_allows(allow, allow_exact, commands || !report_active(), !echo_after);

    // A phase that attempts nothing says so before asking cargo anything: a
    // metadata failure must not replace a refusal already decided.
    selection.check_refusal()?;
    let info = build::project_info(Some(project_root))?;
    let mut ran = run_per_build_shape("clippy", &info, sweeps, selection, reach, |sweep, run, dir| {
        run_one_clippy(project_root, sweep, run, allow, dir, commands)
    })?;
    let active_sweep_count = ran.active_sweep_count;
    let mut results = std::mem::take(&mut ran.results);
    for r in &mut results {
        r.manifest = attribute_manifest_lints(r, &info);
    }

    report_stale_sited_allows(
        "clippy",
        &results,
        allow_exact,
        narrowed || !selection.overrides().is_empty(),
    );

    // With `--cap-lints=warn`, a lint no longer makes cargo exit non-zero, so
    // the pass/fail decision is brokkr's own: every diagnostic is a failure,
    // whatever its (capped) level, except a dependency's warning. A failed run
    // with nothing parseable still fails.
    let members = Some(&info.workspace_members);
    // Cargo's manifest lints never saw clippy's `-A` flags, so `[lints] allow`
    // reaches them here.
    let keep = |d: &cargo_json::DiagnosticEvent| {
        !is_dependency_warning(d, members)
            && !sited_allowed(d, allow_exact)
            && !(d.code.as_deref().is_some_and(crate::config::is_cargo_lint) && code_allowed(d, allow))
    };
    let outcome =
        ran.verdict(|failed| {
            report_diagnostic_phase(
                "clippy",
                &results,
                &info,
                project_root,
                &keep,
                active_sweep_count,
                failed,
            )
        });
    if echo_after && outcome.is_ok() {
        for r in &results {
            output::run_msg(&r.command);
        }
    }
    outcome
}

/// Cargo's manifest lints from a clippy run's stderr
/// ([`cargo_json::manifest_lints_by_block`]), each attributed to the package
/// whose `Cargo.toml` it names, and narrowed to what the run selected.
///
/// Cargo prints the path relative to the workspace root whatever directory it
/// ran in (observed: a run from a member's parent directory printed
/// `crates/a/Cargo.toml`, not `../crates/a/Cargo.toml`), so it resolves
/// against `workspace_root`. Three owners:
///
/// - a member's manifest: that member's package id, so the lint follows the
///   run's selection like any other diagnostic - a `-p a` run reports `a`'s
///   manifest and nobody else's.
/// - the workspace root's `Cargo.toml`: every run reports it, whatever it
///   selected - it holds the workspace's own tables, the ones
///   `unused_workspace_dependencies` is about. No id in a virtual workspace;
///   the root package's id in a non-virtual one.
/// - anything else: cargo runs its manifest-parsing lints over every
///   path-sourced package it loads, members or not - a sibling checkout, a
///   `[patch]`ed or `exclude`d crate. Its id is the non-member marker
///   `manifest+<path>`, which no member carries, so [`is_dependency_warning`]
///   treats its warnings as a dependency's.
fn attribute_manifest_lints(
    r: &SweepResult,
    info: &build::ProjectInfo,
) -> Vec<(usize, cargo_json::DiagnosticEvent)> {
    let blocks = rustc_text::rustc_blocks(&r.stderr);
    let root_manifest = info.workspace_root.join("Cargo.toml");
    cargo_json::manifest_lints_by_block(&blocks)
        .into_iter()
        .filter_map(|(i, mut e)| {
            let path = lexical_normalize(&info.workspace_root.join(e.file.as_deref()?));
            match info.member_manifests.get(&path) {
                // A non-virtual root is a member too, but its manifest still
                // holds the workspace's tables: reported by every run.
                Some(id) if path == root_manifest => e.package_id = Some(id.clone()),
                Some(id) if r.selects(id, info) => e.package_id = Some(id.clone()),
                Some(_) => return None,
                None if path == root_manifest => {}
                None => e.package_id = Some(format!("manifest+{}", path.display())),
            }
            Some((i, e))
        })
        .collect()
}

/// `path` with `.` and `..` components folded away, without touching the
/// filesystem - a manifest path climbing out of the workspace must compare
/// equal to the absolute one `cargo metadata` reports.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The `rustdoc` phase: `cargo doc --no-deps` per build shape, failing on any
/// diagnostic, rendered through clippy's formatter. Inert without `[rustdoc]`.
///
/// Every surviving diagnostic fails, warnings included, the same rule clippy's
/// gate applies: a rustdoc warning nobody is made to read is a doc link rotting
/// in silence, which is the defect class the phase exists for. `[lints] allow`
/// removes a lint by code, `allow_exact` by site.
#[allow(clippy::too_many_arguments)]
fn run_rustdoc_phase(
    project_root: &Path,
    cfg: Option<&RustdocConfig>,
    sweeps: &[ResolvedSweep],
    selection: &PhaseSelection,
    allow: &[String],
    allow_exact: &[SitedAllow],
    commands: bool,
    reach: &mut dyn FnMut(usize),
) -> Result<(), DevError> {
    let Some(cfg) = cfg else {
        return Ok(());
    };
    // As in clippy: the decided refusal before any cargo query.
    selection.check_refusal()?;
    let info = build::project_info(Some(project_root))?;
    // A doc-only sweep is a doctest carrier: it names no compile shape of its
    // own, so documenting it re-reports another sweep's diagnostics under a
    // second label - and may not dedupe, when it inherits from config what the
    // sibling passes on argv. The selection marks it not applicable.
    let mut ran = run_per_build_shape("rustdoc", &info, sweeps, selection, reach, |sweep, run, dir| {
        let args = doc_args(sweep, run, cfg);
        run_one_diagnostic_cargo("rustdoc", project_root, sweep, &args, run, dir, commands)
    })?;
    let active_sweep_count = ran.active_sweep_count;
    let results = std::mem::take(&mut ran.results);
    // The `rustdoc::` half of the sited list is judged here, where those lints
    // can actually appear; clippy judges the rest.
    report_stale_sited_allows("rustdoc", &results, allow_exact, !selection.overrides().is_empty());
    let members = Some(&info.workspace_members);
    let keep = |d: &cargo_json::DiagnosticEvent| {
        !is_dependency_warning(d, members)
            && !code_allowed(d, allow)
            && !sited_allowed(d, allow_exact)
    };
    ran.verdict(|failed| {
        report_diagnostic_phase(
            "rustdoc",
            &results,
            &info,
            project_root,
            &keep,
            active_sweep_count,
            failed,
        )
    })
}

/// Whether `[lints] allow` names this diagnostic's lint. Matched on the parsed
/// identity cargo reports (`rustdoc::broken_intra_doc_links`,
/// `cargo::unused_dependencies`); a lint group name matches nothing here,
/// since diagnostics carry the member lint's code. The ingestion-side allow
/// for the lints no `-A` flag can reach: rustdoc's (no `-A` without changing
/// the doc fingerprint) and cargo's own (no `-A` at all).
fn code_allowed(d: &cargo_json::DiagnosticEvent, allow: &[String]) -> bool {
    d.code.as_deref().is_some_and(|code| {
        let code = crate::config::LintName::parse(code);
        allow.iter().any(|allow| crate::config::LintName::parse(allow).matches(code))
    })
}

/// Whether an `allow_exact` entry suppresses this diagnostic.
fn sited_allowed(d: &cargo_json::DiagnosticEvent, allow_exact: &[SitedAllow]) -> bool {
    allow_exact.iter().any(|s| sited_match(s, d))
}

/// `phase sweep-label`, or just `phase` when the sweep is labelled after it:
/// `brokkr clippy`'s ad-hoc sweep is labelled "clippy", and `clippy clippy:`
/// says the one fact twice.
fn phase_sweep_tag(phase: &str, label: &str) -> String {
    if label == phase {
        phase.to_owned()
    } else {
        format!("{phase} {label}")
    }
}

/// Decide and report a diagnostic phase from its cargo runs: any failed run
/// or any diagnostic `keep` admits fails it. Prints the failing commands, then
/// the scoped, cross-sweep summary. `already_failed`: a run the phase could
/// not make failed it before these results were read, so a clean set prints
/// no green line.
#[allow(clippy::too_many_arguments)]
fn report_diagnostic_phase(
    phase: &str,
    results: &[SweepResult],
    info: &build::ProjectInfo,
    project_root: &Path,
    keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool,
    active_sweep_count: usize,
    already_failed: bool,
) -> Result<(), DevError> {
    let run_failed = |r: &SweepResult| {
        !r.success || r.diagnostics().iter().any(keep)
    };
    if !results.iter().any(run_failed) {
        // The phase's one green line. Only inside `check`: `brokkr clippy`
        // closes with its own `clippy clean` verdict.
        if report_active() && !already_failed {
            output::run_msg(&format!(
                "{phase}: ok ({}) in {}",
                output::count(active_sweep_count, "sweep"),
                fmt_wall(phase_elapsed())
            ));
        }
        return Ok(());
    }

    // A green run printed neither the shape nor the command; a failing sweep
    // is exactly where both earn their place - under `--commands` too, whose
    // streamed line is neither beside the failure nor attributable among runs.
    for r in results.iter().filter(|r| run_failed(r)) {
        output::error(&format!("{}: {}", phase_sweep_tag(phase, &r.label), r.shape));
        output::error(&format!("failing command: {}", r.command));
    }

    output::error(&format_clippy_multi(
        &format!("cargo {}", if phase == "rustdoc" { "doc" } else { phase }),
        results,
        Some(info),
        project_root,
        active_sweep_count,
        keep,
    ));
    Err(DevError::Reported(format!("{phase} failed")))
}

/// Run `run_one` once per cargo resolution of every lane the phase's selection
/// attempts - one per distinct build shape, one per package under
/// package-mode unification - calling `reach(i)` for each sweep whose run
/// returns an outcome. The dedupe, the CLI `-p` intersection and the
/// nothing-reached refusal were all decided by the selection, which every
/// per-shape diagnostic phase reads, so they cannot drift apart; the stored
/// refusal is reported here, at the phase's start.
#[allow(clippy::too_many_arguments)]
fn run_per_build_shape(
    phase: &str,
    info: &build::ProjectInfo,
    sweeps: &[ResolvedSweep],
    selection: &PhaseSelection,
    reach: &mut dyn FnMut(usize),
    run_one: impl Fn(&ResolvedSweep, &ResolutionRun, Option<&Path>) -> Result<SweepResult, DevError> + Sync,
) -> Result<PhaseRuns, DevError> {
    selection.check_refusal()?;
    // cargo's resolved target dir, so a `rustflags` sweep clippy-checks in the
    // *same* isolated `<target>/rustflags-<hash>` its test phase builds into -
    // they must agree on the location, and a workspace can place it off the
    // project root (S3-20). Passed only when some sweep needs it: "any sweep
    // needs an isolated dir", not "any sweep has rustflags" - a package-mode
    // sweep isolates on its own, and the old predicate would have left it
    // clippy-checking in the shared dir while its tests built in the isolated
    // one, the two disagreeing about where the artifacts are.
    let meta_target_dir = sweeps
        .iter()
        .any(|sweep| sweep.target_location().is_isolated())
        .then(|| info.target_dir.clone());

    // Every cargo run the phase makes, in configured order: (sweep, run).
    let mut jobs: Vec<(usize, &ResolutionRun)> = Vec::new();
    // Log only inside `check`, whose run log keeps them and which announced
    // the package rules once, up front (`announce_package_rules`) - repeating
    // them per phase was noise. `brokkr clippy` keeps no run log and announces
    // nothing, so there they print. Every disposition is config plus
    // invocation, the same every run, so none of it is stdout news in `check`.
    let note = |line: &str| if report_active() { output::detail(line) } else { output::run_msg(line) };
    for (i, sweep) in sweeps.iter().enumerate() {
        let Some(entry) = selection.entry(i) else { continue };
        // A lane keeping some packages drops the rest with a note; a lane
        // keeping none says so once, as its skip reason.
        if matches!(entry, LaneEntry::Attempt(_) | LaneEntry::Deduped(_)) {
            for dropped_note in entry.notes() {
                note(&format!("{phase} {}: {dropped_note} (dropped)", sweep.label));
            }
        }
        let attempt = match entry {
            LaneEntry::Attempt(a) => a,
            // Diagnostic phases are per-build-shape while tests are per-lane:
            // two lanes compiling the same requests (a shared `[[check]]`
            // entry, or two sweeps a `-p` narrows to the same projection) are
            // linted or documented once.
            LaneEntry::Deduped(_) => {
                note(&format!("{phase} {}: deduped (build shape already checked)", sweep.label));
                continue;
            }
            other => {
                if let Some(reason) = other.skip_reason() {
                    note(&format!("{phase} {}: skipped ({reason})", sweep.label));
                }
                continue;
            }
        };
        // Package mode lints one package per cargo run, for the same reason it
        // tests one per run: a batched multi-`-p` clippy resolves a graph that
        // is not any of the graphs the lane actually builds, so its lint
        // surface belongs to no real compile.
        for run in attempt.runs() {
            jobs.push((i, run));
        }
    }

    // One chain per target directory, the chains concurrent
    // (`run_grouped`): runs sharing a directory would only queue on cargo's
    // build-dir lock.
    let keyed: Vec<(usize, PathBuf)> = jobs
        .iter()
        .enumerate()
        .map(|(j, (i, _))| (j, target_dir_key(&sweeps[*i], &info.target_dir)))
        .collect();
    let outcomes = run_grouped(&keyed, &|j| {
        let (i, run) = jobs[j];
        run_one(&sweeps[i], run, meta_target_dir.as_deref())
    });
    let mut reached = vec![false; sweeps.len()];
    let mut ran = PhaseRuns {
        results: Vec::with_capacity(jobs.len()),
        error: None,
        active_sweep_count: 0,
    };
    // Record every completed chain before an interrupt can return early from
    // the result pass below. Other target-dir chains may already have run.
    for (j, (_, outcome)) in outcomes.iter().enumerate() {
        if outcome.is_some() {
            let sweep_index = jobs[j].0;
            reach(sweep_index);
            reached[sweep_index] = true;
        }
    }
    ran.active_sweep_count = reached.into_iter().filter(|was_reached| *was_reached).count();
    for (_, outcome) in outcomes {
        let Some(outcome) = outcome else { continue };
        match outcome {
            Ok(r) => ran.results.push(r),
            // A stop ends the phase at once: nothing more is reported.
            Err(DevError::Interrupted) => return Err(DevError::Interrupted),
            // A run that could not be made is the phase's failure, but not
            // a reason to lose what the other runs found: the first is
            // returned beside the results, any further one printed here.
            Err(e) if ran.error.is_some() => {
                if !e.already_printed() {
                    output::error(&e.to_string());
                }
            }
            Err(e) => ran.error = Some(e),
        }
    }
    Ok(ran)
}

/// What a per-build-shape phase's cargo runs produced: every run's result,
/// and the first run that could not be made (others already printed).
struct PhaseRuns {
    results: Vec<SweepResult>,
    error: Option<DevError>,
    /// Distinct sweeps that got at least one cargo run, independent of how
    /// many package-mode resolutions each sweep expanded into.
    active_sweep_count: usize,
}

impl PhaseRuns {
    /// The phase's verdict: a run that could not be made fails it whatever
    /// the others found. `report` reports the runs that were made, and is
    /// told whether the phase already failed - a failed phase has no green
    /// line to print, even when every run it did make was clean.
    fn verdict(self, report: impl FnOnce(bool) -> Result<(), DevError>) -> Result<(), DevError> {
        let reported = report(self.error.is_some());
        match self.error {
            Some(e) => Err(e),
            None => reported,
        }
    }
}

/// Record the test phase's lint suppressions, and where they were injected, in
/// the run log. The verdict line is what names the suppressions on the console;
/// only an injection point that may be inert (below) earns a printed warning.
///
/// The clippy phase can say "allowing X" and leave it there - it passes `-A` on
/// its own argv. The test phase cannot: it has no rustc passthrough, so the
/// flags go into cargo's rustflags at whichever layer [`rustflags::sink`] found
/// live, and injecting at the `build.rustflags` layer while some
/// `target.<cfg>` table actually wins is inert. That case is not an error - it
/// is the deliberately safe direction, since the alternative silently discards
/// the project's own flags - but it must be visible, or a suppression that
/// quietly does nothing reads as a brokkr bug.
fn announce_test_allows(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    allow_flags: &[String],
    allow_exact: &[SitedAllow],
) {
    if allow_flags.is_empty() {
        return;
    }
    let lints: Vec<&str> = allow_flags
        .iter()
        .filter(|f| *f != "-A")
        .map(String::as_str)
        .collect();
    // The sink can differ per sweep (a `rustflags` sweep exports an env var and
    // so always lands in the env layer); report the plain-sweep shape explicitly.
    let sink = rustflags::sink(project_root, false);
    // One clause, not a second line: that a sited entry widens here is a
    // property of the phase, not news about any particular entry, so it does
    // not earn a line of its own - let alone one per entry.
    // Only rustc's own lints reach the build (`test_phase_allow_flags`).
    let widened = if !allow_exact.iter().any(|s| crate::config::reaches_test_build(&s.lint)) {
        ""
    } else {
        "; allow_exact applies build-wide here"
    };
    // Logged, not printed: the verdict line names the suppressions, and a
    // sink brokkr is sure of does what it says. The one case a reader must
    // see is the sink chosen on a guess - then the allows may do nothing,
    // and a lint the gate claims to suppress fails the test build instead.
    output::detail(&format!(
        "test: allowing {} via {} ([lints]{widened})",
        lints.join(", "),
        sink.describe()
    ));
    let plain: Vec<&str> = sweeps
        .iter()
        .filter(|s| s.rustflags.is_empty())
        .map(|s| s.label.as_str())
        .collect();
    if !plain.is_empty() && rustflags::sink_may_be_inert(project_root) {
        output::warn(&format!(
            "[lints] allows ride `{}` for the test, coverage and install builds of {}, but a \
             `target.*.rustflags` table in the cargo config has a selector brokkr cannot \
             evaluate. If it matches this host, cargo reads that table instead and the allows \
             do nothing there.",
            rustflags::Sink::BuildConfig.describe(),
            plain.join(", ")
        ));
    }
}

/// Announce the clippy phase's suppressions in one line.
///
/// A narrowed gate must not read as a full one, so the allowed lints are named
/// on every run - but *naming the lints* is the whole requirement, and this
/// used to print one line per `allow_exact` entry. At three entries that reads
/// as a list; at seventy it is a wall that buries the rest of the run, and the
/// paths in it are already in `brokkr.toml` where they can be read at leisure.
/// So sited entries collapse to their distinct lints with a count each.
///
/// The per-entry detail that is *not* in the config - which entries matched
/// nothing - is reported separately by [`report_stale_sited_allows`], and that
/// one still prints per entry. It is the inverse: rare, actionable, and the
/// only part of this a reader has to act on.
///
/// `blanket_on_stdout` is false when the printed cargo command already shows
/// the blanket `[lints] allow` set as `-A` flags (`brokkr clippy`): the line
/// then goes to the run log only, while `allow_exact` - which never reaches the
/// command line - keeps its stdout line.
fn announce_allows(allow: &[String], allow_exact: &[SitedAllow], to_stdout: bool, blanket_on_stdout: bool) {
    let say = |msg: &str| {
        if to_stdout {
            output::run_msg(msg);
        } else {
            output::detail(msg);
        }
    };
    if !allow.is_empty() {
        let msg = format!("clippy: allowing {} ([lints] allow)", allow.join(", "));
        if blanket_on_stdout {
            say(&msg);
        } else if report_active() {
            // Clippy's console command already carries every blanket -A.
            output::detail(&msg);
        }
    }
    if !allow_exact.is_empty() {
        say(&format!(
            "clippy: allowing {} ([lints] allow_exact)",
            summarize_sited(allow_exact)
        ));
    }
}

/// `lint (n files)` per distinct lint, in first-seen order - or `lint at path`
/// when a lint has just the one site, since the path is short and is the more
/// useful thing to see.
fn summarize_sited(allow_exact: &[SitedAllow]) -> String {
    let mut order: Vec<&str> = Vec::new();
    let mut sites: HashMap<&str, Vec<&str>> = HashMap::new();
    for s in allow_exact {
        let entry = sites.entry(s.lint.as_str()).or_default();
        if entry.is_empty() {
            order.push(s.lint.as_str());
        }
        entry.push(s.path.as_str());
    }
    order
        .iter()
        .map(|lint| match sites.get(lint).map(Vec::as_slice) {
            Some([one]) => format!("{lint} at {one}"),
            Some(many) => format!("{lint} ({} files)", many.len()),
            _ => (*lint).to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A sited suppression that matched nothing across every sweep is dead
/// weight: either upstream fixed the site (delete the entry) or the file
/// moved (re-site it). Say so - a notice, not a failure - so the list never
/// accretes silently. Only an unscoped run can testify: a `-p`-narrowed run
/// simply doesn't check the entry's file when it lives in another package,
/// so "suppressed nothing" there is noise, not evidence of staleness. The
/// caller folds every such narrowing into `narrowed` (a CLI `-p`, and for
/// `brokkr clippy` a probe shape smaller than the gate's).
///
/// Each phase testifies only for the entries it can produce: `rustdoc::` lints
/// never appear in clippy's stream, and every other lint is clippy's to judge
/// (rustdoc re-reports a subset of rustc lints, but clippy sees those too). So
/// clippy passes judgement on the non-`rustdoc::` entries and the rustdoc
/// phase on the `rustdoc::` ones - judged by clippy alone, a `rustdoc::`
/// entry warned "suppressed nothing" on every run. A project with no
/// `[rustdoc]` runs no rustdoc phase and so never judges those entries.
fn report_stale_sited_allows(
    phase: &str,
    results: &[SweepResult],
    allow_exact: &[SitedAllow],
    narrowed: bool,
) {
    if narrowed {
        return;
    }
    let judged: Vec<&SitedAllow> = allow_exact
        .iter()
        .filter(|s| is_rustdoc_lint(&s.lint) == (phase == "rustdoc"))
        .collect();
    if judged.is_empty() {
        return;
    }
    let mut matched = vec![false; judged.len()];
    for r in results {
        for d in r.diagnostics() {
            for (i, s) in judged.iter().enumerate() {
                if sited_match(s, &d) {
                    matched[i] = true;
                }
            }
        }
    }
    for (s, hit) in judged.iter().zip(&matched) {
        if !hit {
            output::warn(&format!(
                "{phase}: allow_exact {s} suppressed nothing (stale entry?)"
            ));
        }
    }
}

/// Whether a lint name belongs to rustdoc's tool namespace.
fn is_rustdoc_lint(lint: &str) -> bool {
    crate::config::LintName::parse(lint).judged_by_rustdoc()
}

/// Does a `[clippy] allow_exact` entry suppress this diagnostic? Lint names
/// match with or without the `clippy::` qualifier (mirroring what `allow`
/// accepts); the file must equal the entry's path exactly - cargo emits
/// build-root-relative paths, which is also what the entry is written from.
fn sited_match(s: &SitedAllow, d: &cargo_json::DiagnosticEvent) -> bool {
    let Some(code) = &d.code else {
        return false;
    };
    let lint_matches = crate::config::LintName::parse(&s.lint)
        .matches(crate::config::LintName::parse(code));
    lint_matches && d.file.as_deref() == Some(s.path.as_str())
}

/// Parse a sweep's stdout and drop every diagnostic a `[clippy] allow_exact`
/// entry suppresses. This is the ingestion-side gate the sited allows act
/// through: unlike `allow`'s `-A`, which a source site's own lint-level
/// attribute can defeat (the `#[expect]`-sibling interaction in
/// `docs/commands/check.md`), a diagnostic filtered here is gone from the
/// pass/fail decision no matter how clippy leveled it. The phases apply the
/// same gate through [`sited_allowed`] inside their `keep` predicate; this
/// whole-stream form remains for the parser tests.
#[cfg(test)]
fn gated_diags(
    stdout: &str,
    allow_exact: &[SitedAllow],
) -> Vec<cargo_json::DiagnosticEvent> {
    let mut events = cargo_json::parse_cargo_diagnostics(stdout);
    if !allow_exact.is_empty() {
        events.retain(|d| !allow_exact.iter().any(|s| sited_match(s, d)));
    }
    events
}

/// Investigative single-phase clippy runner (`brokkr clippy`). Builds one
/// `ResolvedSweep` from the CLI shape, runs the shared clippy pipeline, and
/// reports the same summary line `brokkr check` does.
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
    clippy_allow: &[String],
    clippy_allow_exact: &[SitedAllow],
) -> Result<(), DevError> {
    let started = std::time::Instant::now();
    // `check`'s clippy ceiling applies here too: the ceilings exist because of
    // an observed 1h15m clippy hang, and this runner drives the same pipeline.
    // The caller already holds the lock, so a wait behind another command is
    // not charged. Declared before the guard, so the guard drops first.
    let _ceiling = CheckWatchdog::arm_for("brokkr clippy", None);
    enter_phase("clippy");
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let mut sweep = build_clippy_sweep(
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
    sweep.lib_only = lib_only;
    // The same resolution point `check` uses, so `--sweep NAME` replays the
    // entry's graph and not just its flags: a `feature_unification =
    // "package"` entry lints once per package under the package pin, and a
    // parallel `auto` entry that `check` promotes to a workspace pin is
    // promoted here too. Without this the sweep kept its unresolved `Ambient`
    // default and linted one ambient invocation - a lint surface no gate
    // build compiles. An ad-hoc sweep is never parallel, so it resolves to
    // exactly what it was. The CLI scope is empty on purpose: ad-hoc `-p`
    // lives in `sweep.packages`, where it already disqualifies promotion.
    resolve_sweep_unification(std::slice::from_mut(&mut sweep), project_root, &[], &[])?;

    // One sweep -> run_clippy_phase runs `multi = false`, so output carries no
    // sweep-label tags. The selection has no CLI override: ad-hoc `-p` is
    // already in sweep.packages, so the entry is the sweep's own Explicit (or
    // Bare) selection. `commands = false, echo_after = true`: this is the
    // *investigative* runner, invoked to find out what a given target shape
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
        clippy_allow,
        clippy_allow_exact,
        false,
        &mut |_| {},
        probe_is_narrowed(&sweep),
        true,
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
        Err(DevError::Reported(m)) if m == CLIPPY_FAILED => {
            output::error(&format!("clippy failed in {}", fmt_wall(started.elapsed())));
            Err(DevError::ExitCode(1))
        }
        // Anything else (no Cargo.toml, cargo metadata failure, cargo missing,
        // spawn failure, cooperative interrupt) is NOT an already-rendered lint
        // result - propagate the real cause so main reports it, instead of
        // masking it behind "clippy failed".
        Err(other) => Err(other),
    }
}

/// The label `report_diagnostic_phase` returns (as `DevError::Reported`) for a
/// failed clippy phase - the one error from it whose detail is already on
/// screen.
const CLIPPY_FAILED: &str = "clippy failed";

/// Whether a `brokkr clippy` probe covers less than the gate's lint surface,
/// in which case "this sited allow suppressed nothing" is not evidence the
/// entry is stale - the site may simply be outside what was linted. A package
/// scope (ad-hoc `-p` or a `--sweep` entry's `packages`) leaves other crates
/// unlinted; `--lib` leaves every test-only site unlinted; and a feature set
/// short of `--all-features` leaves feature-gated sites unlinted.
fn probe_is_narrowed(sweep: &ResolvedSweep) -> bool {
    !sweep.packages.is_empty()
        || sweep.lib_only
        || !sweep.features.all_features
}

struct SweepResult {
    label: String,
    /// The sweep's human shape ([`describe_sweep`]). A green run never prints
    /// it; a failing run reports each failing sweep with its own, because
    /// the diagnostic phases report after every sweep has run and no ambient
    /// "current sweep" can say which one a failure belongs to.
    shape: String,
    /// The full `cargo clippy ...` line, kept so a failing sweep can reprint it
    /// even when the collapsed (default) log form suppressed it on the way in.
    command: String,
    stdout: String,
    stderr: String,
    success: bool,
    /// The packages this run selected by name; `None` for a bare selection,
    /// which builds the workspace's default members. What the run covered,
    /// for deciding whether a diagnostic it did not report is news.
    selected: Option<Vec<String>>,
    /// Cargo's manifest lints from this run's stderr, attributed to their
    /// packages, each with the index of the stderr block it came from
    /// ([`attribute_manifest_lints`]). Filled by the clippy phase only: clippy
    /// and rustdoc runs both emit them, and reading them twice would report
    /// each one twice. Empty everywhere else.
    manifest: Vec<(usize, cargo_json::DiagnosticEvent)>,
}

impl SweepResult {
    /// Every diagnostic the run produced: the JSON stream on stdout, plus
    /// cargo's manifest lints, which only ever reach stderr as text.
    fn diagnostics(&self) -> Vec<cargo_json::DiagnosticEvent> {
        let mut events = cargo_json::parse_cargo_diagnostics(&self.stdout);
        events.extend(self.manifest.iter().map(|(_, e)| e.clone()));
        events
    }

    /// Whether this run selected the package `id`.
    fn selects(&self, id: &str, info: &build::ProjectInfo) -> bool {
        match &self.selected {
            None => info.default_members.contains(id),
            Some(names) => info.workspace_members.get(id).is_some_and(|n| names.contains(n)),
        }
    }
}

/// One row of merged-across-sweep clippy output for the text formatter.
struct MergedDiag<'a> {
    diag: &'a cargo_filter::ClippyDiagnostic,
    sweeps: Vec<String>,
}

/// Merge clippy diagnostics across sweeps, deduplicating by
/// (header, location, message). `parses` is `(label, parse_result)`
/// pairs from each sweep; sweep labels are owned strings since
/// `[[check]]` entry names are user-defined.
fn merge_clippy<'a>(
    parses: &'a [(String, cargo_filter::ClippyParse)],
) -> Vec<MergedDiag<'a>> {
    let mut order: Vec<DiagKey> = Vec::new();
    let mut by_key: HashMap<DiagKey, MergedDiag<'a>> = HashMap::new();

    for (label, parsed) in parses {
        for d in &parsed.diagnostics {
            let key = DiagKey::from(d);
            if let Some(existing) = by_key.get_mut(&key) {
                if !existing.sweeps.contains(label) {
                    existing.sweeps.push(label.clone());
                }
            } else {
                order.push(key.clone());
                by_key.insert(
                    key,
                    MergedDiag {
                        diag: d,
                        sweeps: vec![label.clone()],
                    },
                );
            }
        }
    }

    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .collect()
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct DiagKey(String, String, String);

impl From<&cargo_filter::ClippyDiagnostic> for DiagKey {
    fn from(d: &cargo_filter::ClippyDiagnostic) -> Self {
        DiagKey(
            d.header.clone(),
            d.location.clone().unwrap_or_default(),
            d.message.clone(),
        )
    }
}

/// Render the per-diagnostic sweep tag.
///
/// `active_sweep_count` is the number of sweeps `brokkr check`
/// actually ran for this invocation. The `[both]` shorthand is only
/// honest when the diagnostic appeared in *every* active sweep and
/// there are exactly two of them; with three+ active sweeps,
/// `[both]` would hide which two produced the hit. In that case fall
/// through to the explicit `[a+b]` form.
fn sweep_tag(sweeps: &[String], active_sweep_count: usize) -> Option<String> {
    match sweeps.len() {
        0 => None,
        1 => Some(format!("[{}]", sweeps[0])),
        2 if active_sweep_count == 2 => Some("[both]".to_string()),
        _ => Some(format!("[{}]", sweeps.join("+"))),
    }
}

/// The tag for one merged diagnostic, or `None` when it would say nothing.
///
/// A diagnostic reported by every run that selected its package is a property
/// of the code, not of a build shape, and naming the sweeps only lengthens the
/// line. The tag earns its place when some run covering the package did *not*
/// report it - a diagnostic only one feature shape produces, say - so that is
/// the only time it is printed. `package` is the diagnostic's package id; with
/// no id or no project info every run counts as covering, which tags anything
/// not reported by all of them.
fn coverage_tag(
    reported_by: &[String],
    package: Option<&str>,
    results: &[SweepResult],
    info: Option<&build::ProjectInfo>,
    active_sweep_count: usize,
) -> Option<String> {
    let covering: Vec<&str> = results
        .iter()
        .filter(|r| match (package, info) {
            (Some(id), Some(info)) => r.selects(id, info),
            _ => true,
        })
        .map(|r| r.label.as_str())
        .collect();
    let reported_everywhere = !covering.is_empty()
        && covering.iter().all(|label| reported_by.iter().any(|s| s == label));
    if reported_everywhere {
        return None;
    }
    sweep_tag(reported_by, active_sweep_count)
}

/// Multi-sweep version of the text formatter: gathers each sweep's
/// diagnostics ([`SweepResult::diagnostics`]), merges + dedups them, orders
/// them by scope, and when `multi` tags a line with the sweeps that reported
/// it - only where some sweep covering its package did not ([`coverage_tag`]).
/// A sweep whose cargo failed with no error-level diagnostic also gets its
/// captured streams after the list ([`failed_run_streams`]).
#[allow(clippy::too_many_arguments)]
fn format_clippy_multi(
    tool: &str,
    results: &[SweepResult],
    info: Option<&build::ProjectInfo>,
    project_root: &Path,
    active_sweep_count: usize,
    keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool,
) -> String {
    let multi = active_sweep_count > 1;
    // Each diagnostic's package, keyed as the merge keys it: identical
    // diagnostics from two runs come from the same source line, so the same
    // package.
    let mut package_of: HashMap<DiagKey, String> = HashMap::new();
    let parses: Vec<(String, cargo_filter::ClippyParse)> = results
        .iter()
        .map(|r| {
            let mut events = r.diagnostics();
            events.retain(|d| keep(d));
            let parse = clippy_parse_from_events(&events, !r.success);
            for (d, e) in parse.diagnostics.iter().zip(&events) {
                if let Some(id) = &e.package_id {
                    package_of.entry(DiagKey::from(d)).or_insert_with(|| id.clone());
                }
            }
            (r.label.clone(), parse)
        })
        .collect();

    // A run that failed for a reason the list cannot name gets its captured
    // streams after the list: no error-level diagnostic at all, or an error on
    // stderr the list does not carry - under `--keep-going` a build script can
    // panic in the same run that reports another crate's rustc error, and
    // hiding it until that error is fixed is the one-failure-per-run loop
    // `--keep-going` exists to break.
    let mut streams = String::new();
    for (r, (_, p)) in results.iter().zip(&parses) {
        if r.success || !(p.parse_failed || has_unlisted_stderr_error(r, keep)) {
            continue;
        }
        if multi {
            if !streams.is_empty() {
                streams.push('\n');
            }
            streams.push_str(&format!("[{}]\n", r.label));
        }
        streams.push_str(&failed_run_streams(r, keep));
    }

    let merged = merge_clippy(&parses);

    if merged.is_empty() {
        if !streams.is_empty() {
            return streams;
        }
        return format!("{tool}: no issues");
    }

    let total = merged.len();
    let mut refs: Vec<&MergedDiag<'_>> = merged.iter().collect();
    // Sort so every hit of a single lint clumps together, by lint code, file,
    // line, column; `scope_order` then moves unstaged files to the front,
    // keeping this order within each group. Cached keys keep the location
    // parsing to one pass per diagnostic.
    refs.sort_by_cached_key(|m| clippy_sort_key(m.diag));
    let displayed = scope_order(refs, project_root, |m| {
        m.diag.path().unwrap_or_else(|| Path::new(""))
    });

    let errors = if total == 1 { "error" } else { "errors" };
    let header = if multi {
        format!("{tool}: {total} {errors} ({active_sweep_count} sweeps)\n")
    } else {
        format!("{tool}: {total} {errors}\n")
    };

    let mut out = header;
    for m in &displayed {
        out.push_str("  ");
        if multi
            && let Some(tag) = coverage_tag(
                &m.sweeps,
                package_of.get(&DiagKey::from(m.diag)).map(String::as_str),
                results,
                info,
                active_sweep_count,
            )
        {
            out.push_str(&tag);
            out.push(' ');
        }
        out.push_str(&m.diag.format_one());
        out.push('\n');
    }
    if !streams.is_empty() {
        out.push('\n');
        out.push_str(&streams);
    }
    out.trim_end().to_string()
}

/// The stderr blocks the diagnostic list already carries: the manifest lints
/// the phase's `keep` admitted. Printing them again would say each twice.
fn listed_blocks(
    r: &SweepResult,
    keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool,
) -> std::collections::HashSet<usize> {
    r.manifest.iter().filter(|(_, e)| keep(e)).map(|(i, _)| *i).collect()
}

/// Whether a failed run's stderr holds an `error` block the list does not
/// explain: anything but a listed manifest lint and cargo's own summaries of
/// a failure reported elsewhere (`rustc_text::BlockKind::FailureSummary`).
fn has_unlisted_stderr_error(r: &SweepResult, keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool) -> bool {
    let listed = listed_blocks(r, keep);
    rustc_text::rustc_blocks(&r.stderr)
        .iter()
        .enumerate()
        .any(|(i, b)| {
            b.level == Level::Error
                && !listed.contains(&i)
                && rustc_text::classify_block(b) != BlockKind::FailureSummary
        })
}

/// The captured streams of a failed cargo run whose failure the diagnostic
/// list does not explain - a build script that panicked, a manifest cargo
/// refused, an error an allow filtered. Narrowed by kind, never by a line
/// budget:
///
/// - stdout is cargo's `--message-format=json` event stream, so its records
///   are dropped - they are the bulk of the stream (one `compiler-artifact`
///   record per unit, some tens of KB long; measured on a slint workspace
///   whose fontconfig build script failed: 1118 JSON lines, about 850 KB,
///   beside around 130 lines of cargo's own words). The exception is an
///   error-level `compiler-message` the phase's `keep` rejected, kept as its
///   `rendered` text: an allow filtered it from the list, and it is still a
///   reason cargo failed. Any non-JSON line stays.
/// - stderr, when it holds an `error` block, prints its error blocks and
///   withholds the rest, with a trailer counting the warnings withheld -
///   `append_rustc_errors`'s rule, for the same reason: the same run printed a
///   slint build script's warnings by the hundred beside the one panic that
///   mattered. Every `error` block prints except those the list carries
///   ([`listed_blocks`]), and a build script's `--- stdout` inside one loses
///   its `cargo:` directives ([`drop_build_directives`]). Any other block is
///   withheld only when it is self-contained ([`is_self_contained`]); one that
///   swallowed unindented text after its header prints whole, since that text
///   may be the failure's cause - that check comes before any other reason to
///   drop a block. Not counted as withheld: listed warnings, cargo's
///   `generated N warnings` tallies and its `build failed, waiting` status.
///   With no `error` block, stderr is the evidence and prints verbatim.
fn failed_run_streams(r: &SweepResult, keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool) -> String {
    let mut out = String::new();
    let blocks = rustc_text::rustc_blocks(&r.stderr);
    if blocks.iter().any(|b| b.level == Level::Error) {
        let listed = listed_blocks(r, keep);
        let mut withheld = 0usize;
        for (i, block) in blocks.iter().enumerate() {
            let print = match block.level {
                Level::Error => !listed.contains(&i),
                Level::Warning => {
                    let contained = is_self_contained(&block.text, true);
                    if contained
                        && !listed.contains(&i)
                        && rustc_text::classify_block(block) != BlockKind::CargoStatus
                    {
                        withheld += 1;
                    }
                    !contained
                }
                // cargo's `Compiling` progress is indented; anything else
                // ahead of the first header is kept.
                Level::Other => !is_self_contained(&block.text, false),
            };
            if print {
                let text = if block.level == Level::Error {
                    drop_build_directives(&block.text)
                } else {
                    block.text.clone()
                };
                out.push_str(&text);
                out.push('\n');
            }
        }
        if withheld > 0 {
            out.push_str(&format!("{} not shown\n", output::count(withheld, "warning")));
        }
    } else {
        out.push_str(&r.stderr);
    }
    for text in r.stdout.lines().filter_map(|l| failed_stdout_line(l, keep)) {
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// A failed build script's report with the `cargo:` directives in its
/// `--- stdout` section dropped and counted. Those lines are instructions to
/// cargo - one `cargo:rerun-if-env-changed=` per variable a `-sys` crate
/// probes, dozens before the panic that matters - not an account of the
/// failure. `cargo:warning=` and `cargo::error=` lines are messages and stay,
/// as does everything in `--- stderr` and any non-directive stdout.
fn drop_build_directives(text: &str) -> String {
    let mut out = String::new();
    let mut in_stdout = false;
    let mut dropped = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        match trimmed {
            "--- stdout" => in_stdout = true,
            "--- stderr" => in_stdout = false,
            _ => {}
        }
        let directive = trimmed
            .strip_prefix("cargo::")
            .or_else(|| trimmed.strip_prefix("cargo:"))
            .is_some_and(|rest| !rest.starts_with("warning=") && !rest.starts_with("error="));
        if in_stdout && directive {
            dropped += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if dropped > 0 {
        out.push_str(&format!(
            "  ({} not shown)\n",
            output::count(dropped, "build-script directive")
        ));
    }
    out.trim_end().to_owned()
}

/// Whether a stderr block holds nothing but its own diagnostic: every line
/// (after the header, when `has_header`) blank, indented, a column-zero
/// `help:`/`note:`/`= ` continuation, or a source-excerpt line whose line
/// number reaches column zero (`150 | config = "0.15"`) - the lines rustc and
/// cargo attach to a diagnostic. Text that fails this is something
/// `rustc_text::rustc_blocks` merged into the block only because no header came
/// between.
fn is_self_contained(text: &str, has_header: bool) -> bool {
    text.lines().skip(usize::from(has_header)).all(|l| {
        let after_number = l.trim_start_matches(|c: char| c.is_ascii_digit());
        l.is_empty()
            || l.starts_with(char::is_whitespace)
            || l.starts_with("help:")
            || l.starts_with("note:")
            || l.starts_with("= ")
            || (after_number.len() < l.len() && after_number.starts_with(" |"))
    })
}

/// What one line of a failed run's stdout contributes to its report
/// ([`failed_run_streams`]): a non-JSON line itself, an error-level
/// `compiler-message` that `keep` rejected its `rendered` text, any other
/// cargo record nothing.
/// Parsed rather than prefix-matched, so a tool line that merely starts with
/// `{` survives.
fn failed_stdout_line(line: &str, keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool) -> Option<String> {
    use serde_json::Value;
    let record = line
        .starts_with('{')
        .then(|| serde_json::from_str::<Value>(line).ok())
        .flatten()
        .filter(|v| v.get("reason").is_some_and(Value::is_string));
    let Some(record) = record else {
        return Some(line.to_owned());
    };
    if record.get("reason").and_then(Value::as_str) != Some("compiler-message") {
        return None;
    }
    // The list's own reading of the record: an event it keeps is already
    // shown, and one it parses to nothing is summary noise (`aborting due
    // to ...`).
    let event = cargo_json::parse_cargo_diagnostics(line).into_iter().next()?;
    if !is_error_level(&event) || keep(&event) {
        return None;
    }
    let message = record.get("message")?;
    message.get("rendered").and_then(Value::as_str).map(|s| s.trim_end().to_owned())
}

/// Whether a diagnostic is at error level. rustc spells an ICE `error:
/// internal compiler error`, so a prefix, not an equality.
fn is_error_level(d: &cargo_json::DiagnosticEvent) -> bool {
    d.level.starts_with("error")
}

/// Parse cargo's `--message-format=json` stdout into a
/// [`ClippyParse`](cargo_filter::ClippyParse).
///
/// Walks each compiler-message JSON event and maps it to the formatter
/// primitive used by `merge_clippy` and `format_one()`, in discovery order.
/// `parse_failed` follows [`clippy_parse_from_events`]'s rule.
#[cfg(test)]
fn parse_clippy_from_json(
    stdout: &str,
    sweep_failed: bool,
    allow_exact: &[SitedAllow],
) -> cargo_filter::ClippyParse {
    clippy_parse_from_events(&gated_diags(stdout, allow_exact), sweep_failed)
}

/// Map already-filtered diagnostic events to the formatter primitive, in
/// discovery order. `parse_failed` is set when cargo failed and left no
/// `error`-level event, so callers also print the captured streams: under
/// `--cap-lints=warn` a lint never fails cargo, so a failed run with warnings
/// alone failed for a reason no diagnostic names - a build script's panic
/// beside some crate's lints, which the old "no event at all" rule hid.
fn clippy_parse_from_events(
    events: &[cargo_json::DiagnosticEvent],
    sweep_failed: bool,
) -> cargo_filter::ClippyParse {
    let diagnostics: Vec<cargo_filter::ClippyDiagnostic> =
        events.iter().map(event_to_clippy).collect();
    let parse_failed = sweep_failed && !events.iter().any(is_error_level);
    cargo_filter::ClippyParse {
        diagnostics,
        parse_failed,
    }
}

/// Convert a cargo JSON diagnostic event into the formatter primitive.
///
/// `header` always carries the lint code when cargo populated it (every
/// diagnostic, not just first-of-kind), so bulk triage by rule works in
/// text mode. `detail` is recovered from the primary span's inline
/// label first ("expected `i32`, found `&str`"), then from a child note
/// that mentions both "expected" and "found" - matching the two shapes
/// the old text scraper handled.
///
/// Every event reaching here is an error, whatever level cargo gave it: the
/// one kind of warning `check` does not treat as a failure - a dependency's -
/// was dropped upstream ([`is_dependency_warning`]), and clippy runs under
/// `--cap-lints=warn` only so a denied lint cannot abort its crate's compile.
fn event_to_clippy(d: &cargo_json::DiagnosticEvent) -> cargo_filter::ClippyDiagnostic {
    let header = match &d.code {
        Some(c) => format!("error[{c}]"),
        None => "error".to_string(),
    };
    let location = match (&d.file, d.line, d.column) {
        (Some(f), Some(l), Some(c)) => Some(format!("{f}:{l}:{c}")),
        // A manifest lint cargo could not place within its `Cargo.toml`.
        (Some(f), None, None) => Some(f.clone()),
        _ => None,
    };
    let detail = extract_detail_from_event(d);
    cargo_filter::ClippyDiagnostic {
        is_error: true,
        header,
        location,
        message: d.message.clone(),
        detail,
    }
}

/// A warning from a package outside the workspace: the one diagnostic `check`
/// neither fails on nor lists. A dependency's warnings are its maintainers'
/// business; cargo already silences registry and git dependencies, so what
/// reaches here is a path dependency outside the workspace, such as a sibling
/// checkout. An error from a dependency still fails - the build is broken
/// whoever owns the line.
///
/// A diagnostic with no `package_id`, or a run whose workspace members are
/// unknown, is treated as the workspace's own. Cargo's manifest lints get
/// their id from [`attribute_manifest_lints`], a non-member's included, so the
/// same rule places them.
fn is_dependency_warning(
    d: &cargo_json::DiagnosticEvent,
    members: Option<&HashMap<String, String>>,
) -> bool {
    d.level == "warning"
        && matches!((members, &d.package_id), (Some(m), Some(id)) if !m.contains_key(id))
}

/// Pull a one-line "expected X, found Y" detail out of the primary
/// span label or a child note. Returns `None` if neither shape applies.
fn extract_detail_from_event(d: &cargo_json::DiagnosticEvent) -> Option<String> {
    if let Some(label) = &d.primary_label
        && label.contains("expected")
        && label.contains("found")
    {
        return Some(collapse_whitespace(label));
    }
    for child in &d.children {
        if child.message.contains("expected") && child.message.contains("found") {
            return Some(collapse_whitespace(&child.message.replace('\n', ", ")));
        }
    }
    None
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sort key for the diagnostic list: by lint code (so every hit of a rule
/// clumps together), then file and line for stable in-rule ordering. A bare
/// `error` header (no code) sorts to the end.
fn clippy_sort_key(d: &cargo_filter::ClippyDiagnostic) -> (String, String, u64, u64) {
    let lint = extract_lint_code(&d.header);
    // Push bare-level diagnostics to the end of their level by giving
    // them a key that sorts after any real code.
    let lint_key = if lint.is_empty() {
        "\u{10FFFF}".to_string()
    } else {
        lint.to_string()
    };
    let (file, line, col) = parse_location(d.location.as_deref());
    (lint_key, file, line, col)
}

fn extract_lint_code(header: &str) -> &str {
    if let Some(start) = header.find('[')
        && let Some(end) = header.find(']')
        && start < end
    {
        return &header[start + 1..end];
    }
    ""
}

fn parse_location(location: Option<&str>) -> (String, u64, u64) {
    let Some(loc) = location else {
        return (String::new(), 0, 0);
    };
    let mut parts = loc.rsplitn(3, ':');
    let col = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let line = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let file = parts.next().unwrap_or(loc).to_string();
    (file, line, col)
}

/// Split `brokkr check`'s trailing args into a cargo-level slice and a
/// libtest-level slice on the first literal `--`. With no separator,
/// every token is cargo-level. Documented shapes:
/// - `brokkr check -- --test read_paths` -> cargo: `[--test, read_paths]`,
///   libtest: `[]`.
/// - `brokkr check -- -- --ignored` -> cargo: `[]`,
///   libtest: `[--ignored]`.
/// - `brokkr check -- --test cli -- --ignored` -> cargo: `[--test, cli]`,
///   libtest: `[--ignored]`.
fn split_extra_args(extra: &[String]) -> (&[String], &[String]) {
    match extra.iter().position(|a| a == "--") {
        Some(i) => (&extra[..i], &extra[i + 1..]),
        None => (extra, &[][..]),
    }
}

/// Two execution policies that cannot both hold: one runs every test in its
/// own process, the other runs many binaries' tests at once. Refused here
/// rather than at resolve time because that is where both are read; it fires
/// before any test runs either way.
fn reject_conflicting_lanes(sweep: &ResolvedSweep) -> Result<(), DevError> {
    if sweep.process_isolation && sweep.parallel_budget.is_some() {
        return Err(DevError::Config(format!(
            "sweep '{}' sets both `isolation = \"process\"` and `parallel`; process isolation \
             runs one test at a time, and `parallel` runs many at once. Pick one.",
            sweep.label
        )));
    }
    Ok(())
}

/// The run-wide inputs of the test phase.
struct TestPhaseArgs<'a> {
    project: Option<Project>,
    project_root: &'a Path,
    state_root: &'a Path,
    sweeps: &'a [ResolvedSweep],
    /// The test phase's selection, aligned with `sweeps`.
    selection: &'a PhaseSelection,
    doctests: bool,
    commands: bool,
    extra_args: &'a [String],
    allow: &'a [String],
    allow_exact: &'a [SitedAllow],
    /// The run is held to a `certifies = "complete"` claim.
    certifying: bool,
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
    prepared: Option<&[Option<PreparedLane>]>,
    mut timings: Option<&mut Vec<TestTiming>>,
    ledger: &mut ReachLedger,
    stop: &mut Option<TerminationSummary>,
) -> Result<(), DevError> {
    let TestPhaseArgs {
        project,
        project_root,
        state_root,
        sweeps,
        selection,
        doctests,
        commands,
        extra_args,
        allow,
        allow_exact,
        certifying,
    } = *t;
    let multi = sweeps.len() > 1;
    let allow_flags = crate::config::test_phase_allow_flags(allow, allow_exact);
    announce_test_allows(project_root, sweeps, &allow_flags, allow_exact);
    // `brokkr check`'s test phase runs `cargo test` in the dev profile
    // unless the sweep's `[[check]] profile` says otherwise, so each
    // sweep's `build_packages` artefacts land in that profile's
    // `<target>/` subdirectory. Tests that spawn the just-rebuilt binary
    // read BROKKR_TEST_BIN_DIR to skip the `cfg!(debug_assertions)`
    // profile guess (which silently lies when a workspace pins
    // `[profile.test]` overrides, and now also when a sweep opts into
    // release).
    // `whole_workspace` is a property of the tree, not of any sweep. It now
    // feeds `resolve_unification` rather than the parallel lane directly.
    let build::ProjectInfo { target_dir, .. } =
        build::project_info(Some(project_root))?;

    let mut sweeps_run = 0usize;
    let mut outcome: Result<(), DevError> = Ok(());
    for (i, sweep) in sweeps.iter().enumerate() {
        // The CLI `-p` set intersected with the sweep's selection when the
        // selection was built: ruled-out packages are dropped with a note, and
        // the lane is excluded only when nothing survives. Logged only: `-p`
        // is this invocation's doing, so `check` announced it once, up front
        // (`announce_package_rules`), rather than once per phase.
        let attempt = match selection.entry(i) {
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
        ledger.reach_test(i);

        let tap = LaneTap::new(i);
        tap.record(JournalRecord::LaneStarted { lane: i });
        let lane = run_test_lane(
            &LaneArgs {
                project,
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
            prepared.and_then(|p| p.get(i)).and_then(Option::as_ref),
            &tap,
            timings.as_deref_mut(),
        );
        tap.record(JournalRecord::LaneFinished { lane: i, passed: matches!(lane, Ok(true)) });
        // A green run never printed this sweep's shape, so a failure leaving
        // the lane - a red test, or any error: a pre-build, a lane refusal, a
        // spawn failure - names it here, once, whatever path it took.
        let failed = match lane {
            Ok(true) => None,
            Ok(false) => Some(DevError::Build(TESTS_FAILED.into())),
            Err(e) => Some(e),
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
            let decisive = record_phase_stop(i, sweeps.len(), &e, &tap);
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
///   is recorded as stopped by it, so its planned executions are unobserved
///   for that stated reason, not for none.
/// - A stop (interrupt, phase watchdog) is the run's.
/// - Anything else that ends the phase - a blown budget, a refusal - is the
///   run's too, carrying the lane's own decisive termination when it recorded
///   one: a per-test timeout stops brokkr, and everything after it is
///   unobserved because of that timeout.
fn record_phase_stop(failed: usize, lanes: usize, e: &DevError, tap: &LaneTap) -> Option<Termination> {
    let own = tap.decisive_termination();
    match e {
        DevError::Build(m) if m == TESTS_FAILED => {
            let mut first = None;
            for lane in failed + 1..lanes {
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
        other => record_run_stop(other, own),
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
    sweep: &'a ResolvedSweep,
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
fn build_lane_support(a: &LaneArgs<'_>, sweep: &ResolvedSweep, env: &LaneEnv) -> Result<Vec<SupportArtifact>, DevError> {
    let mut support: Vec<SupportArtifact> = Vec::new();
    for req in a.attempt.support() {
        support.extend(run_sweep_pre_build(a.project_root, sweep, req, &env.project_env, &env.allow_args, a.commands)?);
    }
    Ok(support)
}

/// Run one sweep's test lane: its pre-builds (unless it runs its prepared
/// snapshot), then whichever harness it names. `Ok(false)` is a failure
/// already reported.
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
    tap: &LaneTap,
    timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let sweep = a.sweep;
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
        project: a.project,
        project_root: a.project_root,
        state_root: a.state_root,
        target_dir: a.target_dir,
        allow_flags: a.allow_flags,
        commands: a.commands,
        certifying: a.certifying,
    };
    // Per-sweep: a sweep carrying `rustflags` runs in its own isolated
    // target dir with a matching BROKKR_TEST_BIN_DIR + RUSTFLAGS, so a
    // global cfg (e.g. `--cfg madsim`) never thrashes the plain sweeps. The
    // lint allows reach the pre-build and the test run alike - a pre-build
    // compiling the same crate under the unsuppressed lint fails before
    // `cargo test` is ever reached.
    let env = prepared.map_or_else(|| lane_env(&inputs, sweep), |p| p.env.clone());
    reject_conflicting_lanes(sweep)?;
    let kind = lane_kind(sweep);
    let needs_selection = matches!(kind, LaneKind::Parallel | LaneKind::Isolated | LaneKind::Nextest);

    // A lane that executes its binaries directly runs its prepared snapshot:
    // when every executable it runs or hands its tests still has the content
    // the plan listed, it runs as planned, with no cargo invocation. A build
    // here would only ask whether building again changes anything - and a
    // project whose build scripts rerun on every build answers yes, which is
    // drift this lane never had. Any mismatch takes the rebuild-and-compare
    // path below. A serial lane runs `cargo test`, which builds anyway.
    let unchanged = prepared.is_some_and(|p| {
        needs_selection
            && p.verify
            && p.unavailable.is_none()
            && p.snapshot.as_ref().is_some_and(|s| snapshot_drift(s).is_empty())
    });
    let own;
    let prepared = match prepared {
        Some(p) if unchanged => Some(p),
        // A serial lane whose harnesses cannot be attributed: it runs through
        // cargo, as it always has, with no plan to hold it to.
        Some(p) if p.unavailable.is_some() => {
            build_lane_support(a, sweep, &env)?;
            None
        }
        Some(p) if !p.verify => {
            build_lane_support(a, sweep, &env)?;
            Some(p)
        }
        Some(p) => {
            // Something the plan listed changed since: build the lane again
            // (re-uplifting the support binaries first) and prove it is about
            // to run what was enumerated.
            let support = build_lane_support(a, sweep, &env)?;
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
            build_lane_support(a, sweep, &env)?;
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
            a.doctests,
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
            (a.doctests, a.multi, a.commands, a.certifying),
            timings,
            prepared,
            tap,
        ),
    }
}

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
mod sited_summary_tests {
    #![allow(clippy::unwrap_used)]
    use super::summarize_sited;
    use crate::config::SitedAllow;

    fn sited(entries: &[&str]) -> Vec<SitedAllow> {
        entries.iter().map(|s| SitedAllow::parse(s).unwrap()).collect()
    }

    #[test]
    fn a_lone_site_keeps_its_path() {
        // One entry: the path is short and is the useful part.
        let s = sited(&["deprecated@src/a.rs"]);
        assert_eq!(summarize_sited(&s), "deprecated at src/a.rs");
    }

    #[test]
    fn repeated_lint_collapses_to_a_count() {
        // The regression this exists for: a project with 59 sited entries for
        // one lint printed 59 lines and buried the rest of the run.
        let s = sited(&[
            "clippy::assert_is_empty@a.rs",
            "clippy::assert_is_empty@b.rs",
            "clippy::assert_is_empty@c.rs",
        ]);
        assert_eq!(summarize_sited(&s), "clippy::assert_is_empty (3 files)");
    }

    #[test]
    fn distinct_lints_are_listed_in_first_seen_order() {
        let s = sited(&[
            "deprecated@a.rs",
            "clippy::foo@b.rs",
            "deprecated@c.rs",
            "clippy::foo@d.rs",
        ]);
        assert_eq!(
            summarize_sited(&s),
            "deprecated (2 files), clippy::foo (2 files)"
        );
    }

    #[test]
    fn the_summary_is_one_line_however_many_entries() {
        let entries: Vec<String> = (0..70).map(|i| format!("deprecated@src/f{i}.rs")).collect();
        let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
        let out = summarize_sited(&sited(&refs));
        assert!(!out.contains('\n'), "got: {out}");
        assert_eq!(out, "deprecated (70 files)");
    }
}


#[cfg(test)]
mod ran_labels_tests {
    #![allow(clippy::unwrap_used)]

    use super::ReachLedger;
    use crate::profile::ResolvedSweep;

    fn sweep(label: &str) -> ResolvedSweep {
        ResolvedSweep {
            label: label.into(),
            ..Default::default()
        }
    }

    fn ledger(diagnostics: &[bool], test: &[bool]) -> ReachLedger {
        let mut l = ReachLedger::new(diagnostics.len());
        for (i, d) in diagnostics.iter().enumerate() {
            if *d {
                l.reach_diagnostics(i);
            }
        }
        for (i, t) in test.iter().enumerate() {
            if *t {
                l.reach_test(i);
            }
        }
        l
    }

    #[test]
    fn omits_sweeps_skipped_in_both_phases() {
        // S3-33: profile `tier1` selects [default, ffi, live]; a `-p` scope
        // rules ffi/live out of both clippy and the test phase, so only
        // `default` ran. The trailer must not list ffi/live as green.
        let sweeps = [sweep("default"), sweep("ffi"), sweep("live")];
        let l = ledger(&[true, false, false], &[true, false, false]);
        assert_eq!(l.reached_labels(&sweeps), vec!["default"]);
    }

    #[test]
    fn unions_clippy_only_and_test_only_lanes() {
        // A lane can run in one phase but not the other: `a` clippy-checked
        // with its tests skipped, `b` deduped in clippy but its tests still
        // ran, `c` reached by neither. The honest set is the union of the two.
        let sweeps = [sweep("a"), sweep("b"), sweep("c")];
        let l = ledger(&[true, false, false], &[false, true, false]);
        assert_eq!(l.reached_labels(&sweeps), vec!["a", "b"]);
    }

    #[test]
    fn all_green_reports_every_sweep_in_order() {
        let sweeps = [sweep("default"), sweep("ffi")];
        let l = ledger(&[true, true], &[true, true]);
        assert_eq!(l.reached_labels(&sweeps), vec!["default", "ffi"]);
    }

    #[test]
    fn nothing_ran_reports_empty() {
        // A failure in an early convention phase (e.g. gremlins) returns
        // before any sweep runs, so the trailer honestly lists none.
        let sweeps = [sweep("default"), sweep("ffi")];
        assert!(ledger(&[false, false], &[false, false]).reached_labels(&sweeps).is_empty());
    }
}


#[cfg(test)]
mod package_rules_tests {
    #![allow(clippy::unwrap_used)]

    use super::{package_rules_lines, CheckSelections, FeatureOracle};
    use crate::profile::ResolvedSweep;

    fn sweep(label: &str, packages: &[&str], excluded: &[&str]) -> ResolvedSweep {
        ResolvedSweep {
            label: label.into(),
            packages: packages.iter().map(|p| (*p).to_owned()).collect(),
            test_exclude_packages: excluded.iter().map(|p| (*p).to_owned()).collect(),
            ..Default::default()
        }
    }

    /// The announcement for a run with CLI `-p` set `cli`, every phase but
    /// rustdoc enabled (no `[rustdoc]` table).
    fn lines(sweeps: &[ResolvedSweep], cli: &[&str]) -> Vec<String> {
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

    use super::{reject_extra_args_complete, reject_scoped_complete};
    use crate::config::Certifies;

    #[test]
    fn complete_rejects_trailing_args() {
        // S3-13: `--gate -- -- --skip expensive_` (or `-- --lib`) narrows the
        // real run but not the plan, so the skipped pairs would stay expected
        // with nothing running them. Rejected before anything compiles.
        let args = vec!["--".to_owned(), "--skip".to_owned(), "expensive_".to_owned()];
        let err = reject_extra_args_complete(Some(Certifies::Complete), &args)
            .unwrap_err()
            .to_string();
        assert!(err.contains("trailing"), "got: {err}");
        assert!(err.contains("complete"), "got: {err}");
    }

    #[test]
    fn complete_allows_no_trailing_args() {
        reject_extra_args_complete(Some(Certifies::Complete), &[]).unwrap();
    }

    #[test]
    fn partial_and_legacy_allow_trailing_args() {
        let args = vec!["--lib".to_owned()];
        reject_extra_args_complete(Some(Certifies::Partial), &args).unwrap();
        reject_extra_args_complete(None, &args).unwrap();
    }

    #[test]
    fn scoped_complete_still_rejected() {
        // Sanity: the sibling `-p` guard is unchanged.
        let pkgs = vec!["pkg".to_owned()];
        assert!(reject_scoped_complete(Some(Certifies::Complete), &pkgs).is_err());
        reject_scoped_complete(Some(Certifies::Complete), &[]).unwrap();
        reject_scoped_complete(Some(Certifies::Partial), &pkgs).unwrap();
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
        let pair = |t: &str| PairId { shape: "s".into(), resolution: None, unit: unit.clone(), test: t.into() };
        let lane = LaneRecord {
            prepared: true,
            executions: vec![pair("a"), pair("b")],
            ..LaneRecord::empty(0, "default".into(), LaneKind::Serial, "s".into())
        };
        let plan = AccountingPlan { run_id: "1700000000000-1".into(), complete: true, lanes: vec![lane], ..AccountingPlan::default() };
        Prepared {
            prep: ProfilePrep { plan, lanes: vec![None], error: None },
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
        assert_eq!(phase_sweep_tag("clippy", "clippy"), "clippy");
        assert_eq!(phase_sweep_tag("clippy", "default"), "clippy default");
    }
}

