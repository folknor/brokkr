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
    Certifies, CheckEntry, DependencyRule, Diagnostics, GremlinsConfig, HeaderConfig,
    ManifestConfig, RustdocConfig, NON_SKIPPABLE_PHASES, PHASE_NAMES, QuarantineEntry, ScriptCheck, SitedAllow,
    Stage, TestConfig, TextlintRule,
};
use crate::dependency_rules;
use crate::rustflags;
use crate::error::DevError;
use crate::gremlins;
use crate::output;
use crate::profile::{self, DeclaredFilter, FilterKind, ResolvedSweep};
use crate::project::Project;
use crate::scope;
use crate::script_check::Level;
use crate::test_runner::{self, LibtestOutcome};

#[allow(clippy::too_many_arguments)]
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

    announce_profile_header(&active_sweeps, &profile_label, commands);
    announce_adhoc_shaping(
        features,
        no_default_features,
        test_cfg,
        profile_name,
        commands,
    )?;

    let mut collected_timings: Vec<TestTiming> = Vec::new();
    let mut coverage_stats: Option<CoverageStats> = None;
    // Which active sweeps the test phase actually reached. The phase fails
    // fast, so on a failing run the later lanes never execute - and the
    // coverage audit must not credit their ran-set (S3-18). All true on a
    // green run, so the happy path is unchanged.
    let mut executed = vec![false; active_sweeps.len()];
    // Which sweeps the clippy phase actually invoked cargo for - set after its
    // cli_package_scope skips AND build-shape dedupe, so a sweep clippy never
    // ran (out-of-scope `-p`, or deduped onto an identical shape) stays false.
    // The `--json` trailer's `sweeps` array must list what ran, not the
    // selected set (S3-33); a sweep runs in clippy, in the test phase, or both,
    // so the honest set is the union of this and `executed`. Kept separate from
    // `executed` because clippy runs first - folding it in would mark lanes an
    // earlier test-phase fail-fast never reached, the exact bug S3-18 fixed.
    let mut clippy_ran = vec![false; active_sweeps.len()];
    // Doctests off unless `[test] doctests = true` (nextest/CI never runs them).
    let doctests = test_cfg.is_some_and(|c| c.doctests);

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

    // Run every phase behind one closure so a failure from *any* of them
    // (not just the test phase) still funnels through the summary line below,
    // reporting the same total wall time a passing run does.
    //
    // `failing_phase` names the phase about to run and is cleared on
    // success, so after an Err it names the phase that failed - the one
    // per-phase datum the `--json` summary carries.
    let mut failing_phase: Option<&'static str> = None;
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
                packages,
                clippy_allow,
                clippy_allow_exact,
                quarantine,
                rustdoc_cfg,
                bin_cfg,
                certifies,
                doctests,
                commands,
                extra_args,
            },
            &skip,
            &mut failing_phase,
            &mut clippy_ran,
            &mut executed,
            timings.then_some(&mut collected_timings),
            &mut coverage_stats,
        )
    };
    let outcome = run_phases();

    if timings {
        emit_timings(&collected_timings, active_sweeps.len() > 1);
    }

    let ran_labels = ran_sweep_labels(&active_sweeps, &clippy_ran, &executed);

    // The summary/trailer scope label: the CLI `-p` set, comma-joined so the
    // `--json` `package` field stays a string under `schema: 1`.
    let package_label = (!packages.is_empty()).then(|| packages.join(","));

    finish_check(
        &outcome,
        certifies,
        &profile_label,
        &ran_labels,
        skip_phases,
        !prose_skips.is_empty(),
        package_label.as_deref(),
        failing_phase,
        coverage_stats,
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
    // Both halves always run, so the error names every one that failed - not
    // whichever came first, which would hide a script failure behind textlint's.
    let failed: Vec<String> = [textlint, scripts].into_iter().filter_map(Result::err).map(|e| e.to_string()).collect();
    if failed.is_empty() {
        output::run_msg(&format!("selection passed ({elapsed:.1}s)"));
        return Ok(());
    }
    Err(DevError::Build(failed.join("; ")))
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

/// Header for the collapsed form: name the profile and its sweep set once,
/// so the per-sweep lines below can carry only what differs between them.
/// Printed only when more than one sweep is active.
fn announce_profile_header(
    active_sweeps: &[ResolvedSweep],
    profile_label: &Option<String>,
    commands: bool,
) {
    if commands || active_sweeps.len() <= 1 {
        return;
    }
    let labels: Vec<&str> = active_sweeps.iter().map(|s| s.label.as_str()).collect();
    let n = active_sweeps.len();
    let joined = labels.join(", ");
    match profile_label {
        Some(name) => output::run_msg(&format!("profile {name}: {n} sweeps ({joined})")),
        None => output::run_msg(&format!("{n} sweeps ({joined})")),
    }
}

/// Name what an ad-hoc CLI-features run inherited, and from where.
///
/// An ad-hoc run reports no profile in the header (it certifies nothing and
/// claims no `certifies`), which left the run's filters unstated: the only
/// way to tell "these tests failed" from "these tests were never meant to
/// run here" was to stash the diff and run again. One line closes that.
///
/// Silent on the non-ad-hoc path - the profile header already says it.
fn announce_adhoc_shaping(
    features: &[String],
    no_default_features: bool,
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
    commands: bool,
) -> Result<(), DevError> {
    if commands || (features.is_empty() && !no_default_features) {
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
    packages: &'a [String],
    /// The `[clippy] allow` lint list, suppressed via `-A` on every sweep.
    clippy_allow: &'a [String],
    /// The `[clippy] allow_exact` sited list, filtered at JSON ingestion.
    clippy_allow_exact: &'a [SitedAllow],
    quarantine: &'a [QuarantineEntry],
    /// `[rustdoc]`; `None` leaves the rustdoc phase inert.
    rustdoc_cfg: Option<&'a RustdocConfig>,
    /// `[bin]`, for the install-feature phase's package set and mode.
    bin_cfg: Option<&'a crate::config::BinConfig>,
    certifies: Option<Certifies>,
    doctests: bool,
    commands: bool,
    extra_args: &'a [String],
}

/// Run clippy, the test phase, and (under a `complete` claim) the coverage
/// audit. Threads the two ran-tracking masks - `clippy_ran` (set per sweep
/// after clippy's skip/dedupe) and `executed` (set per lane the test phase
/// reaches before its fail-fast) - plus the `failing_phase` pointer and the
/// coverage stats the summary carries even on a failing run.
fn run_build_phases(
    a: &BuildPhaseArgs<'_>,
    skip: &dyn Fn(&str) -> bool,
    failing_phase: &mut Option<&'static str>,
    clippy_ran: &mut [bool],
    executed: &mut [bool],
    collected_timings: Option<&mut Vec<TestTiming>>,
    coverage_stats: &mut Option<CoverageStats>,
) -> Result<(), DevError> {
    // Voiced here because the summary path deliberately does not echo error
    // messages (phases print their own detail) - an unvoiced refusal reads
    // as `check failed` with no line naming why.
    verify_doc_only_rules(a).inspect_err(|e| output::error(&e.to_string()))?;
    run_diagnostic_phases(a, skip, failing_phase, clippy_ran)?;

    if !skip("script_check") {
        begin_phase(failing_phase, "script_check");
        run_script_checks(a.project_root, a.script_checks, Stage::PreTest)?;
    }

    let mut test_failure: Option<DevError> = None;
    if !skip("test") {
        begin_phase(failing_phase, "test");
        test_failure = run_test_phase(
            a.project,
            a.project_root,
            a.state_root,
            a.active_sweeps,
            a.packages,
            a.doctests,
            a.commands,
            a.extra_args,
            a.clippy_allow,
            a.clippy_allow_exact,
            collected_timings,
            executed,
        )
        .err();
        // The reporting contract: a failing path prints its own detail, and
        // the summary branch adds only the timing line. `tests failed` is the
        // one error whose detail was already reported (the failure list
        // above); everything else leaving the test phase - a lane refusal, a
        // wrong-run, a config conflict discovered at run time - carries its
        // whole diagnostic in the message and was previously swallowed:
        // a refused `isolation` x `parallel` gate died between lanes printing
        // nothing but `check failed` (measured on the consuming config).
        if let Some(e) = &test_failure
            && !matches!(e, DevError::Build(m) if m == "tests failed")
        {
            output::error(&e.to_string());
        }
    }
    finish_build_phases(a, skip, failing_phase, executed, coverage_stats, test_failure)
}

/// The two per-build-shape diagnostic phases: clippy, then rustdoc. Rustdoc
/// comes second because it reuses clippy's compiled dependencies, and before
/// the tests because a doc link is cheaper to fail on than a test suite.
fn run_diagnostic_phases(
    a: &BuildPhaseArgs<'_>,
    skip: &dyn Fn(&str) -> bool,
    failing_phase: &mut Option<&'static str>,
    ran: &mut [bool],
) -> Result<(), DevError> {
    if !skip("clippy") {
        begin_phase(failing_phase, "clippy");
        run_clippy_phase(
            a.project_root,
            a.active_sweeps,
            a.packages,
            a.clippy_allow,
            a.clippy_allow_exact,
            a.commands,
            ran,
        )?;
    }

    if !skip("rustdoc") {
        begin_phase(failing_phase, "rustdoc");
        run_rustdoc_phase(
            a.project_root,
            a.rustdoc_cfg,
            a.active_sweeps,
            a.packages,
            a.clippy_allow,
            a.clippy_allow_exact,
            a.commands,
            ran,
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
    executed: &[bool],
    coverage_stats: &mut Option<CoverageStats>,
    test_failure: Option<DevError>,
) -> Result<(), DevError> {
    // Coverage accounting runs only under a complete claim - it is what the
    // claim buys. It runs on a failing test phase
    // too: the audit needs built binaries, not green tests, and the orphan
    // worksheet is most needed exactly on the unhealthy runs.
    if a.certifies == Some(Certifies::Complete) {
        // Stays "test" on a failing run - the audit is best-effort there.
        begin_phase(failing_phase, if test_failure.is_some() { "test" } else { "coverage" });
        // The audit's enumeration compiles, so it needs the test phase's lint
        // allows for the same reason the test phase does - derived from the
        // same source here rather than passed down, so the two cannot drift.
        let audit = audit_coverage(
            a.project_root,
            a.state_root,
            a.active_sweeps,
            executed,
            a.quarantine,
            a.commands,
            &crate::config::test_phase_allow_flags(a.clippy_allow, a.clippy_allow_exact),
            test_failure.as_ref(),
        );
        // Counts first, verdict second: the summary carries them even when
        // the audit is what failed.
        *coverage_stats = audit.stats;
        // Same reporting contract as the test phase: `coverage failed` is the
        // sentinel whose detail (worksheets, orphans) already printed; any
        // other error - an enumeration abort, an engine failure - carries its
        // whole diagnostic in the message and would otherwise be silent.
        if let Err(e) = &audit.result
            && !matches!(e, DevError::Build(m) if m == "coverage failed")
        {
            output::error(&e.to_string());
        }
        audit.result?;
    }

    if let Some(e) = test_failure {
        return Err(e);
    }

    // Only past a green test phase: the test phase fails fast, so a post-test
    // gate on a failing run would be judging a tree whose later lanes never
    // ran. Unlike the coverage audit above, which is deliberately best-effort
    // there, a script-check has no partial-run reading - it just lies.
    if !skip("script_check") {
        begin_phase(failing_phase, "script_check");
        run_script_checks(a.project_root, a.script_checks, Stage::PostTest)?;
    }

    // Last on purpose: package-mode resolution can compile duplicate variants
    // of shared dependencies, making this the most expensive phase on a cold
    // store, and the repo's ordering rule is cheap-fails-first.
    if !skip("install_feature") {
        begin_phase(failing_phase, "install_feature");
        run_install_feature_phase(
            a.project_root,
            a.bin_cfg,
            a.packages,
            a.certifies,
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
    let doc_sweeps: Vec<&ResolvedSweep> =
        a.active_sweeps.iter().filter(|s| s.doc_only).collect();
    if !doc_sweeps.is_empty() {
        let (cargo_extra, _) = split_extra_args(a.extra_args);
        let (selectors, _) = partition_target_selectors(cargo_extra);
        if !selectors.is_empty() {
            return Err(DevError::Config(format!(
                "sweep '{}' is doc-only (`cargo test --doc`, an exclusive selector), and the \
                 forwarded args carry the target selector(s) {}. Drop them, or run a profile \
                 without the doc-only sweep.",
                doc_sweeps[0].label,
                selectors.join(" ")
            )));
        }
        for s in &doc_sweeps {
            if s.process_isolation || matches!(s.test_threads, Some(n) if n != 1) {
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
        if !s.libtest_args.is_empty()
            || !s.name_filters.is_empty()
            || !s.qualified_skips.is_empty()
            || !s.cargo_test_filters.is_empty()
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
            let carries = s.doc_only
                || (s.harness == crate::config::Harness::Libtest
                    && s.parallel_budget.is_none()
                    && !s.process_isolation
                    && s.cargo_test_filters.is_empty()
                    && a.doctests);
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

/// Run the coverage audit for a complete claim. On a green test phase an
/// audit failure fails the run; when the tests themselves failed, the
/// audit is best-effort (its findings still print - they are the
/// worksheet) and is skipped entirely when the failure predates built
/// binaries (anything other than the test phase's own "tests failed").
///
/// Either way the counts ride out with the outcome: a failed audit that
/// reported `coverage: null` was the same died-before-reporting shape the
/// best-effort path above exists to fix.
#[allow(clippy::too_many_arguments)]
fn audit_coverage(
    project_root: &Path,
    state_root: &Path,
    sweeps: &[ResolvedSweep],
    executed: &[bool],
    quarantine: &[QuarantineEntry],
    commands: bool,
    allow_flags: &[String],
    test_failure: Option<&DevError>,
) -> CoverageOutcome {
    match test_failure {
        None => run_coverage_phase(
            project_root,
            state_root,
            sweeps,
            executed,
            quarantine,
            allow_flags,
            commands,
        ),
        Some(DevError::Build(msg)) if msg == "tests failed" => {
            // The test failure is the run's verdict; the audit only
            // contributes its worksheet and its counts.
            let outcome = run_coverage_phase(
                project_root,
                state_root,
                sweeps,
                executed,
                quarantine,
                allow_flags,
                commands,
            );

            CoverageOutcome { stats: outcome.stats, result: Ok(()) }
        }
        Some(_) => CoverageOutcome { stats: None, result: Ok(()) },
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

/// The certifies permission table governs CLI flags too: `-p` scopes the
/// build, and a scoped green is not comparable to the full green (feature
/// unification changes with the package set - the B41 hazard), so a
/// complete profile rejects it before anything compiles.
/// The ordinary (non-parallel, non-isolated) test lane: one `cargo test` per
/// cargo RESOLUTION.
///
/// A package-mode sweep tests each of its packages alone, because a batched
/// multi-`-p` run resolves a graph none of them install under. Every other mode
/// yields a single unscoped resolution, so this is one iteration and the argv
/// is what it always was.
#[allow(clippy::too_many_arguments)]
fn run_sequential_resolutions(
    project_root: &Path,
    state_root: &Path,
    sweep: &ResolvedSweep,
    scope: &[&str],
    extra_args: &[String],
    project_env: &[(String, String)],
    allow_args: &[String],
    // (doctests, multi, commands)
    flags: (bool, bool, bool),
    mut timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let owned: Vec<String> = scope.iter().map(|s| (*s).to_owned()).collect();
    let mut all_passed = true;
    for resolution in sweep.resolutions(&owned) {
        let run_scope: Vec<&str> = match &resolution {
            Some(pkg) => vec![pkg.as_str()],
            None => scope.to_vec(),
        };
        let passed = run_one_test_sweep(
            project_root,
            state_root,
            sweep,
            &run_scope,
            extra_args,
            project_env,
            allow_args,
            flags.0,
            flags.1,
            flags.2,
            timings.as_deref_mut(),
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
        s.parallel_budget.is_some() && unify_workspace(s, &cli_scope, true, cargo_extra)
    });
    let whole_workspace = if any_candidate {
        build::project_info(Some(project_root))?.bare_selection_is_whole_workspace
    } else {
        false
    };

    for sweep in sweeps.iter_mut() {
        let eligible = sweep.parallel_budget.is_some()
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
/// the coverage audit enumerates each lane's ran-set from the sweep's own
/// filters alone, without them. Those narrowed-away pairs would land in
/// `ran`, so the audit would certify `complete` over tests that never
/// executed. Reject trailing args under a complete claim, exactly like
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
             cargo `--lib`) but not the coverage audit, so the audit would \
             count tests that never ran. Use a partial profile for ad-hoc \
             narrowing, or fold the selection into a `[[check]]` entry."
                .into(),
        ));
    }
    Ok(())
}

/// The labels of the sweeps that actually ran, for the honest `--json`
/// trailer (S3-33). A sweep counts if the clippy phase invoked cargo for it
/// (`clippy_ran`, set after cli_package_scope skips and build-shape dedupe)
/// **or** the test phase reached it (`executed`, the S3-18 array) - it may run
/// in one phase, the other, or both. A sweep skipped in both (an out-of-scope
/// `-p`, or a lane an earlier fail-fast never reached) is omitted, so the
/// machine trailer never lists a sweep as green that never ran - the same
/// over-claim the `package` field was added to prevent.
fn ran_sweep_labels<'a>(
    sweeps: &'a [ResolvedSweep],
    clippy_ran: &[bool],
    executed: &[bool],
) -> Vec<&'a str> {
    sweeps
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            clippy_ran.get(*i).copied().unwrap_or(false)
                || executed.get(*i).copied().unwrap_or(false)
        })
        .map(|(_, s)| s.label.as_str())
        .collect()
}

/// Print the summary line, emit the `--json` trailer, and map the claim to
/// the exit contract. The claim decides the word and the exit code: `passed`
/// stays with unclaimed legacy profiles (exactly as trustworthy as before
/// `certifies` existed), `complete` owns the gate verdict, and `partial` may
/// never print a success word a grep could mistake for one - it exits 10 so
/// naive `&& git commit` chaining fails closed. Any failure exits 1.
#[allow(clippy::too_many_arguments)]
fn finish_check(
    outcome: &Result<(), DevError>,
    certifies: Option<Certifies>,
    profile_label: &Option<String>,
    sweep_labels: &[&str],
    skip_phases: &[String],
    prose_only: bool,
    package: Option<&str>,
    failing_phase: Option<&'static str>,
    coverage: Option<CoverageStats>,
    json: bool,
    started: std::time::Instant,
) -> Result<(), DevError> {
    match outcome {
        Ok(()) => match certifies {
            None => {
                // A shortened run must not sign off in the same words a full
                // one does - the announcement at the top has scrolled away by
                // the time this line is read.
                let scope = if prose_only {
                    " (markdown only - build phases skipped)"
                } else {
                    ""
                };
                output::result_msg(&format!(
                    "check passed{scope} in {}",
                    fmt_wall(started.elapsed())
                ));
                if json {
                    emit_json_summary(
                        "passed",
                        certifies,
                        profile_label,
                        sweep_labels,
                        package,
                        None,
                        coverage,
                        started.elapsed(),
                    );
                }
                Ok(())
            }
            Some(Certifies::Complete) => {
                output::result_msg(&format!("check complete in {}", fmt_wall(started.elapsed())));
                if json {
                    emit_json_summary(
                        "complete",
                        certifies,
                        profile_label,
                        sweep_labels,
                        package,
                        None,
                        coverage,
                        started.elapsed(),
                    );
                }
                Ok(())
            }
            Some(Certifies::Partial) => {
                let mut narrowed: Vec<String> = Vec::new();

                if !skip_phases.is_empty() {
                    narrowed.push(format!("skipped phases: {}", skip_phases.join(", ")));
                }

                if let Some(p) = package {
                    narrowed.push(format!("scoped to -p {p}"));
                }
                let suffix = if narrowed.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", narrowed.join("; "))
                };
                output::result_msg(&format!(
                    "check partial in {}{suffix}",
                    fmt_wall(started.elapsed())
                ));
                if json {
                    emit_json_summary(
                        "partial",
                        certifies,
                        profile_label,
                        sweep_labels,
                        package,
                        None,
                        coverage,
                        started.elapsed(),
                    );
                }
                Err(DevError::ExitCode(10))
            }
        },
        Err(_) => {
            // The failing phase already printed its detail above; add the
            // symmetric summary line and exit non-zero without main echoing a
            // second, timing-less `[error]` line.
            output::error(&format!("check failed in {}", fmt_wall(started.elapsed())));
            if json {
                emit_json_summary(
                    "failed",
                    certifies,
                    profile_label,
                    sweep_labels,
                    package,
                    failing_phase,
                    coverage,
                    started.elapsed(),
                );
            }
            Err(DevError::ExitCode(1))
        }
    }
}

/// The `--json` summary object: one line, last on stdout. Versioned
/// and additive: fields are only ever added under
/// `schema: 1`, consumers must tolerate unknown ones, and a bump is
/// reserved for renames or semantic changes. `certifies` mirrors the
/// resolved profile's claim (`null` for unclaimed profiles); `verdict` is
/// `passed`/`complete`/`partial`/`failed`, paired with exit codes 0/0/10/1.
#[derive(serde::Serialize)]
struct CheckSummary<'a> {
    schema: u32,
    certifies: Option<&'a str>,
    verdict: &'a str,
    profile: Option<&'a str>,
    sweeps: Vec<&'a str>,
    /// The CLI `-p` scope, when one narrowed the run - a consumer must be
    /// able to see that a green covered specific packages, not the
    /// workspace. A multi-package run joins the names with commas (still a
    /// string, keeping the field additive under `schema: 1`).
    package: Option<&'a str>,
    failed_phase: Option<&'a str>,
    /// Coverage accounting result; present only when the coverage phase
    /// ran to completion (complete profiles).
    coverage: Option<CoverageStats>,
    elapsed_ms: u64,
}

#[allow(clippy::too_many_arguments)]
fn emit_json_summary(
    verdict: &str,
    certifies: Option<Certifies>,
    profile: &Option<String>,
    sweeps: &[&str],
    package: Option<&str>,
    failed_phase: Option<&'static str>,
    coverage: Option<CoverageStats>,
    elapsed: std::time::Duration,
) {
    let summary = CheckSummary {
        schema: 1,
        certifies: certifies.map(|c| match c {
            Certifies::Complete => "complete",
            Certifies::Partial => "partial",
        }),
        verdict,
        profile: profile.as_deref(),
        sweeps: sweeps.to_vec(),
        package,
        failed_phase,
        coverage,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    };
    match serde_json::to_string(&summary) {
        Ok(line) => println!("{line}"),
        Err(e) => output::error(&format!("--json summary serialization failed: {e}")),
    }
}

/// Format a wall-clock duration as a compact `1m23s` (or `8.4s` under a
/// minute) for the `brokkr check` summary line.
fn fmt_wall(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    // Whole seconds straight from the duration - no float->int cast to trip
    // the truncation/sign-loss lints, and sub-second precision is noise at
    // this scale anyway.
    let total = d.as_secs();
    format!("{}m{:02}s", total / 60, total % 60)
}

/// One test timing observation, tagged with its sweep label so the merged
/// descending list can show which sweep an entry came from.
pub(crate) struct TestTiming {
    pub(crate) sweep: String,
    pub(crate) name: String,
    pub(crate) elapsed: std::time::Duration,
}

fn emit_timings(timings: &[TestTiming], multi_sweep: bool) {
    if timings.is_empty() {
        output::run_msg("timings: no tests ran");
        return;
    }

    let mut sorted: Vec<&TestTiming> = timings.iter().collect();
    sorted.sort_by_key(|t| std::cmp::Reverse(t.elapsed));

    let mut msg = format!("timings: {}, slowest first\n", output::count(sorted.len(), "test"));
    for t in sorted {
        let secs = t.elapsed.as_secs_f64();
        if multi_sweep {
            msg.push_str(&format!("  {secs:>7.3}s [{}] {}\n", t.sweep, t.name));
        } else {
            msg.push_str(&format!("  {secs:>7.3}s {}\n", t.name));
        }
    }
    output::run_msg(msg.trim_end());
}

/// Build the list of sweeps both phases iterate, applying the
/// priority ladder documented at the top of the file.
///
/// Returns `Err` only when the user asked for a `--profile` that
/// doesn't resolve. Every other branch always succeeds with at least
/// one sweep.
/// Escape sentences for a cross-entry `env` disagreement. `clippy` can pick a
/// value (`--env`) or replay one entry (`--sweep`); `check` has neither flag,
/// so its way out is to stop being ad-hoc.
const CLIPPY_ENV_CONFLICT_REMEDY: &str = "`brokkr clippy` can't pick one; pass \
     `--env KEY=...` to choose, or `--sweep NAME` to run one entry.";
const CHECK_ENV_CONFLICT_REMEDY: &str = "an ad-hoc `--features` run takes no \
     `[[check]]` entry and can't pick one; run a profile (or `-p` with no \
     `--features`) instead, or reconcile the entries' `env`.";

pub(crate) fn decide_active_sweeps(
    check_entries: &[CheckEntry],
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
    features: &[String],
    no_default_features: bool,
) -> Result<Vec<ResolvedSweep>, DevError> {
    // 1. CLI override: ad-hoc one-off sweep. Overrides *sweep selection* -
    //    it takes no `[[check]]` entry, and ships no `build_packages` (the
    //    user is spot-checking; if they need a CLI rebuild they pass
    //    --package).
    //
    //    It does NOT discard the profile's run shaping. Sweep selection
    //    decides what cargo compiles; the profile's `skip`/`only`/
    //    `test_threads`/`isolation` decide which tests run and how, and the
    //    two are independent - a test that cannot pass in-process cannot
    //    pass in-process at any feature shape. This branch used to drop both
    //    together, so `check -p <pkg> --features x` issued a bare `cargo
    //    test` with no `--skip` at all and failed on tests the profile had
    //    always excluded, reporting a red indistinguishable from a code
    //    failure. `announce_adhoc_shaping` says which profile was applied.
    if !features.is_empty() || no_default_features {
        let mut feature_args = Vec::new();
        if no_default_features {
            feature_args.push("--no-default-features".into());
        }
        if !features.is_empty() {
            feature_args.push("--features".into());
            feature_args.push(features.join(","));
        }
        let mut sweep = ResolvedSweep {
            label: "default".into(),
            cargo_feature_args: feature_args,
            build_packages: Vec::new(),
            packages: Vec::new(),
            libtest_args: Vec::new(),
            cargo_test_filters: Vec::new(),
            name_filters: Vec::new(),
            env: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        if let Some(cfg) = test_cfg
            && let Some(name) = effective_profile_name(test_cfg, profile_name)?
        {
            profile::run_shaping(cfg, &name)?.apply(&mut sweep);
        }
        // Entry-level env, unioned across every `[[check]]` entry - the same
        // `merge_check_envs` `brokkr clippy`'s ad-hoc path uses, so the two
        // commands compile the same shape from the same config. Without it a
        // build-affecting invariant carried by the entries rather than the
        // profile (a var a build script reads, a codegen toggle) was silently
        // dropped, and a probe could go green having compiled something other
        // than what the gate compiles - a quiet wrong, where the dropped-skips
        // bug was a loud one. A key two entries disagree on is a hard error,
        // never a coin flip.
        //
        // This does NOT reach an invariant expressed as a cargo *feature*:
        // that follows feature resolution, which follows the CLI scope, and no
        // env union restores it.
        //
        // Entry env overlays profile env, matching `build_resolved_sweep`.
        //
        // `test_exclude_packages` is deliberately NOT unioned: it is a
        // per-entry selection workaround, not an invariant, so unioning it
        // would narrow what a scoped run tests while everything else about the
        // run claims to be wider. That residual stays with the caller.
        for (k, v) in merge_check_envs(
            check_entries,
            &std::collections::BTreeSet::new(),
            CHECK_ENV_CONFLICT_REMEDY,
        )? {
            sweep.env.insert(k, v);
        }
        return Ok(vec![sweep]);
    }

    // 2. Explicit --profile or default_profile from [test].
    if let Some(name) = effective_profile_name(test_cfg, profile_name)? {
        // Safe to unwrap: effective_profile_name returns Some only when
        // test_cfg is Some.
        let cfg = test_cfg.expect("test_cfg known present");
        return profile::resolve(cfg, check_entries, &name);
    }

    // 3. [[check]] entries with no profile - run every entry in order,
    //    with no libtest filters.
    if !check_entries.is_empty() {
        return Ok(check_entries
            .iter()
            .map(profile::sweep_from_check_entry)
            .collect());
    }

    // 4. Legacy fallback: `brokkr check` against a project with no
    //    `[[check]]` and no profile config. One `--all-features`
    //    invocation, matching pre-redesign behaviour. Label tracks
    //    the cargo flag so callers (e.g. `brokkr test`) don't have
    //    to special-case-detect this branch by feature-arg shape.
    Ok(vec![ResolvedSweep {
        label: "all-features".into(),
        cargo_feature_args: vec!["--all-features".into()],
        build_packages: Vec::new(),
        packages: Vec::new(),
        libtest_args: Vec::new(),
        cargo_test_filters: Vec::new(),
        name_filters: Vec::new(),
        env: std::collections::BTreeMap::new(),
        ..Default::default()
    }])
}

/// Return `Some(name)` if a profile should be resolved. Errors when
/// the user passed `--profile <name>` but the project has no `[test]`
/// section at all (loud failure beats silent fallback).
fn effective_profile_name(
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
) -> Result<Option<String>, DevError> {
    match (test_cfg, profile_name) {
        (Some(_), Some(n)) => Ok(Some(n.to_owned())),
        (Some(cfg), None) => Ok(cfg.default_profile.clone()),
        (None, Some(n)) => Err(DevError::Config(format!(
            "--profile {n} requires `[test.profiles.{n}]` in brokkr.toml; \
             no `[test]` section is defined."
        ))),
        (None, None) => Ok(None),
    }
}

fn run_gremlins(
    project_root: &Path,
    config: Option<&GremlinsConfig>,
    fix: bool,
) -> Result<(), DevError> {
    // `[gremlins] disable = true` skips the whole phase - both the scan and
    // `--fix-gremlins`.
    if config.is_some_and(|c| c.disable) {
        output::run_msg("gremlins: disabled by config");
        return Ok(());
    }

    if fix {
        let fixed = gremlins::fix(project_root, config)?;
        let total: usize = fixed.iter().map(|f| f.count).sum();
        if total == 0 {
            output::run_msg("fix-gremlins: nothing to fix");
        } else {
            output::run_msg(&format!(
                "fix-gremlins: rewrote {} across {}",
                output::count(total, "char"),
                output::count(fixed.len(), "file")
            ));
            for f in &fixed {
                output::run_msg(&format!("  {} ({})", f.path.display(), f.count));
            }
        }
    }

    let found = gremlins::scan(project_root, config)?;

    if found.is_empty() {
        output::run_msg("zero gremlins!");
        return Ok(());
    }

    let total = found.len();
    let displayed = scope_order(found, project_root, |g| g.path.as_path());

    let mut msg = format!("gremlins: {total} found\n");
    for g in &displayed {
        msg.push_str("  ");
        msg.push_str(&gremlins::format_one(g));
        msg.push('\n');
    }
    msg.push_str("  hint: rerun with `brokkr check --fix-gremlins` to rewrite all banned chars in place\n");
    output::error(msg.trim_end());
    Err(DevError::Build("gremlins found".into()))
}

/// Order a phase's errors for display via [`scope::prioritize`]: the errors in
/// files with unstaged changes first, then the rest. Every error is displayed -
/// there is no cap. `get_path` maps an error to its file.
///
/// The unstaged set is computed here rather than once in `cmd_check` so the git
/// call is paid only when a phase actually has errors to display. Phases fail
/// fast, so at most one of them reaches this per invocation.
fn scope_order<T>(
    violations: Vec<T>,
    project_root: &Path,
    get_path: impl Fn(&T) -> &Path,
) -> Vec<T> {
    let unstaged = scope::unstaged_files(project_root);
    scope::prioritize(violations, get_path, unstaged.as_ref())
}

/// The `[header]` phase: a required file header whose year must be current.
/// Inert unless the project has a `[header]` section.
fn run_header(
    project_root: &Path,
    header_cfg: Option<&HeaderConfig>,
) -> Result<(), DevError> {
    let Some(cfg) = header_cfg else {
        return Ok(());
    };
    let year = crate::header::current_utc_year()?;
    let expected = crate::header::expand(&cfg.pattern, year);

    let violations = crate::header::scan(project_root, cfg, year)?;

    if violations.is_empty() {
        output::run_msg("header: ok");
        return Ok(());
    }

    output::run_msg(&format!("header: require `{expected}`"));
    let total = violations.len();
    let displayed = scope_order(violations, project_root, |v| v.file.as_path());
    let mut msg = format!("header: {}\n", output::count(total, "violation"));
    for v in &displayed {
        msg.push_str("  ");
        msg.push_str(&crate::header::format_one(v, &expected));
        msg.push('\n');
    }
    output::error(msg.trim_end());

    Err(DevError::Build("header check failed".into()))
}

/// The `[[textlint]]` phase: declarative forbid-a-pattern line rules. Inert
/// unless the project defines `[[textlint]]` entries.
fn run_textlint(
    project_root: &Path,
    rules: &[TextlintRule],
) -> Result<(), DevError> {
    if rules.is_empty() {
        return Ok(());
    }

    let scan = crate::textlint::scan(project_root, rules)?;
    let violations = scan.violations;

    if violations.is_empty() {
        // Counts on the green line, matching `dependency rules`: "ok" alone
        // cannot distinguish a clean tree from a rule whose `paths` glob has
        // stopped matching anything.
        output::run_msg(&format!(
            "textlint: ok ({}, {})",
            output::count(rules.len(), "rule"),
            output::count(scan.files, "file")
        ));
        return Ok(());
    }

    output::run_msg(&format!("textlint: {}", output::count(rules.len(), "rule")));
    let total = violations.len();
    let displayed = scope_order(violations, project_root, |v| v.file.as_path());
    let mut msg = format!("textlint: {}\n", output::count(total, "violation"));
    for v in &displayed {
        msg.push_str("  ");
        msg.push_str(&crate::textlint::format_one(v));
        msg.push('\n');
    }
    output::error(msg.trim_end());

    Err(DevError::Build("textlint failed".into()))
}

/// The `[manifest]` phase: native structural `Cargo.toml` conventions. Inert
/// unless the project has a `[manifest]` section with at least one check on.
fn run_manifest(
    project_root: &Path,
    manifest_cfg: Option<&ManifestConfig>,
) -> Result<(), DevError> {
    let Some(cfg) = manifest_cfg else {
        return Ok(());
    };

    let violations = crate::manifest::scan(project_root, cfg)?;

    if violations.is_empty() {
        output::run_msg("manifest: ok");
        return Ok(());
    }

    output::run_msg("manifest: Cargo.toml conventions");
    let total = violations.len();
    let displayed = scope_order(violations, project_root, |v| v.file.as_path());
    let mut msg = format!("manifest: {}\n", output::count(total, "violation"));
    for v in &displayed {
        msg.push_str("  ");
        msg.push_str(&crate::manifest::format_one(v));
        msg.push('\n');
    }
    output::error(msg.trim_end());

    Err(DevError::Build("manifest check failed".into()))
}

/// The `[[script_check]]` phase for one [`Stage`]: run the configured commands
/// that named this stage and assert each one's output matches its sentinel.
/// Inert when no entry sits at this stage. Every check runs (failures are
/// collected, not fail-fast) so one `brokkr check` surfaces all broken gates at
/// once. The command's exit code is ignored - only the output match decides
/// pass/fail; a spawn failure is a hard error. See [`crate::script_check`].
fn run_script_checks(
    project_root: &Path,
    checks: &[ScriptCheck],
    stage: Stage,
) -> Result<(), DevError> {
    let checks: Vec<&ScriptCheck> = checks.iter().filter(|c| c.stage == stage).collect();
    if checks.is_empty() {
        return Ok(());
    }

    let total = checks.len();
    let mut failures: Vec<(&ScriptCheck, crate::script_check::Outcome)> = Vec::new();
    for check in checks {
        let outcome = crate::script_check::run_one(check, project_root)?;
        if !outcome.passed {
            failures.push((check, outcome));
        }
    }

    // One line for the stage, not one per check: a passing gate's name carries
    // no information a reader can act on, and a project with twenty of them
    // buried the rest of the run under a wall of `ok`. The count is what makes
    // the line falsifiable - it says which corpus passed, so a stage that
    // silently stopped running its checks is visible as a shrinking number.
    // Failures are unaffected; each one still prints its captured output below.
    if failures.is_empty() {
        output::run_msg(&format!("script-check: ok ({})", output::count(total, "check")));
        return Ok(());
    }
    if failures.len() < total {
        output::run_msg(&format!(
            "script-check: {} of {total} ok",
            total - failures.len()
        ));
    }

    let mut msg = format!("script-check: {} failed\n", failures.len());
    for (check, outcome) in &failures {
        msg.push_str("  ");
        msg.push_str(&check.name);
        msg.push_str(&format!(
            ": {} did not match {:?}\n",
            stream_label(check.stream),
            check.expect
        ));
        append_script_failure(&mut msg, check, outcome);
    }
    output::error(msg.trim_end());

    Err(DevError::Build("script-check failed".into()))
}

/// Render one failing script-check's captured output.
///
/// The captured stream IS the diagnostic here - brokkr never saw the command's
/// internals, only what it printed - so the rendering question is which part of
/// it to show, not how much. Two paths:
///
/// - `diagnostics = "rustc"` with at least one `error` block: the error blocks
///   alone, with a trailer counting the warnings withheld. Measured on the
///   consuming config (a `cargo doc --workspace` gate): 27 denied
///   `broken-intra-doc-links` errors against several hundred
///   `private-intra-doc-links` warnings from every other crate - a 1:20
///   signal-to-noise ratio, with the signal uniformly at `error`. Printing the
///   failing level alone fit it in a third of a screen.
/// - Everything else: both streams verbatim.
///
/// A `rustc` entry with no error block falls through to the verbatim view
/// rather than printing an empty section: a gate can fail because its sentinel
/// never appeared at all (the command died, or was stubbed), and that failure's
/// evidence is the output, not a diagnostic level that isn't there.
///
/// Nothing here is capped by line count. The only narrowing is by diagnostic
/// level, which is a claim about what the reader needs; a line budget was a
/// claim about how much they could stand, and it hid the one error that
/// mattered as readily as the noise around it.
fn append_script_failure(
    msg: &mut String,
    check: &ScriptCheck,
    outcome: &crate::script_check::Outcome,
) {
    if check.diagnostics == Diagnostics::Rustc {
        let stdout = String::from_utf8_lossy(&outcome.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&outcome.stderr).into_owned();
        let streams = [
            ("stdout", crate::script_check::rustc_blocks(&stdout)),
            ("stderr", crate::script_check::rustc_blocks(&stderr)),
        ];
        if streams
            .iter()
            .any(|(_, blocks)| blocks.iter().any(|b| b.level == Level::Error))
        {
            append_rustc_errors(msg, &streams);
            return;
        }
    }
    append_captured_stream(msg, "stdout", &outcome.stdout);
    append_captured_stream(msg, "stderr", &outcome.stderr);
}

/// Append every `error`-level block of a rustc-shaped failure, then one trailer
/// counting the warnings that were not printed.
///
/// The trailer exists because the omission is a choice brokkr made, not one the
/// command made: a reader who sees only errors must be told the warnings were
/// there, or a `warning:`-shaped cause reads as absent rather than withheld.
fn append_rustc_errors(msg: &mut String, streams: &[(&str, Vec<crate::script_check::Block>)]) {
    let mut warnings = 0usize;
    for (label, blocks) in streams {
        let errors: Vec<&crate::script_check::Block> =
            blocks.iter().filter(|b| b.level == Level::Error).collect();
        warnings += blocks.iter().filter(|b| b.level == Level::Warning).count();
        if errors.is_empty() {
            continue;
        }
        msg.push_str(&format!("    --- {label} (errors) ---\n"));
        for block in errors {
            for line in block.text.lines() {
                msg.push_str("    ");
                msg.push_str(line);
                msg.push('\n');
            }
        }
    }

    if warnings > 0 {
        msg.push_str(&format!("    {} not shown\n", output::count(warnings, "warning")));
    }
}

/// The stream(s) a script-check matched against, for its failure line.
fn stream_label(stream: crate::config::Stream) -> &'static str {
    match stream {
        crate::config::Stream::Stdout => "stdout",
        crate::config::Stream::Stderr => "stderr",
        crate::config::Stream::Both => "stdout+stderr",
    }
}

/// Append a labelled, indented block of a script-check's captured stream to the
/// failure message. A no-op for an empty stream.
///
/// Verbatim: an opaque check's verdict is typically its LAST line (that is what
/// `last-line` matches), while a command's fatal error is typically near its
/// first, so any cap hides whichever of those the reader came for.
fn append_captured_stream(msg: &mut String, label: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(bytes);
    msg.push_str(&format!("    --- {label} ---\n"));
    for line in text.lines() {
        msg.push_str("    ");
        msg.push_str(line);
        msg.push('\n');
    }
}

fn run_dependency_rules(
    project_root: &Path,
    rules: &[DependencyRule],
    commands: bool,
) -> Result<(), DevError> {
    if rules.is_empty() {
        return Ok(());
    }

    // A fixed invocation with no per-project variation - it says strictly less
    // than the `dependency rules: ...` line below it, so it is `--commands`-only
    // like every other cargo line. The phase is otherwise silent until its
    // result, matching the native phases (header/textlint).
    if commands {
        output::run_msg("cargo metadata --format-version 1 --no-deps (dependency rules)");
    }
    let report = dependency_rules::check(project_root, rules)?;

    if report.violations.is_empty() {
        output::run_msg(&format!(
            "dependency rules: ok ({}, {})",
            output::count(report.rules, "rule"),
            output::count(report.packages, "workspace package"),
        ));
        return Ok(());
    }

    let total = report.violations.len();
    let mut msg = format!("dependency rules: {}\n", output::count(total, "violation"));
    for violation in &report.violations {
        msg.push_str("  ");
        msg.push_str(&dependency_rules::format_violation(violation));
        msg.push('\n');
    }
    output::error(msg.trim_end());

    Err(DevError::Build("dependency rules failed".into()))
}

/// The `publish_cycle` phase: refuse a dependency cycle among publishable
/// workspace members.
///
/// Unlike its neighbours this phase needs no config to arm it. There is
/// nothing to declare - a publication cycle is derivable from the
/// manifests alone, is never intentional, and is invisible to every other
/// phase, since `cargo build`, clippy and the test lanes all resolve the
/// workspace at once and are perfectly happy with one. It surfaces at
/// release time instead, which is precisely why it belongs in the cheap
/// always-on tier rather than in a command someone has to remember to run.
///
/// Ignores the CLI `-p` scope deliberately: publication order is a
/// property of the whole workspace, and a cycle that a narrowed run hid
/// would be a cycle that lands on master. `cargo metadata --no-deps` is
/// the one subprocess, and it is cheap.
///
/// The analysis is `deps`' - literally the same `publish_cycle::run` - so
/// `brokkr check` and `brokkr deps` can never disagree about a tree.
fn run_publish_cycle(
    project_root: &Path,
    commands: bool,
) -> Result<(), DevError> {
    if commands {
        output::run_msg("cargo metadata --format-version 1 --no-deps (publish cycle)");
    }
    let cycles = crate::deps::publication_cycles(project_root)?;

    if cycles.is_empty() {
        output::run_msg("publish cycle: ok");
        return Ok(());
    }

    let mut msg = format!(
        "publish cycle: {}\n",
        output::count(cycles.len(), "cargo publication cycle")
    );
    for cycle in &cycles {
        for line in crate::deps::publish_cycle_lines(cycle) {
            msg.push_str("  ");
            msg.push_str(&line);
            msg.push('\n');
        }
    }
    // Never let the list read as the complete inventory: the fix someone
    // picks depends on knowing whether anything else is still entangled.
    // Point at `deps` too - scoping a fix is investigation, and that is
    // the command for it.
    msg.push_str(&format!("  {}\n", crate::deps::PUBLISH_CYCLE_CAVEAT));
    msg.push_str("  `brokkr deps` reports these alongside the rest of the dependency picture\n");
    output::error(msg.trim_end());

    Err(DevError::Build("publish cycle failed".into()))
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
fn clippy_args(sweep: &ResolvedSweep, scope: &[&str], allow: &[String]) -> Vec<String> {
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
    if !scope.is_empty() {
        for pkg in scope {
            args.push("--package".into());
            args.push((*pkg).into());
        }
    } else {
        // Scope to the sweep's packages (`-p <pkg>`) so `--features` is valid
        // in a virtual workspace, where cargo rejects features at the root.
        for pkg in &sweep.packages {
            args.push("-p".into());
            args.push(pkg.clone());
        }
    }
    args.extend(sweep.cargo_feature_args.iter().cloned());
    args.push("--".into());
    args.push("--cap-lints=warn".into());
    for lint in allow {
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
    run_scope: &[&str],
    allow: &[String],
    meta_target_dir: Option<&Path>,
    commands: bool,
) -> Result<SweepResult, DevError> {
    let args = clippy_args(sweep, run_scope, allow);
    run_one_diagnostic_cargo("clippy", project_root, sweep, &args, run_scope, meta_target_dir, commands)
}

/// `cargo doc` argv for one sweep resolution. The selection half is clippy's -
/// profile, unification, packages, features - because rustdoc resolves `cfg`
/// exactly as the build does, so documenting a sweep under any other shape
/// judges doc comments on code that shape never compiles.
///
/// No `--cap-lints`, unlike clippy: that flag exists there to finish the graph
/// past a denied lint, and `--keep-going` already does that for doc. No `-A`
/// either - rustdoc takes rustc flags only through `RUSTDOCFLAGS`, which would
/// change the doc fingerprint - so `[lints] allow` is applied at ingestion
/// instead ([`rustdoc_allowed`]).
fn doc_args(sweep: &ResolvedSweep, scope: &[&str], cfg: &RustdocConfig) -> Vec<String> {
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
    if scope.is_empty() {
        for pkg in &sweep.packages {
            args.push("-p".into());
            args.push(pkg.clone());
        }
    } else {
        for pkg in scope {
            args.push("--package".into());
            args.push((*pkg).into());
        }
    }
    args.extend(sweep.cargo_feature_args.iter().cloned());
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
    run_scope: &[&str],
    meta_target_dir: Option<&Path>,
    commands: bool,
) -> Result<SweepResult, DevError> {
    output::run_msg(&sweep_run_line(phase, sweep, args, false, commands, run_scope));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // Apply the sweep's env to the clippy build too, so a build-affecting
    // var (codegen toggle, etc.) is set consistently across every phase -
    // clippy, the test pre-build, and the test run - not just the tests.
    let mut env_owned: Vec<(String, String)> = sweep
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // A sweep with `rustflags` (or package-mode unification) clippy-checks
    // under the same cfg + isolated target dir as its tests, so the gate's
    // lints match its build. Plain sweeps contribute nothing here, and for
    // those `meta_target_dir` is `None` (computed by the caller).
    if let Some(dir) = meta_target_dir {
        // No lint allows in the env here: clippy passes the same `-A` set
        // on its own argv (`clippy_args`), and a second copy in RUSTFLAGS
        // would only change the build fingerprint away from the test
        // phase's, costing a rebuild for no change in what is suppressed.
        env_owned.extend(sweep_cargo_env(sweep, dir, &[]));
    }
    let env_refs: Vec<(&str, &str)> = env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;
    Ok(SweepResult {
        label: sweep.label.clone(),
        command: format!("cargo {}", args.join(" ")),
        stdout: String::from_utf8_lossy(&captured.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
        success: captured.status.success(),
        selected: if !run_scope.is_empty() {
            Some(run_scope.iter().map(|s| (*s).to_owned()).collect())
        } else if !sweep.packages.is_empty() {
            Some(sweep.packages.clone())
        } else {
            None
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn run_clippy_phase(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    packages: &[String],
    allow: &[String],
    allow_exact: &[SitedAllow],
    commands: bool,
    clippy_ran: &mut [bool],
) -> Result<(), DevError> {
    let multi = sweeps.len() > 1;

    announce_allows(allow, allow_exact);

    let info = build::project_info(Some(project_root))?;
    let results = run_per_build_shape("clippy", &info, sweeps, packages, clippy_ran, |_| None, |sweep, run_scope, dir| {
        run_one_clippy(project_root, sweep, run_scope, allow, dir, commands)
    })?;

    report_stale_sited_allows(&results, allow_exact, packages);

    // With `--cap-lints=warn`, a lint no longer makes cargo exit non-zero, so
    // the pass/fail decision is brokkr's own: every diagnostic is a failure,
    // whatever its (capped) level, except a dependency's warning. A failed run
    // with nothing parseable still fails.
    let members = Some(&info.workspace_members);
    let keep = |d: &cargo_json::DiagnosticEvent| {
        !is_dependency_warning(d, members) && !sited_allowed(d, allow_exact)
    };
    report_diagnostic_phase("clippy", &results, &info, project_root, &keep, multi, commands)
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
    packages: &[String],
    allow: &[String],
    allow_exact: &[SitedAllow],
    commands: bool,
    ran: &mut [bool],
) -> Result<(), DevError> {
    let Some(cfg) = cfg else {
        return Ok(());
    };
    let info = build::project_info(Some(project_root))?;
    // A doc-only sweep is a doctest carrier: it names no compile shape of its
    // own, so documenting it re-reports another sweep's diagnostics under a
    // second label - and may not dedupe, when it inherits from config what the
    // sibling passes on argv.
    let skip = |sweep: &ResolvedSweep| sweep.doc_only.then_some("doctest carrier, no build shape of its own");
    let results = run_per_build_shape("rustdoc", &info, sweeps, packages, ran, skip, |sweep, run_scope, dir| {
        let args = doc_args(sweep, run_scope, cfg);
        run_one_diagnostic_cargo("rustdoc", project_root, sweep, &args, run_scope, dir, commands)
    })?;
    let members = Some(&info.workspace_members);
    let keep = |d: &cargo_json::DiagnosticEvent| {
        !is_dependency_warning(d, members)
            && !rustdoc_allowed(d, allow)
            && !sited_allowed(d, allow_exact)
    };
    let multi = results.len() > 1;
    report_diagnostic_phase("rustdoc", &results, &info, project_root, &keep, multi, commands)
}

/// Whether `[lints] allow` names this diagnostic's lint. Matched on the exact
/// code cargo reports (`rustdoc::broken_intra_doc_links`); a lint group name
/// matches nothing here, since diagnostics carry the member lint's code.
fn rustdoc_allowed(d: &cargo_json::DiagnosticEvent, allow: &[String]) -> bool {
    d.code.as_deref().is_some_and(|c| allow.iter().any(|a| a == c))
}

/// Whether an `allow_exact` entry suppresses this diagnostic.
fn sited_allowed(d: &cargo_json::DiagnosticEvent, allow_exact: &[SitedAllow]) -> bool {
    allow_exact.iter().any(|s| sited_match(s, d))
}

/// Decide and report a diagnostic phase from its cargo runs: any failed run
/// or any diagnostic `keep` admits fails it. Prints the failing commands, then
/// the scoped, cross-sweep summary.
#[allow(clippy::too_many_arguments)]
fn report_diagnostic_phase(
    phase: &str,
    results: &[SweepResult],
    info: &build::ProjectInfo,
    project_root: &Path,
    keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool,
    multi: bool,
    commands: bool,
) -> Result<(), DevError> {
    let run_failed = |r: &SweepResult| {
        !r.success || cargo_json::parse_cargo_diagnostics(&r.stdout).iter().any(keep)
    };
    if !results.iter().any(run_failed) {
        return Ok(());
    }

    // Collapsed form suppressed the command on the way in; a failing sweep is
    // exactly where the copy-pasteable line earns its place.
    if !commands {
        for r in results.iter().filter(|r| run_failed(r)) {
            output::error(&format!("failing command: {}", r.command));
        }
    }

    output::error(&format_clippy_multi(
        &format!("cargo {}", if phase == "rustdoc" { "doc" } else { phase }),
        results,
        Some(info),
        project_root,
        multi,
        keep,
    ));
    Err(DevError::Build(format!("{phase} failed")))
}

/// Run `run_one` once per distinct build shape (and once per package under
/// package-mode unification), marking `ran[i]` for each sweep that got a cargo
/// run. The dedupe, the CLI `-p` intersection and the nothing-ran refusal are
/// shared by every per-shape diagnostic phase, so they cannot drift apart.
/// `skip` names a reason a phase does not apply to a sweep at all; such
/// sweeps do not count toward the nothing-ran refusal.
#[allow(clippy::too_many_arguments)]
fn run_per_build_shape(
    phase: &str,
    info: &build::ProjectInfo,
    sweeps: &[ResolvedSweep],
    packages: &[String],
    ran: &mut [bool],
    skip: impl Fn(&ResolvedSweep) -> Option<&'static str>,
    mut run_one: impl FnMut(&ResolvedSweep, &[&str], Option<&Path>) -> Result<SweepResult, DevError>,
) -> Result<Vec<SweepResult>, DevError> {
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
        .any(ResolvedSweep::needs_isolated_target_dir)
        .then(|| info.target_dir.clone());

    // Diagnostic phases are per-build-shape while tests are per-lane: two lanes
    // sharing a `[[check]]` entry must not be linted or documented twice, so
    // dedupe on the whole build shape.
    let mut seen_shapes: std::collections::HashSet<profile::BuildShapeKey> =
        std::collections::HashSet::new();

    let mut results: Vec<SweepResult> = Vec::with_capacity(sweeps.len());
    let mut applicable = 0usize;
    for (i, sweep) in sweeps.iter().enumerate() {
        if let Some(reason) = skip(sweep) {
            output::run_msg(&format!("{phase} {}: skipped ({reason})", sweep.label));
            continue;
        }
        applicable += 1;
        // The CLI `-p` set intersects with the sweep's selection - it never
        // combines, because cargo unions selection flags (cli_package_scope).
        // Ruled-out packages are dropped with a note; a sweep keeping none
        // is skipped entirely.
        let (scope, dropped) = match cli_package_scope(sweep, packages, false) {
            Ok(s) => s,
            Err(reason) => {
                output::run_msg(&format!("{phase} {}: skipped ({reason})", sweep.label));
                continue;
            }
        };
        for note in &dropped {
            output::run_msg(&format!("{phase} {}: {note} (dropped)", sweep.label));
        }

        if !seen_shapes.insert(sweep.build_shape_key()) {
            output::run_msg(&format!(
                "{phase} {}: deduped (build shape already checked)",
                sweep.label
            ));
            continue;
        }
        // Past the skip and the dedupe: this sweep gets its own cargo run, so
        // the `--json` trailer may honestly list it as checked (S3-33).
        // `i` indexes `sweeps`, and `ran` is sized to match, so direct.
        ran[i] = true;
        // Package mode lints one package per cargo run, for the same reason it
        // tests one per run: a batched multi-`-p` clippy resolves a graph that
        // is not any of the graphs the lane actually builds, so its lint
        // surface belongs to no real compile.
        let owned_scope: Vec<String> = scope.iter().map(|s| (*s).to_owned()).collect();
        for resolution in sweep.resolutions(&owned_scope) {
            let run_scope: Vec<&str> = match &resolution {
                Some(pkg) => vec![pkg.as_str()],
                None => scope.clone(),
            };
            results.push(run_one(sweep, &run_scope, meta_target_dir.as_deref())?);
        }
    }

    // Skipping some sweeps for an out-of-scope `-p` is fine; skipping all of
    // them means nothing was checked, which must not read as clean.
    if results.is_empty() && applicable > 0 {
        let labels: Vec<&str> = sweeps.iter().filter(|s| skip(s).is_none()).map(|s| s.label.as_str()).collect();
        // Two different causes, and the message used to assume the first: with
        // no CLI `-p` at all it interpolated an empty list and read as
        // `-p : every sweep's config rules the selection out`, which named
        // neither what was rejected nor why. A phase that fails must say what
        // it rejected - a bare "check failed" sends the reader to the source.
        return Err(DevError::Config(if packages.is_empty() {
            format!(
                "nothing reached {phase}: every active sweep ({}) produced no \
                 cargo invocation. A sweep reaches this only with an empty \
                 resolution plan, which is a brokkr bug - please report the \
                 `[[check]]` entries involved.",
                labels.join(", ")
            )
        } else {
            format!(
                "-p {}: every sweep's config rules the selection out ({}); \
                 nothing reached {phase}",
                packages.join(" -p "),
                labels.join(", ")
            )
        }));
    }
    Ok(results)
}

/// A suppressed lint narrows what "clippy clean" certifies, so the log must
/// say so up front - like `skip_phases`, a narrowed run must never read as a
/// full one. Blanket allows on one line, each sited allow on its own.
/// Announce the test phase's lint suppressions, and where they were injected.
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
    // so always lands in the env layer); report the shape the plain sweeps get.
    let sink = rustflags::sink(project_root, sweeps.iter().all(|s| !s.rustflags.is_empty()));
    // One clause, not a second line: that a sited entry widens here is a
    // property of the phase, not news about any particular entry, so it does
    // not earn a line of its own - let alone one per entry.
    let widened = if allow_exact.is_empty() {
        ""
    } else {
        "; allow_exact applies build-wide here"
    };
    output::run_msg(&format!(
        "test: allowing {} via {} ([lints]{widened})",
        lints.join(", "),
        sink.describe()
    ));
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
fn announce_allows(allow: &[String], allow_exact: &[SitedAllow]) {
    if !allow.is_empty() {
        output::run_msg(&format!(
            "clippy: allowing {} ([lints] allow)",
            allow.join(", ")
        ));
    }
    if !allow_exact.is_empty() {
        output::run_msg(&format!(
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
/// so "suppressed nothing" there is noise, not evidence of staleness.
fn report_stale_sited_allows(
    results: &[SweepResult],
    allow_exact: &[SitedAllow],
    packages: &[String],
) {
    if allow_exact.is_empty() || !packages.is_empty() {
        return;
    }
    let mut matched = vec![false; allow_exact.len()];
    for r in results {
        for d in cargo_json::parse_cargo_diagnostics(&r.stdout) {
            for (i, s) in allow_exact.iter().enumerate() {
                if sited_match(s, &d) {
                    matched[i] = true;
                }
            }
        }
    }
    for (s, hit) in allow_exact.iter().zip(&matched) {
        if !hit {
            output::run_msg(&format!(
                "clippy: allow_exact {s} suppressed nothing (stale entry?)"
            ));
        }
    }
}

/// Does a `[clippy] allow_exact` entry suppress this diagnostic? Lint names
/// match with or without the `clippy::` qualifier (mirroring what `allow`
/// accepts); the file must equal the entry's path exactly - cargo emits
/// build-root-relative paths, which is also what the entry is written from.
fn sited_match(s: &SitedAllow, d: &cargo_json::DiagnosticEvent) -> bool {
    let Some(code) = &d.code else {
        return false;
    };
    let lint_matches = code == &s.lint
        || code.strip_prefix("clippy::") == Some(s.lint.as_str())
        || s.lint.strip_prefix("clippy::") == Some(code.as_str());
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

    // One sweep -> run_clippy_phase runs `multi = false`, so output carries no
    // sweep-label tags. `packages: &[]` because ad-hoc `-p` is already in
    // sweep.packages (emitted as `-p <pkg>`); the extra `--package` slots stay
    // unused. `commands = true`: this is the *investigative* runner, invoked to
    // find out what a given target shape actually does, so the full cargo line
    // is the point - unlike `brokkr check`, where it is per-run noise.
    // The investigative runner has no `--json` trailer, so the ran-mask is
    // write-only here; a single throwaway slot satisfies the shared signature.
    let mut clippy_ran = [false];
    match run_clippy_phase(
        project_root,
        std::slice::from_ref(&sweep),
        &[],
        clippy_allow,
        clippy_allow_exact,
        true,
        &mut clippy_ran,
    ) {
        Ok(()) => {
            output::result_msg(&format!("clippy clean in {}", fmt_wall(started.elapsed())));
            Ok(())
        }
        // A *rendered* clippy failure: the phase already printed the diagnostics,
        // so add the summary and exit 1 without main echoing a second line.
        Err(DevError::Build(_)) => {
            output::error(&format!("clippy failed in {}", fmt_wall(started.elapsed())));
            Err(DevError::ExitCode(1))
        }
        // Anything else (cargo missing, spawn failure, cooperative interrupt) is
        // NOT an already-rendered lint result - propagate the real cause so main
        // reports it, instead of masking it behind "clippy failed".
        Err(other) => Err(other),
    }
}

/// Construct the single `ResolvedSweep` a `brokkr clippy` invocation runs.
///
/// `--sweep NAME` borrows the named `[[check]]` entry wholesale (packages,
/// features, env). Otherwise it is ad-hoc: packages from `-p`, features from the
/// flags, env unioned from every `[[check]]` entry. `--env` overrides win over
/// both and are applied last; keys an override sets are exempt from cross-sweep
/// conflict detection, so `--env` can *resolve* a conflict rather than be masked
/// by it.
fn build_clippy_sweep(
    check_entries: &[CheckEntry],
    packages: &[String],
    all_features: bool,
    features: &[String],
    no_default_features: bool,
    sweep_name: Option<&str>,
    env_overrides: &[(String, String)],
) -> Result<ResolvedSweep, DevError> {
    let overridden: std::collections::BTreeSet<&str> =
        env_overrides.iter().map(|(k, _)| k.as_str()).collect();

    let mut sweep = if let Some(name) = sweep_name {
        let entry = check_entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| {
                DevError::Config(format!(
                    "--sweep '{name}' matches no [[check]] entry in brokkr.toml"
                ))
            })?;
        // A single entry's env is unambiguous - no cross-sweep merge needed.
        profile::sweep_from_check_entry(entry)
    } else {
        let cargo_feature_args = if all_features {
            vec!["--all-features".into()]
        } else {
            let mut a = Vec::new();
            if no_default_features {
                a.push("--no-default-features".into());
            }
            if !features.is_empty() {
                a.push("--features".into());
                a.push(features.join(","));
            }
            a
        };
        ResolvedSweep {
            label: "clippy".into(),
            cargo_feature_args,
            packages: packages.to_vec(),
            env: merge_check_envs(check_entries, &overridden, CLIPPY_ENV_CONFLICT_REMEDY)?,
            ..Default::default()
        }
    };

    // `--env` overrides win last, over both the merged check env and a borrowed
    // entry's env.
    for (k, v) in env_overrides {
        sweep.env.insert(k.clone(), v.clone());
    }
    Ok(sweep)
}

/// Union the `env` of every `[[check]]` entry, erroring on a key two entries set
/// to *different* values - **unless** that key is in `overridden`, where an
/// explicit `--env` will set it anyway and the cross-sweep disagreement is moot.
///
/// Union (not intersection) is deliberate: a build-affecting invariant present
/// in most-but-not-all entries must not be silently dropped - that is the exact
/// failure BUG_SWEEP set out to prevent. The cost is that entries setting
/// *disjoint* keys contribute all of them; the escape is `--env KEY=...` (or
/// `--sweep NAME` to replay one entry exactly).
///
/// Every entry `env` is carried, load-bearing or not: telling the two apart
/// would mean knowing what each `build.rs` reads. So this guarantees the same
/// *env* on both paths, and nothing about what that env achieves - an
/// invariant a project expresses as a cargo feature follows feature
/// resolution, and no env union reaches it.
///
/// Shared by `brokkr clippy`'s ad-hoc path and `brokkr check`'s, so the two
/// commands resolve the same env from the same config - they used to disagree,
/// `clippy --features x` unioning the entries' env while `check --features x`
/// silently ran without it. `remedy` carries the escape sentence, since the
/// two commands offer different flags for it.
fn merge_check_envs(
    entries: &[CheckEntry],
    overridden: &std::collections::BTreeSet<&str>,
    remedy: &str,
) -> Result<std::collections::BTreeMap<String, String>, DevError> {
    let mut merged: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for e in entries {
        for (k, v) in &e.env {
            if overridden.contains(k.as_str()) {
                continue;
            }
            match merged.get(k) {
                Some(existing) if existing != v => {
                    return Err(DevError::Config(format!(
                        "[[check]] env conflict on `{k}`: '{existing}' vs '{v}' across \
                         sweeps - {remedy}"
                    )));
                }
                _ => {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Ok(merged)
}

struct SweepResult {
    label: String,
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
}

impl SweepResult {
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
    sweep_tag(reported_by, results.len())
}

/// Multi-sweep version of the text formatter: parses each sweep's stdout
/// JSON, merges + dedups diagnostics, orders them by scope, and when `multi`
/// tags a line with the sweeps that reported it - only where some sweep
/// covering its package did not ([`coverage_tag`]). Falls back to per-sweep
/// raw streams when cargo failed but emitted no compiler-message events
/// (e.g. cargo itself crashed before reaching the diagnostic phase).
#[allow(clippy::too_many_arguments)]
fn format_clippy_multi(
    tool: &str,
    results: &[SweepResult],
    info: Option<&build::ProjectInfo>,
    project_root: &Path,
    multi: bool,
    keep: &dyn Fn(&cargo_json::DiagnosticEvent) -> bool,
) -> String {
    // Each diagnostic's package, keyed as the merge keys it: identical
    // diagnostics from two runs come from the same source line, so the same
    // package.
    let mut package_of: HashMap<DiagKey, String> = HashMap::new();
    let parses: Vec<(String, cargo_filter::ClippyParse)> = results
        .iter()
        .map(|r| {
            let mut events = cargo_json::parse_cargo_diagnostics(&r.stdout);
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

    // Any sweep with parse_failed: fall back to raw aggregated streams.
    if parses.iter().any(|(_, p)| p.parse_failed) {
        let mut out = String::new();
        for r in results {
            if multi {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{}]\n", r.label));
            }
            out.push_str(&r.stderr);
            out.push_str(&r.stdout);
        }
        return out;
    }

    let merged = merge_clippy(&parses);

    if merged.is_empty() {
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
        format!("{tool}: {total} {errors} ({} sweeps)\n", results.len())
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
            )
        {
            out.push_str(&tag);
            out.push(' ');
        }
        out.push_str(&m.diag.format_one());
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Parse cargo's `--message-format=json` stdout into a
/// [`ClippyParse`](cargo_filter::ClippyParse).
///
/// Walks each compiler-message JSON event and maps it to the formatter
/// primitive used by `merge_clippy` and `format_one()`, in discovery order.
/// When cargo failed and emitted no compiler-message events, sets
/// `parse_failed` so callers can fall back to dumping the raw streams.
#[cfg(test)]
fn parse_clippy_from_json(
    stdout: &str,
    sweep_failed: bool,
    allow_exact: &[SitedAllow],
) -> cargo_filter::ClippyParse {
    clippy_parse_from_events(&gated_diags(stdout, allow_exact), sweep_failed)
}

/// Map already-filtered diagnostic events to the formatter primitive, in
/// discovery order. `parse_failed` is set when cargo failed and left no event,
/// so callers can fall back to dumping the raw streams.
fn clippy_parse_from_events(
    events: &[cargo_json::DiagnosticEvent],
    sweep_failed: bool,
) -> cargo_filter::ClippyParse {
    let diagnostics: Vec<cargo_filter::ClippyDiagnostic> =
        events.iter().map(event_to_clippy).collect();
    let parse_failed = sweep_failed && diagnostics.is_empty();
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
/// unknown, is treated as the workspace's own.
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

/// Iterate `sweeps`, pre-building each sweep's `build_packages` and
/// then running `cargo test` for it. Fails fast on the first sweep
/// that fails (build or test), mirroring how the clippy phase
/// short-circuits on a non-zero status.
#[allow(clippy::too_many_arguments)]
fn run_test_phase(
    project: Option<Project>,
    project_root: &Path,
    state_root: &Path,
    sweeps: &[ResolvedSweep],
    packages: &[String],
    doctests: bool,
    commands: bool,
    extra_args: &[String],
    allow: &[String],
    allow_exact: &[SitedAllow],
    mut timings: Option<&mut Vec<TestTiming>>,
    executed: &mut [bool],
) -> Result<(), DevError> {
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

    let mut ran_any = false;
    for (i, sweep) in sweeps.iter().enumerate() {
        // The CLI `-p` set intersects with the sweep's selection: ruled-out
        // packages are dropped with a note, and the sweep is skipped only
        // when nothing survives (cli_package_scope).
        let (scope, dropped) = match cli_package_scope(sweep, packages, true) {
            Ok(s) => s,
            Err(reason) => {
                output::run_msg(&format!("test {}: skipped ({reason})", sweep.label));
                continue;
            }
        };
        for note in &dropped {
            output::run_msg(&format!("test {}: {note} (dropped)", sweep.label));
        }
        ran_any = true;
        // Record before the run: the sweep's tests execute below (pass or
        // fail), so the coverage audit may credit its ran-set. A sweep the
        // loop never reaches (an earlier one failed fast) stays false.
        executed[i] = true;

        // Per-sweep: a sweep carrying `rustflags` runs in its own isolated
        // target dir with a matching BROKKR_TEST_BIN_DIR + RUSTFLAGS, so a
        // global cfg (e.g. `--cfg madsim`) never thrashes the plain sweeps.
        // A lint allow reaches this build through exactly one layer, and which
        // one is per sweep: a sweep carrying `rustflags` exports an env var, so
        // the env is live for it whatever the config chain says. Whichever it
        // is, it must reach the pre-build and the test run alike - a pre-build
        // compiling the same crate under the unsuppressed lint fails before
        // `cargo test` is ever reached.
        let (env_allows, allow_args) =
            rustflags::plumbing(project_root, !sweep.rustflags.is_empty(), &allow_flags);
        // Dev unless the sweep pinned a profile: BROKKR_TEST_BIN_DIR must
        // name the directory this sweep's own pre-build wrote into.
        let profile_dir = sweep
            .profile
            .map_or("debug", crate::config::SweepProfile::target_subdir);
        let project_env = sweep_runtime_env(sweep, project, &target_dir, profile_dir, env_allows);
        for pkg in &sweep.build_packages {
            run_sweep_pre_build(
                project_root,
                sweep,
                pkg,
                &project_env,
                &allow_args,
                commands,
            )?;
        }

        reject_conflicting_lanes(sweep)?;

        let success = if sweep.harness == crate::config::Harness::Nextest {
            run_nextest_sweep(
                project_root, state_root, sweep, &scope, extra_args, &project_env, &allow_args,
                commands,
            )?
        } else if let Some(budget) = sweep.parallel_budget {
            run_parallel_sweep(
                project_root,
                state_root,
                sweep,
                &scope,
                budget,
                extra_args,
                &project_env,
                &allow_args,
                doctests,
                commands,
                timings.as_deref_mut(),
            )?
        } else if sweep.process_isolation {
            run_isolated_sweep(
                project_root,
                state_root,
                sweep,
                &scope,
                extra_args,
                &project_env,
                &allow_args,
                doctests,
                commands,
                timings.as_deref_mut(),
            )?
        } else {
            run_sequential_resolutions(
                project_root,
                state_root,
                sweep,
                &scope,
                extra_args,
                &project_env,
                &allow_args,
                (doctests, multi, commands),
                timings.as_deref_mut(),
            )?
        };
        if !success {
            return Err(DevError::Build("tests failed".into()));
        }
    }

    // Skipping some sweeps for an out-of-scope `-p` is fine; skipping all of
    // them means zero tests ran, which must not read as green.
    if !ran_any && !sweeps.is_empty() {
        return Err(DevError::Config(format!(
            "-p {}: every sweep's config rules the selection out; zero tests ran",
            packages.join(" -p ")
        )));
    }

    Ok(())
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
mod scope_order_tests {
    use super::*;
    use std::path::PathBuf;

    /// Every error is displayed, even where git cannot be asked and so
    /// nothing can be moved to the front.
    #[test]
    fn everything_is_displayed_without_git() {
        let violations = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        let displayed = scope_order(violations, Path::new("/nonexistent"), PathBuf::as_path);
        assert_eq!(displayed.len(), 2);
    }
}

#[cfg(test)]
mod clippy_sweep_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::collections::BTreeSet;

    fn entry(name: &str, env: &[(&str, &str)]) -> CheckEntry {
        CheckEntry {
            name: name.into(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Default::default()
        }
    }

    fn overrides(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // ----- merge_check_envs -----

    #[test]
    fn merge_unions_disjoint_keys() {
        let entries = [entry("a", &[("FOO", "1")]), entry("b", &[("BAR", "2")])];
        let merged = merge_check_envs(&entries, &BTreeSet::new(), CLIPPY_ENV_CONFLICT_REMEDY).unwrap();
        assert_eq!(merged.get("FOO").map(String::as_str), Some("1"));
        assert_eq!(merged.get("BAR").map(String::as_str), Some("2"));
    }

    #[test]
    fn merge_agreeing_duplicate_is_fine() {
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "1")])];
        let merged = merge_check_envs(&entries, &BTreeSet::new(), CLIPPY_ENV_CONFLICT_REMEDY).unwrap();
        assert_eq!(merged.get("HP").map(String::as_str), Some("1"));
    }

    #[test]
    fn merge_conflicting_duplicate_errors_naming_key() {
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let err = merge_check_envs(&entries, &BTreeSet::new(), CLIPPY_ENV_CONFLICT_REMEDY).unwrap_err();
        assert!(err.to_string().contains("HP"), "got: {err}");
    }

    #[test]
    fn merge_conflict_is_exempt_when_overridden() {
        // The key both entries disagree on is in `overridden`, so no error - the
        // --env override will set it. (The P1-1 fix: --env resolves a conflict.)
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let overridden: BTreeSet<&str> = ["HP"].into_iter().collect();
        let merged = merge_check_envs(&entries, &overridden, CLIPPY_ENV_CONFLICT_REMEDY).unwrap();
        // The overridden key is skipped entirely here; build_clippy_sweep layers
        // the override on afterwards.
        assert!(!merged.contains_key("HP"));
    }

    // ----- build_clippy_sweep: ad-hoc -----

    #[test]
    fn adhoc_all_features() {
        let s = build_clippy_sweep(&[], &[], true, &[], false, None, &[]).unwrap();
        assert_eq!(s.cargo_feature_args, vec!["--all-features"]);
        assert_eq!(s.label, "clippy");
    }

    #[test]
    fn adhoc_no_default_plus_features() {
        let feats = vec!["x".to_owned(), "y".to_owned()];
        let s = build_clippy_sweep(&[], &[], false, &feats, true, None, &[]).unwrap();
        assert_eq!(
            s.cargo_feature_args,
            vec!["--no-default-features", "--features", "x,y"]
        );
    }

    #[test]
    fn adhoc_packages_copied() {
        let pkgs = vec!["crate_a".to_owned(), "crate_b".to_owned()];
        let s = build_clippy_sweep(&[], &pkgs, false, &[], false, None, &[]).unwrap();
        assert_eq!(s.packages, vec!["crate_a", "crate_b"]);
    }

    #[test]
    fn adhoc_env_override_resolves_conflict() {
        // Two entries disagree on HP; --env HP=chosen must make the whole thing
        // succeed AND land the chosen value on the sweep. This is the end-to-end
        // P1-1 regression: conflict no longer errors before the override applies.
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let ov = overrides(&[("HP", "chosen")]);
        let s = build_clippy_sweep(&entries, &[], false, &[], false, None, &ov).unwrap();
        assert_eq!(s.env.get("HP").map(String::as_str), Some("chosen"));
    }

    #[test]
    fn adhoc_env_override_wins_over_agreeing_value() {
        let entries = [entry("a", &[("HP", "1")])];
        let ov = overrides(&[("HP", "9")]);
        let s = build_clippy_sweep(&entries, &[], false, &[], false, None, &ov).unwrap();
        assert_eq!(s.env.get("HP").map(String::as_str), Some("9"));
    }

    // ----- build_clippy_sweep: --sweep -----

    #[test]
    fn sweep_unknown_name_errors() {
        let entries = [entry("known", &[])];
        let err =
            build_clippy_sweep(&entries, &[], false, &[], false, Some("missing"), &[]).unwrap_err();
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn sweep_copies_entry_env_and_features() {
        let mut e = entry("ffi", &[("HP", "1")]);
        e.features = vec!["ffi".into()];
        e.packages = vec!["nautilus-model".into()];
        let entries = [e];
        let s = build_clippy_sweep(&entries, &[], false, &[], false, Some("ffi"), &[]).unwrap();
        assert_eq!(s.cargo_feature_args, vec!["--features", "ffi"]);
        assert_eq!(s.packages, vec!["nautilus-model"]);
        assert_eq!(s.env.get("HP").map(String::as_str), Some("1"));
    }

    #[test]
    fn sweep_env_override_wins_over_entry() {
        let entries = [entry("ffi", &[("HP", "1")])];
        let ov = overrides(&[("HP", "0")]);
        let s = build_clippy_sweep(&entries, &[], false, &[], false, Some("ffi"), &ov).unwrap();
        assert_eq!(s.env.get("HP").map(String::as_str), Some("0"));
    }
}


#[cfg(test)]
mod ran_labels_tests {
    #![allow(clippy::unwrap_used)]

    use super::ran_sweep_labels;
    use crate::profile::ResolvedSweep;

    fn sweep(label: &str) -> ResolvedSweep {
        ResolvedSweep {
            label: label.into(),
            ..Default::default()
        }
    }

    #[test]
    fn omits_sweeps_skipped_in_both_phases() {
        // S3-33: profile `tier1` selects [default, ffi, live]; a `-p` scope
        // rules ffi/live out of both clippy and the test phase, so only
        // `default` ran. The trailer must not list ffi/live as green.
        let sweeps = [sweep("default"), sweep("ffi"), sweep("live")];
        let clippy_ran = [true, false, false];
        let executed = [true, false, false];
        assert_eq!(
            ran_sweep_labels(&sweeps, &clippy_ran, &executed),
            vec!["default"]
        );
    }

    #[test]
    fn unions_clippy_only_and_test_only_lanes() {
        // A lane can run in one phase but not the other: `a` clippy-checked
        // with its tests skipped, `b` deduped in clippy but its tests still
        // ran, `c` reached by neither. The honest set is the union of the two.
        let sweeps = [sweep("a"), sweep("b"), sweep("c")];
        let clippy_ran = [true, false, false];
        let executed = [false, true, false];
        assert_eq!(
            ran_sweep_labels(&sweeps, &clippy_ran, &executed),
            vec!["a", "b"]
        );
    }

    #[test]
    fn all_green_reports_every_sweep_in_order() {
        let sweeps = [sweep("default"), sweep("ffi")];
        let all_true = [true, true];
        assert_eq!(
            ran_sweep_labels(&sweeps, &all_true, &all_true),
            vec!["default", "ffi"]
        );
    }

    #[test]
    fn nothing_ran_reports_empty() {
        // A failure in an early convention phase (e.g. gremlins) returns
        // before any sweep runs, so the trailer honestly lists none.
        let sweeps = [sweep("default"), sweep("ffi")];
        let none = [false, false];
        assert!(ran_sweep_labels(&sweeps, &none, &none).is_empty());
    }
}

#[cfg(test)]
mod json_summary_tests {
    #![allow(clippy::unwrap_used)]

    use super::{CheckSummary, CoverageStats};

    #[test]
    fn summary_carries_schema_and_null_certifies() {
        let s = CheckSummary {
            schema: 1,
            certifies: None,
            verdict: "passed",
            profile: Some("tier1"),
            sweeps: vec!["default", "ffi"],
            package: None,
            failed_phase: None,
            coverage: None,
            elapsed_ms: 1234,
        };
        let line = serde_json::to_string(&s).unwrap();
        // The two contract-critical fields: the version consumers key on,
        // and `certifies` present-but-null until certification exists.
        assert!(line.contains("\"schema\":1"), "{line}");
        assert!(line.contains("\"certifies\":null"), "{line}");
        assert!(line.contains("\"verdict\":\"passed\""), "{line}");
        assert!(line.contains("\"sweeps\":[\"default\",\"ffi\"]"), "{line}");
    }

    #[test]
    fn failed_summary_names_the_phase() {
        let s = CheckSummary {
            schema: 1,
            certifies: None,
            verdict: "failed",
            profile: None,
            sweeps: vec!["all-features"],
            package: Some("nautilus-betfair"),
            failed_phase: Some("clippy"),
            coverage: Some(CoverageStats {
                pairs: 100,
                run: 90,
                quarantined: 8,
                ignored: 2,
                curated: 0,
                orphaned: 0,
                dead_filters: 0,
            }),
            elapsed_ms: 10,
        };
        let line = serde_json::to_string(&s).unwrap();
        assert!(line.contains("\"verdict\":\"failed\""), "{line}");
        assert!(line.contains("\"failed_phase\":\"clippy\""), "{line}");
        assert!(line.contains("\"profile\":null"), "{line}");
        // The `-p` scope must be visible to consumers - a green that
        // covered one package may not be mistaken for a workspace green.
        assert!(line.contains("\"package\":\"nautilus-betfair\""), "{line}");
    }
}

#[cfg(test)]
mod script_failure_render_tests {
    use super::append_script_failure;
    use crate::config::{Diagnostics, MatchMode, ScriptCheck, Stage, Stream};
    use crate::script_check::Outcome;

    fn check(diagnostics: Diagnostics) -> ScriptCheck {
        ScriptCheck {
            name: "rustdoc-links".into(),
            command: "cargo doc --no-deps --workspace".into(),
            expect: "Generated".into(),
            match_mode: MatchMode::Contains,
            stream: Stream::Both,
            stage: Stage::PreTest,
            diagnostics,
        }
    }

    fn outcome(stdout: &str, stderr: &str) -> Outcome {
        Outcome {
            passed: false,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// The shape of the reported case: a few errors buried in warnings.
    fn noisy() -> String {
        let mut s = String::new();
        for i in 0..40 {
            s.push_str(&format!(
                "warning: public documentation for `T{i}` links to private item `P{i}`\n  --> crates/a/src/lib.rs:{i}:5\n   = note: this link resolves only for private docs\n"
            ));
        }
        s.push_str("error: unused imports: `Mutex` and `cell::RefCell`\n  --> crates/daemon/src/worker_handle/binary.rs:10:5\n");
        s.push_str("error[E0432]: unresolved import `crate::nope`\n  --> crates/a/src/lib.rs:1:5\n");
        s
    }

    #[test]
    fn rustc_shows_errors_and_counts_the_hidden_warnings() {
        let mut msg = String::new();
        append_script_failure(&mut msg, &check(Diagnostics::Rustc), &outcome("", &noisy()));
        // Both errors, with their continuation lines.
        assert!(msg.contains("error: unused imports"));
        assert!(msg.contains("binary.rs:10:5"));
        assert!(msg.contains("error[E0432]"));
        // No warning body survives - that is the whole point.
        assert!(!msg.contains("links to private item"));
        assert!(msg.contains("40 warnings not shown"));
        // Only the failing stream gets a section; the empty one is skipped.
        assert!(msg.contains("--- stderr (errors) ---"));
        assert!(!msg.contains("stdout"));
    }

    /// Every error block prints, however many there are, and across both
    /// streams: the narrowing is by level, never by count.
    #[test]
    fn every_error_block_prints_across_both_streams() {
        let mut msg = String::new();
        append_script_failure(
            &mut msg,
            &check(Diagnostics::Rustc),
            &outcome("error: from stdout\n", "error: from stderr\n"),
        );
        assert!(msg.contains("from stdout"));
        assert!(msg.contains("from stderr"));
        assert!(!msg.contains("not shown"));
    }

    #[test]
    fn rustc_without_an_error_block_falls_back_to_the_verbatim_view() {
        // A sentinel that never appeared because the command died early: the
        // evidence is the output, not a level that isn't there.
        let mut msg = String::new();
        append_script_failure(
            &mut msg,
            &check(Diagnostics::Rustc),
            &outcome("", "warning: something\nthe tool was killed\n"),
        );
        assert!(msg.contains("--- stderr ---"));
        assert!(msg.contains("the tool was killed"));
        assert!(msg.contains("warning: something"));
    }

    /// An opaque check's captured stream prints whole - the fatal error near
    /// the first line and the verdict on the last are both the evidence.
    #[test]
    fn opaque_prints_the_whole_stream() {
        let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut msg = String::new();
        append_script_failure(&mut msg, &check(Diagnostics::Opaque), &outcome(&body, ""));
        assert!(msg.contains("line 0"));
        assert!(msg.contains("line 50"));
        assert!(msg.contains("line 99"));
        assert!(!msg.contains("hidden"));
    }

    #[test]
    fn opaque_keeps_a_short_stream_intact() {
        let mut msg = String::new();
        append_script_failure(&mut msg, &check(Diagnostics::Opaque), &outcome("a\nb\nc\n", ""));
        assert!(!msg.contains("hidden"));
        for line in ["a", "b", "c"] {
            assert!(msg.contains(line));
        }
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
        // real run but not the audit, so the skipped pairs would land in the
        // audit's ran-set. Rejected before anything compiles.
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
