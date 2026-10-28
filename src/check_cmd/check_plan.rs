// The planning prefix of `brokkr check`, shared by the run and `check --audit`.
//
// One planner decides everything a check run settles before its first phase:
// the profile and its claim, the tested sweeps and lint lanes, the doctest
// carriers a complete claim derives, the skipped phases, the serial-build
// answers and every phase's lane selection. `cmd_check` executes the plan;
// `cmd_check_audit` renders it. Neither re-derives any of it, so the audit
// cannot describe a run the check would not make.
//
// Planning builds nothing, but it is not pure: it may read `cargo metadata`
// (parallel promotion, feature projection, a complete claim's doc facts), git
// (the markdown-only shortcut) and the cargo config chain (serial builds). Its
// announcements go through a [`Narrator`] at the point they are decided, so a
// run that refuses mid-plan has printed exactly what it printed before the
// planner existed, and the audit collects the same lines instead.

/// Where planning's announcements go: straight to the console and run log
/// (a check run), or into a list the audit prints as part of its plan.
pub(crate) struct Narrator {
    collected: Option<std::cell::RefCell<Vec<String>>>,
}

impl Narrator {
    /// Print each line as it is decided, on its own channel.
    pub(crate) fn live() -> Self {
        Self { collected: None }
    }

    /// Keep every line, whatever its channel: the audit shows run-log-only
    /// lines on the console too, since explaining the run is its whole job.
    fn collecting() -> Self {
        Self {
            collected: Some(std::cell::RefCell::new(Vec::new())),
        }
    }

    /// A console line (`output::run_msg` on a live run).
    pub(crate) fn run_msg(&self, line: &str) {
        match &self.collected {
            Some(lines) => lines.borrow_mut().push(line.to_owned()),
            None => output::run_msg(line),
        }
    }

    /// A run-log line (`output::detail` on a live run).
    pub(crate) fn detail(&self, line: &str) {
        match &self.collected {
            Some(lines) => lines.borrow_mut().push(line.to_owned()),
            None => output::detail(line),
        }
    }

    fn into_lines(self) -> Vec<String> {
        self.collected.map(std::cell::RefCell::into_inner).unwrap_or_default()
    }
}

/// Everything planning reads: the config sections that decide a run, and the
/// invocation's own shaping flags.
pub(crate) struct PlanInputs<'a> {
    pub(crate) project_root: &'a Path,
    pub(crate) check_entries: &'a [CheckEntry],
    pub(crate) lint_sweeps: &'a [LintSweep],
    pub(crate) quarantine: &'a [QuarantineEntry],
    pub(crate) test_cfg: Option<&'a TestConfig>,
    pub(crate) gated: GatedPhases<'a>,
    pub(crate) script_checks: &'a [ScriptCheck],
    pub(crate) rustdoc_configured: bool,
    pub(crate) features: &'a [String],
    pub(crate) no_default_features: bool,
    pub(crate) packages: &'a [String],
    pub(crate) profile_name: Option<&'a str>,
    pub(crate) gate: bool,
    pub(crate) force_rust: bool,
    pub(crate) extra_args: &'a [String],
}

/// What one check run will do, decided before any phase starts.
pub(crate) struct CheckPlan<'a> {
    /// The profile that drove sweep selection; `None` for an ad-hoc,
    /// profile-less or legacy run.
    profile_label: Option<String>,
    certifies: Option<Certifies>,
    claim: Claim,
    /// The profile's `skip_phases`.
    skip_phases: &'a [String],
    /// The phases the markdown-only shortcut skips; empty when it does not apply.
    prose_skips: Vec<Phase>,
    /// The `[[script_check]]` entries the claim owes.
    owed_scripts: Vec<ScriptCheck>,
    sweeps: RunSweeps,
    /// A complete claim's doc facts, unless a `doctests` quarantine waives them.
    doc_facts: Option<DocFacts>,
    /// Serial lanes run doctests inline.
    doctests: bool,
    serial_builds: bool,
    diagnostic_serial_builds: bool,
    selections: CheckSelections,
}

impl CheckPlan<'_> {
    /// Whether the run skips `phase`: the profile's `skip_phases` or the
    /// markdown-only shortcut.
    fn skips(&self, phase: Phase) -> bool {
        self.skip_phases.iter().any(|name| name == phase.as_str()) || self.prose_skips.contains(&phase)
    }
}

