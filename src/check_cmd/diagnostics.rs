fn clippy_args(sweep: &PlannedSweep, run: &ResolutionRun, lints: &LintsConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "clippy".into(),
        "--keep-going".into(),
        // `--all-targets` on every gate sweep; `--lib` only under the
        // investigative `brokkr clippy --lib`, which exists to reproduce the
        // lint surface of a plain lib build. See `PlannedSweep::lib_only`.
        if sweep.lib_only {
            "--lib".into()
        } else {
            "--all-targets".into()
        },
        "--message-format=json".into(),
    ];
    // The sweep's build shape through the one selection renderer: its
    // profile (`cfg(debug_assertions)` decides which code exists, so linting a
    // release sweep in dev checks source the release build never compiles),
    // its unification pin (package and workspace resolution enable different
    // features, so they present different lint surfaces), and the run's
    // projected `-p`/feature request (`-p` scoping is also what makes
    // `--features` valid in a virtual workspace). No lane target filters:
    // clippy lints the shape, not one test lane.
    args.extend(cargo_selection_args(
        sweep,
        run,
        sweep.cargo_profile(),
        CargoSelectionPurpose::Shape,
    ));
    args.push("--".into());
    args.push("--cap-lints=warn".into());
    // Cargo's own lints are allowed at ingestion instead (`code_allowed`):
    // rustc knows no `cargo` lint tool.
    for lint in lints.allow.iter().filter(|l| !crate::config::check::is_cargo_lint(l)) {
        args.push("-A".into());
        args.push(lint.as_str().to_owned());
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
    sweep: &PlannedSweep,
    run: &ResolutionRun,
    lints: &LintsConfig,
    meta_target_dir: Option<&Path>,
    reporter: Reporter,
) -> Result<SweepResult, DevError> {
    let args = clippy_args(sweep, run, lints);
    let result = run_one_diagnostic_cargo(Phase::Clippy, project_root, sweep, &args, run, meta_target_dir, reporter);
    // No SweepResult reaches the normal reporter on a spawn error, interrupt
    // or captured-run deadline. Preserve the investigative command there too.
    if result.is_err() {
        reporter.failed_invocation(
            Phase::Clippy,
            &sweep.label,
            &describe_sweep(sweep, false, &run.selection, run.feature_args()),
            &format!("cargo {}", args.join(" ")),
        );
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
fn doc_args(sweep: &PlannedSweep, run: &ResolutionRun, cfg: &RustdocConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "doc".into(),
        "--no-deps".into(),
        "--keep-going".into(),
        "--message-format=json".into(),
    ];
    if cfg.document_private_items {
        args.push("--document-private-items".into());
    }
    args.extend(cargo_selection_args(
        sweep,
        run,
        sweep.cargo_profile(),
        CargoSelectionPurpose::Shape,
    ));
    args
}

/// One cargo run whose stdout is a `--message-format=json` diagnostic stream:
/// the sweep's env, and its isolated target dir when it has one, so the run
/// reuses the artifacts its test phase builds rather than rebuilding beside
/// them. `phase` names the run in the log line.
fn run_one_diagnostic_cargo(
    phase: Phase,
    project_root: &Path,
    sweep: &PlannedSweep,
    args: &[String],
    run: &ResolutionRun,
    meta_target_dir: Option<&Path>,
    reporter: Reporter,
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
    let doc_check = phase == Phase::Rustdoc && crate::rustdoc_check::supported(project_root, &env_owned);
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
    reporter.diagnostic_invocation(&line_shape, &command);

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_refs: Vec<(&str, &str)> = env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let started = std::time::SystemTime::now();
    let captured = output::run_cargo(&arg_refs, project_root, &env_refs, Some(phase))?;
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
    let diagnostics = cargo_json::parse_cargo_diagnostics(&stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr).into_owned();
    let suppressed_manifest_blocks =
        cargo_json::manifest_lints_by_block(&rustc_text::rustc_blocks(&stderr))
            .into_iter()
            .filter_map(|(block, diagnostic)| diagnostic.is_warning().then_some(block))
            .collect();
    Ok(SweepResult {
        label: sweep.label.clone(),
        shape,
        command,
        stdout,
        stderr,
        success: captured.success(),
        selected: selection.packages().map(<[String]>::to_vec),
        diagnostics,
        suppressed_manifest_blocks,
        listed_manifest_errors: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_clippy_phase(
    project_root: &Path,
    sweeps: &[PlannedSweep],
    selection: &PhaseSelection,
    lints: &LintsConfig,
    reporter: Reporter,
    reach: &mut dyn FnMut(usize),
    // The caller's word that this run lints less than the gate's whole
    // surface, beyond what a CLI `-p` already says (`brokkr clippy`'s probe
    // shapes). Only silences the stale-allow report.
    narrowed: bool,
    serial_builds: bool,
) -> Result<(), DevError> {
    let (allow, allow_exact) = (lints.allow.as_slice(), lints.allow_exact.as_slice());
    // Inside `check` the verdict line names every suppression once, for all
    // the phases it narrows; `brokkr clippy` (and `--commands`, which asks
    // for the long form) prints them here.
    let exact_summary = (!allow_exact.is_empty()).then(|| summarize_sited(allow_exact));
    reporter.allow_notice(allow, exact_summary.as_deref());

    // A phase that attempts nothing says so before asking cargo anything: a
    // metadata failure must not replace a refusal already decided.
    selection.check_refusal()?;
    let info = build::project_info(Some(project_root))?;
    let mut ran = run_per_build_shape(Phase::Clippy, reporter, &info, sweeps, selection, reach, serial_builds, |sweep, run, dir| {
        run_one_clippy(project_root, sweep, run, lints, dir, reporter)
    })?;
    let active_sweep_count = ran.active_sweep_count;
    let mut results = std::mem::take(&mut ran.results);
    let owners = OwnerIndex::from_metadata_at(
        &info.workspace_root,
        project_root,
        &info.workspace_members,
        &info.member_manifests,
    );
    for r in &mut results {
        for diagnostic in &mut r.diagnostics {
            diagnostic.classify_owner(&owners);
        }
        for (block, diagnostic) in attribute_manifest_lints(r, &info, &owners) {
            if diagnostic.is_error() {
                r.listed_manifest_errors.push((block, diagnostic.clone()));
            }
            r.diagnostics.push(diagnostic);
        }
    }

    report_stale_sited_allows(
        Phase::Clippy,
        &results,
        allow_exact,
        narrowed || !selection.overrides().is_empty(),
    );

    // With `--cap-lints=warn`, a lint no longer makes cargo exit non-zero, so
    // the pass/fail decision is brokkr's own: every diagnostic is a failure,
    // whatever its (capped) level, except a dependency's warning. A failed run
    // with nothing parseable still fails.
    // Cargo's manifest lints never saw clippy's `-A` flags, so `[lints] allow`
    // reaches them here.
    let keep = |d: &Diagnostic| {
        !d.is_dependency_warning()
            && !sited_allowed(d, allow_exact)
            && !(diagnostic_lint(d).is_some_and(crate::config::check::is_cargo_lint)
                && code_allowed(d, allow))
    };
    let outcome = ran.verdict(|failed| {
            report_diagnostic_phase(
                Phase::Clippy,
                reporter,
                &results,
                &info,
                project_root,
                &keep,
                active_sweep_count,
                failed,
            )
        });
    if outcome.is_ok() {
        for r in &results {
            reporter.command_after_success(&r.command);
        }
    }
    outcome
}

/// Cargo's manifest lints from a clippy run's stderr, classified by the same
/// metadata index as compiler diagnostics. Member manifests follow the run's
/// selection; the workspace root and path dependencies remain distinct owners
/// without synthetic package ids.
fn attribute_manifest_lints(
    r: &SweepResult,
    info: &build::ProjectInfo,
    owners: &OwnerIndex,
) -> Vec<(usize, Diagnostic)> {
    let blocks = rustc_text::rustc_blocks(&r.stderr);
    cargo_json::manifest_lints_by_block(&blocks)
        .into_iter()
        .filter_map(|(block, mut diagnostic)| {
            diagnostic.classify_owner(owners);
            match &diagnostic.owner {
                Owner::Member(id) => {
                    if !r.selects(id, info) {
                        return None;
                    }
                    diagnostic.package_id = Some(id.clone());
                }
                Owner::WorkspaceRoot | Owner::NonMember(_) | Owner::Unknown => {}
            }
            Some((block, diagnostic))
        })
        .collect()
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
    sweeps: &[PlannedSweep],
    selection: &PhaseSelection,
    lints: &LintsConfig,
    reporter: Reporter,
    reach: &mut dyn FnMut(usize),
    serial_builds: bool,
) -> Result<(), DevError> {
    let Some(cfg) = cfg else {
        return Ok(());
    };
    let (allow, allow_exact) = (lints.allow.as_slice(), lints.allow_exact.as_slice());
    // As in clippy: the decided refusal before any cargo query.
    selection.check_refusal()?;
    let info = build::project_info(Some(project_root))?;
    // A doc-only sweep is a doctest carrier: it names no compile shape of its
    // own, so documenting it re-reports another sweep's diagnostics under a
    // second label - and may not dedupe, when it inherits from config what the
    // sibling passes on argv. The selection marks it not applicable.
    let mut ran = run_per_build_shape(Phase::Rustdoc, reporter, &info, sweeps, selection, reach, serial_builds, |sweep, run, dir| {
        let args = doc_args(sweep, run, cfg);
        run_one_diagnostic_cargo(Phase::Rustdoc, project_root, sweep, &args, run, dir, reporter)
    })?;
    let active_sweep_count = ran.active_sweep_count;
    let mut results = std::mem::take(&mut ran.results);
    // The `rustdoc::` half of the sited list is judged here, where those lints
    // can actually appear; clippy judges the rest.
    report_stale_sited_allows(Phase::Rustdoc, &results, allow_exact, !selection.overrides().is_empty());
    let owners = OwnerIndex::from_metadata_at(
        &info.workspace_root,
        project_root,
        &info.workspace_members,
        &info.member_manifests,
    );
    for result in &mut results {
        for diagnostic in &mut result.diagnostics {
            diagnostic.classify_owner(&owners);
        }
    }
    let keep = |d: &Diagnostic| {
        !d.is_dependency_warning()
            && !code_allowed(d, allow)
            && !sited_allowed(d, allow_exact)
    };
    ran.verdict(|failed| {
        report_diagnostic_phase(
            Phase::Rustdoc,
            reporter,
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
fn code_allowed(d: &Diagnostic, allow: &[crate::config::check::LintName]) -> bool {
    diagnostic_lint(d).is_some_and(|code| allow.iter().any(|allow| allow.matches(code)))
}

/// A diagnostic's lint identity, parsed when cargo's event enters `Diagnostic`.
fn diagnostic_lint(d: &Diagnostic) -> Option<&crate::config::check::LintName> {
    d.code.as_ref()
}

/// Whether an `allow_exact` entry suppresses this diagnostic.
fn sited_allowed(d: &Diagnostic, allow_exact: &[SitedAllow]) -> bool {
    allow_exact.iter().any(|s| sited_match(s, d))
}

/// `phase sweep-label`, or just `phase` when the sweep is labelled after it:
/// `brokkr clippy`'s ad-hoc sweep is labelled "clippy", and `clippy clippy:`
/// says the one fact twice.
fn phase_sweep_tag(phase: Phase, label: &str) -> String {
    if label == phase.as_str() {
        phase.as_str().to_owned()
    } else {
        format!("{} {label}", phase.as_str())
    }
}

/// Decide and report a diagnostic phase from its cargo runs: any failed run
/// or any diagnostic `keep` admits fails it. Prints the failing commands, then
/// the scoped, cross-sweep summary. `already_failed`: a run the phase could
/// not make failed it before these results were read, so a clean set prints
/// no green line.
#[allow(clippy::too_many_arguments)]
fn report_diagnostic_phase(
    phase: Phase,
    reporter: Reporter,
    results: &[SweepResult],
    info: &build::ProjectInfo,
    project_root: &Path,
    keep: &dyn Fn(&Diagnostic) -> bool,
    active_sweep_count: usize,
    already_failed: bool,
) -> Result<(), DevError> {
    let run_failed = |r: &SweepResult| !r.success || r.diagnostics.iter().any(keep);
    if !results.iter().any(run_failed) {
        // The phase's one green line. Only inside `check`: `brokkr clippy`
        // closes with its own `clippy clean` verdict.
        if !already_failed {
            reporter.green_line(&format!(
                "{}: ok ({}) in {}",
                phase.as_str(),
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
        &format!("cargo {}", phase.cargo_verb().unwrap_or(phase.as_str())),
        results,
        Some(info),
        project_root,
        active_sweep_count,
        keep,
    ));
    Err(DevError::Reported(format!("{} failed", phase.as_str())))
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
    phase: Phase,
    reporter: Reporter,
    info: &build::ProjectInfo,
    sweeps: &[PlannedSweep],
    selection: &PhaseSelection,
    reach: &mut dyn FnMut(usize),
    serial_builds: bool,
    run_one: impl Fn(&PlannedSweep, &ResolutionRun, Option<&Path>) -> Result<SweepResult, DevError> + Sync,
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
    let note = |line: &str| reporter.config_note(line);
    for (i, sweep) in sweeps.iter().enumerate() {
        let Some(entry) = selection.entry(LaneIdx::new(i)) else { continue };
        // A lane keeping some packages drops the rest with a note; a lane
        // keeping none says so once, as its skip reason.
        if matches!(entry, LaneEntry::Attempt(_) | LaneEntry::Deduped(_)) {
            for dropped_note in entry.notes() {
                note(&format!("{} {}: {dropped_note} (dropped)", phase.as_str(), sweep.label));
            }
        }
        let attempt = match entry {
            LaneEntry::Attempt(a) => a,
            // Diagnostic phases are per-build-shape while tests are per-lane:
            // two lanes compiling the same requests (a shared `[[check]]`
            // entry, or two sweeps a `-p` narrows to the same projection) are
            // linted or documented once.
            LaneEntry::Deduped(_) => {
                note(&format!("{} {}: deduped (build shape already checked)", phase.as_str(), sweep.label));
                continue;
            }
            other => {
                if let Some(reason) = other.skip_reason() {
                    note(&format!("{} {}: skipped ({reason})", phase.as_str(), sweep.label));
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
    let outcomes = run_grouped(&keyed, serial_builds, &|j| {
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
                if e.rendering_policy() != crate::error::RenderingPolicy::AlreadyPrinted {
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
    sweeps: &[PlannedSweep],
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
    let widened = if !allow_exact.iter().any(|s| crate::config::check::reaches_test_build(&s.lint)) {
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
    phase: Phase,
    results: &[SweepResult],
    allow_exact: &[SitedAllow],
    narrowed: bool,
) {
    if narrowed {
        return;
    }
    let judged: Vec<&SitedAllow> = allow_exact
        .iter()
        .filter(|s| phase.judges_lint(&s.lint))
        .collect();
    if judged.is_empty() {
        return;
    }
    let mut matched = vec![false; judged.len()];
    for r in results {
        for d in &r.diagnostics {
            for (i, s) in judged.iter().enumerate() {
                if sited_match(s, d) {
                    matched[i] = true;
                }
            }
        }
    }
    for (s, hit) in judged.iter().zip(&matched) {
        if !hit {
            output::warn(&format!(
                "{}: allow_exact {s} suppressed nothing (stale entry?)",
                phase.as_str()
            ));
        }
    }
}

/// Does a `[clippy] allow_exact` entry suppress this diagnostic? Lint names
/// match with or without the `clippy::` qualifier (mirroring what `allow`
/// accepts); the file must equal the entry's path exactly - cargo emits
/// build-root-relative paths, which is also what the entry is written from.
fn sited_match(s: &SitedAllow, d: &Diagnostic) -> bool {
    let Some(code) = d.code.as_ref() else {
        return false;
    };
    let lint_matches = s.lint.matches(code);
    lint_matches && d.path() == Some(Path::new(s.path.as_str()))
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
) -> Vec<Diagnostic> {
    let mut events = cargo_json::parse_cargo_diagnostics(stdout);
    if !allow_exact.is_empty() {
        events.retain(|d| !allow_exact.iter().any(|s| sited_match(s, d)));
    }
    events
}

/// Investigative single-phase clippy runner (`brokkr clippy`). Builds one
/// `PlannedSweep` from the CLI shape and runs the shared clippy pipeline.
fn probe_is_narrowed(sweep: &PlannedSweep) -> bool {
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
    /// Structured diagnostics parsed once from cargo JSON. Clippy appends its
    /// stderr-only manifest lints here after assigning their owners.
    diagnostics: Vec<Diagnostic>,
    /// Manifest warnings already handled by the structured pipeline (selected
    /// or filtered by owner). These blocks never reappear in raw stderr.
    suppressed_manifest_blocks: Vec<usize>,
    /// Denied manifest lints listed in `diagnostics`, with the stderr block
    /// each came from. Such a block is consumed only while the phase's filter
    /// keeps its diagnostic: an error an allow removed from the list is still
    /// cargo's reason for failing, so its raw block prints.
    listed_manifest_errors: Vec<(usize, Diagnostic)>,
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
    diag: &'a crate::diagnostic::Diagnostic,
    sweeps: Vec<String>,
}

/// The merge identity of one listed diagnostic: code, site and message, at
/// the level the list renders every entry at. A diagnostic is an error in
/// this list whatever level cargo gave it (see [`clippy_parse_from_events`]),
/// so a capped lint and its uncapped twin from another sweep are one entry.
fn merge_key(d: &crate::diagnostic::Diagnostic) -> crate::diagnostic::DiagnosticKey {
    d.key_as(Level::Error)
}

/// Merge clippy diagnostics across sweeps, deduplicating by [`merge_key`].
/// `parses` is `(label, parse_result)` pairs from each sweep; sweep labels
/// are owned strings since `[[check]]` entry names are user-defined.
fn merge_clippy<'a>(
    parses: &'a [(String, cargo_filter::ClippyParse)],
) -> Vec<MergedDiag<'a>> {
    let mut order: Vec<crate::diagnostic::DiagnosticKey> = Vec::new();
    let mut by_key: HashMap<crate::diagnostic::DiagnosticKey, MergedDiag<'a>> = HashMap::new();

    for (label, parsed) in parses {
        for d in &parsed.diagnostics {
            let key = merge_key(d);
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
    keep: &dyn Fn(&Diagnostic) -> bool,
) -> String {
    let multi = active_sweep_count > 1;
    // Each diagnostic's package, keyed as the merge keys it: identical
    // diagnostics from two runs come from the same source line, so the same
    // package.
    let mut package_of: HashMap<crate::diagnostic::DiagnosticKey, String> = HashMap::new();
    let parses: Vec<(String, cargo_filter::ClippyParse)> = results
        .iter()
        .map(|r| {
            let mut events = r.diagnostics.clone();
            events.retain(|d| keep(d));
            let parse = clippy_parse_from_events(&events, !r.success);
            for d in &parse.diagnostics {
                if let Some(id) = &d.package_id {
                    package_of.entry(merge_key(d)).or_insert_with(|| id.clone());
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
                package_of.get(&merge_key(m.diag)).map(String::as_str),
                results,
                info,
                active_sweep_count,
            )
        {
            out.push_str(&tag);
            out.push(' ');
        }
        out.push_str(&m.diag.format_as(Level::Error));
        out.push('\n');
    }
    if !streams.is_empty() {
        out.push('\n');
        out.push_str(&streams);
    }
    out.trim_end().to_string()
}

/// The stderr blocks the diagnostic list already carries: every manifest
/// warning (including those filtered by owner or selection, which never
/// reappear as raw text) and the denied manifest lints `keep` still lists.
fn consumed_manifest_blocks(
    r: &SweepResult,
    keep: &dyn Fn(&Diagnostic) -> bool,
) -> std::collections::HashSet<usize> {
    r.suppressed_manifest_blocks
        .iter()
        .copied()
        .chain(
            r.listed_manifest_errors
                .iter()
                .filter(|(_, d)| keep(d))
                .map(|(block, _)| *block),
        )
        .collect()
}

/// Whether a failed run's stderr holds an `error` block the list does not
/// explain: anything but a consumed manifest block and cargo's own summaries of
/// a failure reported elsewhere (`rustc_text::BlockKind::FailureSummary`).
fn has_unlisted_stderr_error(r: &SweepResult, keep: &dyn Fn(&Diagnostic) -> bool) -> bool {
    let listed = consumed_manifest_blocks(r, keep);
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
///   (consumed manifest blocks are already handled structurally), and a build
///   script's `--- stdout` inside one loses
///   its `cargo:` directives ([`drop_build_directives`]). Any other block is
///   withheld only when it is self-contained ([`is_self_contained`]); one that
///   swallowed unindented text after its header prints whole, since that text
///   may be the failure's cause - that check comes before any other reason to
///   drop a block. Not counted as withheld: manifest lints, cargo's
///   `generated N warnings` tallies and its `build failed, waiting` status.
///   With no `error` block, stderr is the evidence and prints without manifest
///   lints already handled by the structured path.
fn failed_run_streams(r: &SweepResult, keep: &dyn Fn(&Diagnostic) -> bool) -> String {
    let mut out = String::new();
    let blocks = rustc_text::rustc_blocks(&r.stderr);
    if blocks.iter().any(|b| b.level == Level::Error) {
        let listed = consumed_manifest_blocks(r, keep);
        let mut withheld = 0usize;
        for (i, block) in blocks.iter().enumerate() {
            let print = if listed.contains(&i) {
                false
            } else {
                match block.level {
                    Level::Error => true,
                    Level::Warning => {
                        let contained = is_self_contained(&block.text, true);
                        if contained && rustc_text::classify_block(block) != BlockKind::CargoStatus {
                            withheld += 1;
                        }
                        !contained
                    }
                    // cargo's `Compiling` progress is indented; anything else
                    // ahead of the first header is kept.
                    Level::Other => !is_self_contained(&block.text, false),
                }
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
        out.push_str(&stderr_without_manifest_lints(&r.stderr));
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
/// `compiler-message` that `keep` rejected as a typed diagnostic, any other
/// cargo record nothing.
/// Parsed rather than prefix-matched, so a tool line that merely starts with
/// `{` survives.
fn failed_stdout_line(line: &str, keep: &dyn Fn(&Diagnostic) -> bool) -> Option<String> {
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
    let diagnostic = cargo_json::parse_cargo_diagnostics(line).into_iter().next()?;
    if !is_error_level(&diagnostic) || keep(&diagnostic) {
        return None;
    }
    Some(diagnostic.format_as(Level::Error))
}

/// Whether a diagnostic is at error level. The reader maps rustc's ICE
/// spelling (`error: internal compiler error`) to [`Level::Error`] as well.
fn is_error_level(d: &Diagnostic) -> bool {
    d.is_error()
}

/// Parse cargo's `--message-format=json` stdout into a
/// [`ClippyParse`](cargo_filter::ClippyParse).
///
/// Every compiler-message JSON event, in discovery order. `parse_failed`
/// follows [`clippy_parse_from_events`]'s rule.
#[cfg(test)]
fn parse_clippy_from_json(
    stdout: &str,
    sweep_failed: bool,
    allow_exact: &[SitedAllow],
) -> cargo_filter::ClippyParse {
    clippy_parse_from_events(&gated_diags(stdout, allow_exact), sweep_failed)
}

/// Collect already-filtered diagnostic events for the list, in discovery
/// order, levels as cargo gave them. `parse_failed` is set when cargo failed
/// and left no `error`-level event, so callers also print the captured
/// streams: under `--cap-lints=warn` a lint never fails cargo, so a failed run
/// with warnings alone failed for a reason no diagnostic names - a build
/// script's panic beside some crate's lints, which the old "no event at all"
/// rule hid.
///
/// The list renders every entry as an error ([`merge_key`], `format_as`),
/// whatever level cargo gave it: the one kind of warning `check` does not
/// treat as a failure - a dependency's - was dropped upstream through
/// [`Diagnostic::is_dependency_warning`], and clippy runs under `--cap-lints=warn` only
/// so a denied lint cannot abort its crate's compile.
fn clippy_parse_from_events(
    events: &[Diagnostic],
    sweep_failed: bool,
) -> cargo_filter::ClippyParse {
    let parse_failed = sweep_failed && !events.iter().any(is_error_level);
    cargo_filter::ClippyParse {
        diagnostics: events.to_vec(),
        parse_failed,
    }
}

/// Sort key for the diagnostic list: by lint code (so every hit of a rule
/// clumps together), then file, line and column for stable in-rule ordering.
/// A diagnostic with no code sorts to the end.
fn clippy_sort_key(d: &crate::diagnostic::Diagnostic) -> (String, PathBuf, u64, u64) {
    // A key that sorts after any real code.
    let lint_key = d
        .code
        .as_ref()
        .map(|code| code.as_str().to_owned())
        .unwrap_or_else(|| "\u{10FFFF}".to_owned());
    let (file, line, column) = match &d.site {
        Some(site) => (site.file.clone(), site.line.unwrap_or(0), site.column.unwrap_or(0)),
        None => (PathBuf::new(), 0, 0),
    };
    (lint_key, file, line, column)
}

/// Cargo JSON diagnostics plus cargo's stderr-only manifest lints, classified
/// through the same owner index. Duplicate messages from the two channels are
/// collapsed by their typed identity.
fn owned_build_diagnostics(
    stdout: &str,
    stderr: &str,
    owners: &crate::diagnostic::OwnerIndex,
) -> Vec<crate::diagnostic::Diagnostic> {
    let mut diagnostics = cargo_json::parse_cargo_diagnostics(stdout);
    diagnostics.extend(
        cargo_json::manifest_lints_by_block(&rustc_text::rustc_blocks(stderr))
            .into_iter()
            .map(|(_, diagnostic)| diagnostic),
    );
    for diagnostic in &mut diagnostics {
        diagnostic.classify_owner(owners);
    }

    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|diagnostic| seen.insert(diagnostic.key_as(diagnostic.level)));
    diagnostics
}

/// The manifest-lint blocks already represented by `owned_build_diagnostics`.
/// Keep the rest of stderr verbatim for cargo errors and build-script output.
fn stderr_without_manifest_lints(stderr: &str) -> String {
    let blocks = rustc_text::rustc_blocks(stderr);
    let listed: std::collections::HashSet<usize> =
        cargo_json::manifest_lints_by_block(&blocks).into_iter().map(|(index, _)| index).collect();
    blocks
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !listed.contains(index))
        .map(|(_, block)| block.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render cargo diagnostics with the shared one-line formatter and typed sort
/// order. Dependency warnings are the only findings omitted.
fn render_build_diagnostics(
    tool: &str,
    diagnostics: &[crate::diagnostic::Diagnostic],
    warnings_only: bool,
) -> Option<String> {
    let mut visible: Vec<&crate::diagnostic::Diagnostic> = diagnostics
        .iter()
        .filter(|diagnostic| {
            !diagnostic.is_dependency_warning()
                && (!warnings_only || diagnostic.is_warning())
        })
        .collect();
    if visible.is_empty() {
        return None;
    }
    visible.sort_by_key(|diagnostic| {
        let (path, line, column) = diagnostic.site.as_ref().map_or_else(
            || (std::path::PathBuf::new(), 0, 0),
            |site| (site.file.clone(), site.line.unwrap_or(0), site.column.unwrap_or(0)),
        );
        let level = match diagnostic.level {
            Level::Error => 0,
            Level::Warning => 1,
            Level::Other => 2,
        };
        (
            diagnostic
                .code
                .as_ref()
                .map(|code| code.as_str().to_owned())
                .unwrap_or_else(|| "\u{10FFFF}".into()),
            path,
            line,
            column,
            level,
            diagnostic.message.clone(),
        )
    });
    let errors = visible.iter().filter(|d| d.is_error()).count();
    let warnings = visible.iter().filter(|d| d.is_warning()).count();
    let mut rendered = format!("{tool}: {errors} errors, {warnings} warnings");
    for diagnostic in visible {
        rendered.push_str("\n  ");
        rendered.push_str(&diagnostic.format_one());
    }
    Some(rendered)
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