/// Plan one check run. Announcements are made through `narrate` in the order
/// they are decided; an error returns after whatever was already said.
#[allow(clippy::too_many_lines)]
pub(crate) fn plan_check<'a>(i: &PlanInputs<'a>, narrate: &Narrator) -> Result<CheckPlan<'a>, DevError> {
    let gate_name = resolve_gate_profile(i.gate, i.test_cfg)?;
    let profile_name = gate_name.as_deref().or(i.profile_name);
    let invocation = (i.project_root, i.packages, i.extra_args);
    let mut active_sweeps = active_sweeps_resolved(
        i.check_entries,
        i.test_cfg,
        profile_name,
        (i.features, i.no_default_features),
        invocation,
    )?;
    // The profile that drove sweep selection, if any. Ad-hoc CLI features
    // bypass profiles entirely (priority 1 in decide_active_sweeps), so an
    // ad-hoc run reports no profile even when [test].default_profile is set.
    let adhoc = !i.features.is_empty() || i.no_default_features;
    let profile_label = if adhoc { None } else { effective_profile_name(i.test_cfg, profile_name)? };
    let (certifies, skip_phases) = profile_claim(&profile_label, i.test_cfg);
    let claim = Claim::new(certifies);
    reject_scoped_complete(claim, i.packages)?;
    reject_extra_args_complete(claim, i.extra_args)?;
    // Each script check's `when` against the claim, decided once: the
    // background start, every stage and the post-test skipped count see only
    // the entries this run owes.
    let owed_scripts = scripts_for_claim(i.script_checks, claim, narrate);
    // The `[[lint_sweep]]` lanes this run owes, planned like the tested ones
    // but never handed to the test machinery. An ad-hoc features run replaces
    // sweep selection wholesale, so it takes none.
    let lint_lanes = if adhoc { Vec::new() } else { lint_lanes_for_claim(i.lint_sweeps, claim, invocation, narrate)? };

    // Each planned cargo request carries the sweep's features projected onto
    // its packages; the metadata snapshot that takes is read at most once,
    // and only when some projection could drop a token.
    let oracle = FeatureOracle::at(i.project_root);
    // A complete claim owes every tested build's doctests, unless a
    // `[[quarantine]] category = "doctests"` entry justifies their absence:
    // the carriers its configured lanes leave missing are derived here,
    // before anything is selected, so every phase, the plan and the journal
    // see them as lanes like any other.
    let doctests_quarantined = i.quarantine.iter().any(|q| q.category.as_deref() == Some("doctests"));
    let doc_facts = if claim.enumerates_universe() { Some(workspace_doc_facts(i.project_root, &[])?) } else { None };
    if let Some(facts) = &doc_facts {
        let reserved: Vec<&str> = lint_lanes.iter().map(|l| l.label.as_str()).collect();
        let derived = derive_doctest_carriers(&active_sweeps, &reserved, facts, &oracle)?;
        if doctests_quarantined {
            // Said, not run: the quarantine's reach is part of what the run is.
            announce_quarantined_carriers(&derived, narrate);
        } else {
            announce_derived_carriers(&derived, narrate);
            active_sweeps.extend(derived);
        }
    }
    let doc_facts = doc_facts.filter(|_| !doctests_quarantined);
    let sweeps = RunSweeps::new(active_sweeps, lint_lanes)?;

    announce_profile_header(sweeps.tested(), &profile_label, narrate);
    announce_lint_lanes(sweeps.lint(), narrate);
    announce_invocation_shaping(i.project_root, i.packages, i.extra_args, narrate);
    announce_adhoc_shaping(i.features, i.no_default_features, i.test_cfg, profile_name, narrate)?;

    // Whether a serial lane runs doctests inline: off unless `[test] doctests
    // = true` (their separate rustdoc pass is kept out of the edit loop for
    // cost), and off under a complete claim, whose doctests run once per
    // tested build, unfiltered, in its doc carriers - an inline pass would
    // run them again under the lane's `skip`/`only`, which no audit can hold
    // to anything.
    let doctests = i.test_cfg.is_some_and(|c| c.doctests) && !claim.enumerates_universe();
    // Two answers: a lint lane's env can configure a build dir for the
    // diagnostic phases that run it, and must not reschedule preparation.
    let serial_configured = i.test_cfg.is_some_and(|c| c.serial_builds);
    let diagnostic_serial_builds =
        resolve_serial_builds(i.project_root, serial_configured, sweeps.linted(), "clippy and rustdoc", narrate);
    let serial_builds = resolve_serial_builds(i.project_root, serial_configured, sweeps.tested(), "prepare", narrate);

    // A partial profile may skip phases (validated against Phase::ALL at
    // load time). Announce the omission up front - a narrowed run must
    // never look like a full one in the log.
    announce_skipped_phases(skip_phases, narrate);

    // On top of that, a tree whose only uncommitted change is markdown gets
    // the prose-only shortcut: the conventions still apply to prose, but
    // nothing that was just edited can have changed how the code builds.
    // Anything that shapes or demands the build waives the shortcut. A profile
    // covers `--gate` and `certifies` too: both imply one is in effect.
    let shaped = adhoc || !i.packages.is_empty() || !i.extra_args.is_empty();
    let waived = i.force_rust || shaped || profile_label.is_some();
    let prose_skips = prose_only_skips(i.project_root, waived, narrate);
    let skip = |phase: Phase| skip_phases.iter().any(|name| name == phase.as_str()) || prose_skips.contains(&phase);

    // Which lanes each phase is eligible to attempt, with which packages:
    // decided once, here, and read by every phase. A structural error (package
    // mode over a selection naming no packages) fails before anything runs,
    // with no trailer; a "nothing reached" refusal is stored and reported at
    // its phase's boundary.
    let selections =
        CheckSelections::build(&sweeps, i.packages, &skip, i.rustdoc_configured, i.gated, certifies, &oracle)?;
    announce_package_rules(sweeps.linted(), &selections, narrate);
    narrate_feature_drops(
        &[
            (sweeps.linted(), &selections.clippy),
            (sweeps.linted(), &selections.rustdoc),
            (sweeps.tested(), &selections.test),
        ],
        narrate,
    );

    Ok(CheckPlan {
        profile_label,
        certifies,
        claim,
        skip_phases,
        prose_skips,
        owed_scripts,
        sweeps,
        doc_facts,
        doctests,
        serial_builds,
        diagnostic_serial_builds,
        selections,
    })
}

/// The config `check --audit` reads beyond what planning does: the selector
/// sections it audits and the ones its findings come from.
pub(crate) struct AuditInputs<'a> {
    pub(crate) gremlins_cfg: Option<&'a GremlinsConfig>,
    pub(crate) header_cfg: Option<&'a HeaderConfig>,
    pub(crate) textlint_rules: &'a [TextlintRule],
    pub(crate) manifest_cfg: Option<&'a ManifestConfig>,
    pub(crate) dependency_rules: &'a [DependencyRule],
    pub(crate) lints: &'a LintsConfig,
}

/// `check --audit`: explain the check this invocation would run, then report
/// configuration findings. No phase runs and nothing is built; planning may
/// read `cargo metadata`, git and the cargo config, as a run's planning does.
/// The findings are each computed on their own, so one that cannot be
/// computed is itself a finding and the rest still print. An ordinary check
/// stays silent about every one of them: a user-wide rule legitimately names
/// paths most trees lack, so saying so on every run is noise.
///
/// Findings are reported, not judged: the exit status is 0 either way. A stop
/// (Ctrl-C, `brokkr kill`) is not a finding and ends the audit as it ends a run.
pub(crate) fn cmd_check_audit(plan_inputs: &PlanInputs<'_>, a: &AuditInputs<'_>) -> Result<(), DevError> {
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let root = plan_inputs.project_root;
    let mut findings: Vec<String> = Vec::new();

    let narrate = Narrator::collecting();
    let planned = plan_check(plan_inputs, &narrate);
    let narration = narrate.into_lines();
    match planned {
        Ok(plan) => {
            output::run_msg(&format!("check plan: {}", plan_header(&plan, plan_inputs)));
            for line in narration.iter().chain(&plan_lines(&plan, plan_inputs, a)) {
                output::run_msg(&format!("  {line}"));
            }
        }
        Err(e) if is_stop(&e) => return Err(e),
        Err(e) => {
            output::run_msg("check plan (incomplete - planning refused):");
            for line in &narration {
                output::run_msg(&format!("  {line}"));
            }
            findings.push(format!("check plan refused: {e}"));
        }
    }

    let tree = LazyTree::new(root);
    for line in textlint_count_lines(&tree, a.textlint_rules)? {
        output::run_msg(&line);
    }
    findings.extend(conventions::selector_findings(&tree, a.gremlins_cfg, a.header_cfg, a.textlint_rules, a.manifest_cfg)?);
    findings.extend(unreferenced_check_entries(plan_inputs.check_entries, plan_inputs.test_cfg));
    findings.extend(workspace_selector_findings(root, a.dependency_rules, plan_inputs.quarantine)?);
    findings.extend(absent_allow_exact_paths(root, a.lints));

    for finding in &findings {
        output::warn(finding);
    }
    output::run_msg(&format!("audit: {}", output::count(findings.len(), "finding")));
    Ok(())
}

/// An error that stops the command rather than describing the config.
fn is_stop(e: &DevError) -> bool {
    matches!(e, DevError::Interrupted | DevError::ExitCode(_)) || crate::shutdown::is_shutdown_requested()
}

/// The profile the plan resolved and what it claims.
fn plan_header(plan: &CheckPlan<'_>, i: &PlanInputs<'_>) -> String {
    let claim = match plan.certifies {
        Some(Certifies::Complete) => "certifies complete",
        Some(Certifies::Partial) => "certifies partial",
        None => "certifies nothing",
    };
    match &plan.profile_label {
        Some(name) => format!("profile {name} ({claim})"),
        None if !i.features.is_empty() || i.no_default_features => format!("ad-hoc features, no profile ({claim})"),
        None => format!("no profile ({claim})"),
    }
}

/// The plan's facts the narration does not state: every lane's configured
/// shape and every phase's disposition.
fn plan_lines(plan: &CheckPlan<'_>, i: &PlanInputs<'_>, a: &AuditInputs<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    let tested = plan.sweeps.tested().len();
    for (k, sweep) in plan.sweeps.linted().iter().enumerate() {
        let kind = if k < tested { "tested" } else { "lint only" };
        lines.push(format!("lane {} ({kind}): {}", sweep.label, lane_shape(sweep)));
    }
    for phase in Phase::ALL {
        lines.push(format!("phase {}: {}", phase.as_str(), phase_disposition(phase, plan, i, a)));
    }
    lines
}

/// One lane's configured shape. Configured, not effective: a `-p` narrowing
/// or a package-mode split can change the packages and features a given
/// resolution receives, and the narration names every such drop.
fn lane_shape(sweep: &PlannedSweep) -> String {
    let mut parts = Vec::new();
    parts.push(if !sweep.packages.is_empty() {
        format!("packages {}", sweep.packages.join(", "))
    } else if !sweep.test_exclude_packages.is_empty() {
        format!("workspace less {} (test phase)", sweep.test_exclude_packages.join(", "))
    } else {
        "default members".to_owned()
    });
    let features = sweep.features.configured_key();
    parts.push(if features.is_empty() { "default features".to_owned() } else { features.join(" ") });
    if let Some(target) = &sweep.target {
        parts.push(format!("target {target}"));
    }
    if let Some(mode) = sweep.effective_unification().describe() {
        parts.push(mode.to_owned());
    }
    if !sweep.build_packages.is_empty() {
        parts.push(format!("builds {}", sweep.build_packages.join(", ")));
    }
    format!("configured {}", parts.join("; "))
}

/// What the plan decided for one phase, and why.
fn phase_disposition(phase: Phase, plan: &CheckPlan<'_>, i: &PlanInputs<'_>, a: &AuditInputs<'_>) -> String {
    if plan.skip_phases.iter().any(|name| name == phase.as_str()) {
        return "skipped (profile skip_phases)".to_owned();
    }
    if plan.prose_skips.contains(&phase) {
        return "skipped (markdown-only tree)".to_owned();
    }
    let planned = |what: String| if what.is_empty() { "planned".to_owned() } else { format!("planned ({what})") };
    let selection = |s: &PhaseSelection, lanes: usize| match s.refusal() {
        Some(r) => format!("refuses: {r}"),
        None => planned(output::count(
            (0..lanes).filter(|&k| s.attempt(LaneIdx::new(k)).is_some()).count(),
            "eligible lane",
        )),
    };
    let linted = plan.sweeps.linted().len();
    match phase {
        Phase::Gremlins if a.gremlins_cfg.is_some_and(|c| c.disable) => "disabled by config".to_owned(),
        Phase::Header if a.header_cfg.is_none() => "not configured".to_owned(),
        Phase::Textlint if a.textlint_rules.is_empty() => "not configured".to_owned(),
        Phase::Textlint => planned(output::count(a.textlint_rules.len(), "rule")),
        Phase::Manifest if a.manifest_cfg.is_none() => "not configured".to_owned(),
        Phase::ScriptCheck if i.script_checks.is_empty() => "not configured".to_owned(),
        Phase::ScriptCheck if plan.owed_scripts.is_empty() => "none owed (every entry ruled out by its when)".to_owned(),
        Phase::ScriptCheck => {
            // Post-test checks judge a test phase; with none they are skipped.
            let post_test = plan.owed_scripts.iter().filter(|c| c.stage == Stage::PostTest).count();
            let skipped = if plan.skips(Phase::Test) && post_test > 0 {
                format!(", {post_test} post-test skipped with the test phase")
            } else {
                String::new()
            };
            planned(format!(
                "{} owed of {}{skipped}",
                output::count(plan.owed_scripts.len(), "check"),
                i.script_checks.len()
            ))
        }
        Phase::DependencyRules if a.dependency_rules.is_empty() => "not configured".to_owned(),
        Phase::DependencyRules => planned(output::count(a.dependency_rules.len(), "rule")),
        Phase::Clippy => selection(&plan.selections.clippy, linted),
        Phase::Rustdoc if !i.rustdoc_configured => "not configured".to_owned(),
        Phase::Rustdoc => selection(&plan.selections.rustdoc, linted),
        Phase::Test => selection(&plan.selections.test, plan.sweeps.tested().len()),
        Phase::Prepare if plan.skips(Phase::Test) => "not run (test skipped)".to_owned(),
        Phase::Prepare => "planned (with the test phase)".to_owned(),
        Phase::Coverage if !plan.claim.enumerates_universe() => "not run (no complete claim)".to_owned(),
        Phase::Coverage if plan.skips(Phase::Test) => "not run (test skipped)".to_owned(),
        Phase::Coverage => "planned (complete claim)".to_owned(),
        Phase::MemberCheck => match &plan.selections.members {
            MemberSelection::NotApplicable(why) => format!("not run ({why})"),
            MemberSelection::Eligible { .. } => planned(String::new()),
        },
        Phase::FixtureLint if i.gated.fixture_lints.is_empty() => "not configured".to_owned(),
        Phase::FixtureLint if plan.selections.fixtures.is_empty() => "none owed (every entry ruled out by its when)".to_owned(),
        Phase::FixtureLint => planned(output::count(plan.selections.fixtures.len(), "entry")),
        _ => planned(String::new()),
    }
}

/// One line per `[[textlint]]` rule with the tracked files its `paths` and
/// `exclude` select. A path count, not a scan count: `test_code`,
/// `only_if_file_matches` and binary content can still keep a selected file
/// from being matched, and judging those reads the files.
fn textlint_count_lines(tree: &LazyTree<'_>, rules: &[TextlintRule]) -> Result<Vec<String>, DevError> {
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    let counts = conventions::textlint_path_counts(tree, rules)?;
    let mut lines = vec![format!("textlint: {} (files selected by path)", output::count(rules.len(), "rule"))];
    for (name, n) in counts {
        lines.push(format!("  {name}: {}", output::count(n, "file")));
    }
    Ok(lines)
}

/// `[[check]]` entries no `[test.profiles]` entry references, through `lanes`
/// or `extends`. Said only where profiles exist. Without a `default_profile`
/// a bare check and `brokkr test` still select every entry, and `brokkr
/// clippy --sweep` replays any entry, so this names an entry no profile
/// reaches, not one that runs nowhere.
fn unreferenced_check_entries(entries: &[CheckEntry], test_cfg: Option<&TestConfig>) -> Vec<String> {
    let Some(cfg) = test_cfg.filter(|c| !c.profiles.is_empty()) else {
        return Vec::new();
    };
    let mut referenced = std::collections::BTreeSet::new();
    for name in cfg.profiles.keys() {
        let mut visited = std::collections::BTreeSet::new();
        crate::config::referenced_check_entries(&cfg.profiles, name, &mut visited, &mut referenced);
    }
    let bare = if cfg.default_profile.is_some() {
        "a bare check runs the default profile, so only `brokkr clippy --sweep` reaches it"
    } else {
        "with no default_profile, a bare check, `brokkr test` and `brokkr clippy --sweep` still reach it"
    };
    entries
        .iter()
        .filter(|e| !referenced.contains(&e.name))
        .map(|e| format!("[[check]] {:?} is referenced by no [test.profiles] entry; {bare}", e.name))
        .collect()
}

/// Package selectors that name no workspace package: every
/// `[[dependency_rule]]` `from` and `except` the dependency-rules phase would
/// refuse, and every `[[quarantine]] package` - which, naming no member,
/// can justify no pair. Reads one `cargo metadata --no-deps`, only when some
/// selector needs it; a metadata failure is one finding, not an error.
fn workspace_selector_findings(
    root: &Path,
    rules: &[DependencyRule],
    quarantine: &[QuarantineEntry],
) -> Result<Vec<String>, DevError> {
    let quarantined: Vec<&str> = quarantine.iter().filter_map(|q| q.package.as_deref()).collect();
    if rules.is_empty() && quarantined.is_empty() {
        return Ok(Vec::new());
    }
    let metadata = match crate::cargo::metadata::load(root, crate::cargo::metadata::Query::workspace(), &[], None) {
        Ok(m) => m,
        Err(e) if is_stop(&e) => return Err(e),
        Err(e) => return Ok(vec![format!("package selectors not audited: cargo metadata failed: {e}")]),
    };
    let mut out = crate::dependency_rules::selector_findings(&metadata, rules);
    let members: std::collections::BTreeSet<&str> = metadata
        .packages
        .iter()
        .filter(|p| metadata.is_member(&p.id))
        .map(|p| p.name.as_str())
        .collect();
    for q in quarantine {
        if let Some(pkg) = q.package.as_deref().filter(|p| !members.contains(p)) {
            out.push(format!(
                "[[quarantine]] {} (issue {}): package {pkg:?} is not a workspace package, so the entry justifies nothing",
                q.pattern.as_deref().unwrap_or("?"),
                q.issue
            ));
        }
    }
    Ok(out)
}

/// `[lints] allow_exact` entries whose path does not exist under the build
/// root (a dangling symlink counts as absent). A suppression is matched by
/// exact path, so one naming an absent file suppresses nothing today. It is
/// an absence, not a staleness verdict: a file a build generates may be
/// missing until the build runs, and whether a present file's suppression
/// still fires is known only from a run's diagnostics.
fn absent_allow_exact_paths(root: &Path, lints: &LintsConfig) -> Vec<String> {
    let mut out = Vec::new();
    for allow in &lints.allow_exact {
        let entry = format!("{}@{}", allow.lint.as_str(), allow.path);
        match std::fs::metadata(root.join(&allow.path)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                out.push(format!("[lints] allow_exact {entry:?}: the path is absent"));
            }
            Err(e) => out.push(format!("[lints] allow_exact {entry:?}: could not inspect the path: {e}")),
        }
    }
    out
}

#[cfg(test)]
mod check_plan_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn entry(name: &str) -> CheckEntry {
        CheckEntry { name: name.into(), ..Default::default() }
    }

    #[test]
    fn unreferenced_entries_follow_lanes_and_extends() {
        let cfg: TestConfig = toml::from_str(
            r#"
default_profile = "fast"

[profiles.base]
sweeps = ["a"]

[profiles.fast]
extends = "base"

[profiles.gate]
lanes = ["fast", "wide"]

[profiles.wide]
sweeps = ["b"]
"#,
        )
        .unwrap();
        let entries = [entry("a"), entry("b"), entry("orphan")];
        let found = unreferenced_check_entries(&entries, Some(&cfg));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("\"orphan\"") && found[0].contains("clippy --sweep"));
        // No profiles: a bare check runs every entry, nothing to say.
        assert!(unreferenced_check_entries(&entries, None).is_empty());
    }

    #[test]
    fn allow_exact_paths_are_judged_by_presence() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lints = LintsConfig {
            allow: Vec::new(),
            allow_exact: vec![
                SitedAllow::parse("clippy::too_many_lines@src/main.rs").unwrap(),
                SitedAllow::parse("clippy::too_many_lines@src/no_such_file.rs").unwrap(),
            ],
        };
        let found = absent_allow_exact_paths(root, &lints);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("no_such_file.rs") && found[0].contains("absent"));
    }
}
