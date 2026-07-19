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
use std::path::Path;

use crate::build;
use crate::cargo_filter;
use crate::cargo_json;
use crate::config::{
    CheckEntry, DependencyRule, GremlinsConfig, HeaderConfig, ManifestConfig, StyleConfig,
    TestConfig, TextlintRule,
};
use crate::dependency_rules;
use crate::error::DevError;
use crate::gremlins;
use crate::output;
use crate::profile::{self, ResolvedSweep};
use crate::project::Project;
use crate::scope;
use crate::test_runner::{self, LibtestOutcome};

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_check(
    project: Option<Project>,
    project_root: &Path,
    check_entries: &[CheckEntry],
    dependency_rules: &[DependencyRule],
    test_cfg: Option<&TestConfig>,
    gremlins_cfg: Option<&GremlinsConfig>,
    style_cfg: Option<&StyleConfig>,
    header_cfg: Option<&HeaderConfig>,
    textlint_rules: &[TextlintRule],
    manifest_cfg: Option<&ManifestConfig>,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    profile_name: Option<&str>,
    raw: bool,
    limit: usize,
    all: bool,
    fix_gremlins: bool,
    timings: bool,
    extra_args: &[String],
) -> Result<(), DevError> {
    let started = std::time::Instant::now();
    let active_sweeps =
        decide_active_sweeps(check_entries, test_cfg, profile_name, features, no_default_features)?;

    let mut collected_timings: Vec<TestTiming> = Vec::new();
    // Doctests are excluded by default (nextest, hence CI, never runs them);
    // an explicit `[test] doctests = true` opts back in. Absent `[test]`,
    // the honest default is off.
    let doctests = test_cfg.is_some_and(|c| c.doctests);

    // Run every phase behind one closure so a failure from *any* of them
    // (not just the test phase) still funnels through the summary line below,
    // reporting the same total wall time a passing run does.
    let mut run_phases = || -> Result<(), DevError> {
        run_gremlins(project_root, gremlins_cfg, limit, all, fix_gremlins)?;
        run_style(project_root, style_cfg, gremlins_cfg, limit, all)?;
        run_header(project_root, header_cfg, limit, all)?;
        run_textlint(project_root, textlint_rules, limit, all)?;
        run_manifest(project_root, manifest_cfg, limit, all)?;
        run_dependency_rules(project_root, dependency_rules, limit, all)?;
        run_clippy_phase(project_root, &active_sweeps, package, raw, limit, all)?;
        run_test_phase(
            project,
            project_root,
            &active_sweeps,
            package,
            raw,
            doctests,
            extra_args,
            timings.then_some(&mut collected_timings),
        )
    };
    let outcome = run_phases();

    if timings {
        emit_timings(&collected_timings, limit, all, active_sweeps.len() > 1);
    }

    match outcome {
        Ok(()) => {
            output::result_msg(&format!("check passed in {}", fmt_wall(started.elapsed())));
            Ok(())
        }
        Err(_) => {
            // The failing phase already printed its detail above; add the
            // symmetric summary line and exit non-zero without main echoing a
            // second, timing-less `[error]` line.
            output::error(&format!("check failed in {}", fmt_wall(started.elapsed())));
            Err(DevError::ExitCode(1))
        }
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

fn emit_timings(timings: &[TestTiming], limit: usize, all: bool, multi_sweep: bool) {
    if timings.is_empty() {
        output::run_msg("timings: no tests ran");
        return;
    }

    let mut sorted: Vec<&TestTiming> = timings.iter().collect();
    sorted.sort_by_key(|t| std::cmp::Reverse(t.elapsed));

    let total = sorted.len();
    let displayed: &[&TestTiming] = if all || total <= limit {
        &sorted
    } else {
        &sorted[..limit]
    };

    let mut msg = format!("timings: {total} test(s), slowest first\n");
    for t in displayed {
        let secs = t.elapsed.as_secs_f64();
        if multi_sweep {
            msg.push_str(&format!("  {secs:>7.3}s [{}] {}\n", t.sweep, t.name));
        } else {
            msg.push_str(&format!("  {secs:>7.3}s {}\n", t.name));
        }
    }
    if displayed.len() < total {
        msg.push_str(&format!("  ... {} more (rerun with --all to show)\n", total - displayed.len()));
    }
    output::run_msg(msg.trim_end());
}

/// Build the list of sweeps both phases iterate, applying the
/// priority ladder documented at the top of the file.
///
/// Returns `Err` only when the user asked for a `--profile` that
/// doesn't resolve. Every other branch always succeeds with at least
/// one sweep.
pub(crate) fn decide_active_sweeps(
    check_entries: &[CheckEntry],
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
    features: &[String],
    no_default_features: bool,
) -> Result<Vec<ResolvedSweep>, DevError> {
    // 1. CLI override: ad-hoc one-off sweep. Skips `[[check]]` and any
    //    profile entirely, and ships no `build_packages` (the user is
    //    spot-checking; if they need a CLI rebuild they pass --package).
    if !features.is_empty() || no_default_features {
        let mut feature_args = Vec::new();
        if no_default_features {
            feature_args.push("--no-default-features".into());
        }
        if !features.is_empty() {
            feature_args.push("--features".into());
            feature_args.push(features.join(","));
        }
        return Ok(vec![ResolvedSweep {
            label: "default".into(),
            cargo_feature_args: feature_args,
            build_packages: Vec::new(),
            packages: Vec::new(),
            libtest_args: Vec::new(),
            cargo_test_filters: Vec::new(),
            name_filters: Vec::new(),
            env: std::collections::BTreeMap::new(),
            ..Default::default()
        }]);
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
    limit: usize,
    all: bool,
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
                "fix-gremlins: rewrote {total} char(s) across {} file(s)",
                fixed.len()
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
    let (displayed, trailer) = if all {
        (found, None)
    } else {
        let changed = scope::changed_files(project_root);
        let part = scope::partition(found, |g| g.path.as_path(), limit, changed.as_ref());
        let trailer = scope::format_trailer(part.hidden_unscoped);
        (part.displayed, trailer)
    };

    let mut msg = format!("gremlins: {total} found\n");
    for g in &displayed {
        msg.push_str("  ");
        msg.push_str(&gremlins::format_one(g));
        msg.push('\n');
    }
    if let Some(t) = trailer {
        msg.push_str("  ");
        msg.push_str(&t);
        msg.push('\n');
    }
    msg.push_str("  hint: rerun with `brokkr check --fix-gremlins` to rewrite all banned chars in place\n");
    output::error(msg.trim_end());
    Err(DevError::Build("gremlins found".into()))
}

/// Apply the same scope-first prioritisation the gremlins/clippy phases use to a
/// native phase's violation list. Under `--all` everything is shown and there is
/// no trailer; otherwise the list is partitioned so every hit in a branch-changed
/// file (per `scope::changed_files`) is shown in full and only unscoped overflow
/// is capped at `limit`, returning the shared `+N in unchanged files` trailer.
/// `get_path` maps a violation to the file it belongs to.
///
/// `changed_files` is computed here rather than once in `cmd_check` so the git
/// call is paid only when a phase actually has violations to display. The native
/// phases fail fast (`?` in `run_phases`), so at most one of them ever reaches
/// this per invocation - there is no double-compute to avoid.
fn scope_limit<T>(
    violations: Vec<T>,
    project_root: &Path,
    limit: usize,
    all: bool,
    get_path: impl Fn(&T) -> &Path,
) -> (Vec<T>, Option<String>) {
    // In `--all` mode nothing is capped, so skip the git call entirely.
    let changed = if all {
        None
    } else {
        scope::changed_files(project_root)
    };
    scope_limit_with(violations, limit, all, get_path, changed.as_ref())
}

/// Pure core of [`scope_limit`] with the branch-changed file set injected, so the
/// scope-first ordering can be exercised without a live git repo.
fn scope_limit_with<T>(
    violations: Vec<T>,
    limit: usize,
    all: bool,
    get_path: impl Fn(&T) -> &Path,
    changed: Option<&std::collections::HashSet<std::path::PathBuf>>,
) -> (Vec<T>, Option<String>) {
    if all {
        return (violations, None);
    }
    let part = scope::partition(violations, get_path, limit, changed);
    let trailer = scope::format_trailer(part.hidden_unscoped);
    (part.displayed, trailer)
}

/// The `[style]` phase: opt-in native Rust style checks. Currently the single
/// blank-line-above-control-flow rule. Inert unless the project enables a rule
/// in `[style]`. Reuses the `[gremlins].exclude` list to skip vendored dirs.
fn run_style(
    project_root: &Path,
    style_cfg: Option<&StyleConfig>,
    gremlins_cfg: Option<&GremlinsConfig>,
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
    let Some(cfg) = style_cfg else {
        return Ok(());
    };
    if !cfg.rust_blank_line_above_control_flow {
        return Ok(());
    }

    let violations = crate::style::scan(project_root, gremlins_cfg)?;

    if violations.is_empty() {
        output::run_msg("style: ok");
        return Ok(());
    }

    output::run_msg("style: blank line above control flow (Rust)");
    let total = violations.len();
    let (displayed, trailer) =
        scope_limit(violations, project_root, limit, all, |v| v.file.as_path());
    let mut msg = format!("style: {total} violation(s)\n");
    for v in &displayed {
        msg.push_str("  ");
        msg.push_str(&crate::style::format_one(v));
        msg.push('\n');
    }
    if let Some(t) = trailer {
        msg.push_str("  ");
        msg.push_str(&t);
        msg.push('\n');
    }
    msg.push_str(
        "  hint: add a blank line above the construct, or share an identifier with the line above",
    );
    output::error(&msg);

    Err(DevError::Build("style check failed".into()))
}

/// The `[header]` phase: a required file header whose year must be current.
/// Inert unless the project has a `[header]` section.
fn run_header(
    project_root: &Path,
    header_cfg: Option<&HeaderConfig>,
    limit: usize,
    all: bool,
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
    let (displayed, trailer) =
        scope_limit(violations, project_root, limit, all, |v| v.file.as_path());
    let mut msg = format!("header: {total} violation(s)\n");
    for v in &displayed {
        msg.push_str("  ");
        msg.push_str(&crate::header::format_one(v, &expected));
        msg.push('\n');
    }
    if let Some(t) = trailer {
        msg.push_str("  ");
        msg.push_str(&t);
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
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
    if rules.is_empty() {
        return Ok(());
    }

    let violations = crate::textlint::scan(project_root, rules)?;

    if violations.is_empty() {
        output::run_msg("textlint: ok");
        return Ok(());
    }

    output::run_msg(&format!("textlint: {} rule(s)", rules.len()));
    let total = violations.len();
    let (displayed, trailer) =
        scope_limit(violations, project_root, limit, all, |v| v.file.as_path());
    let mut msg = format!("textlint: {total} violation(s)\n");
    for v in &displayed {
        msg.push_str("  ");
        msg.push_str(&crate::textlint::format_one(v));
        msg.push('\n');
    }
    if let Some(t) = trailer {
        msg.push_str("  ");
        msg.push_str(&t);
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
    limit: usize,
    all: bool,
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
    let (displayed, trailer) =
        scope_limit(violations, project_root, limit, all, |v| v.file.as_path());
    let mut msg = format!("manifest: {total} violation(s)\n");
    for v in &displayed {
        msg.push_str("  ");
        msg.push_str(&crate::manifest::format_one(v));
        msg.push('\n');
    }
    if let Some(t) = trailer {
        msg.push_str("  ");
        msg.push_str(&t);
        msg.push('\n');
    }
    output::error(msg.trim_end());

    Err(DevError::Build("manifest check failed".into()))
}

fn run_dependency_rules(
    project_root: &Path,
    rules: &[DependencyRule],
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
    if rules.is_empty() {
        return Ok(());
    }

    output::run_msg("cargo metadata --format-version 1 --no-deps (dependency rules)");
    let report = dependency_rules::check(project_root, rules)?;

    if report.violations.is_empty() {
        output::run_msg(&format!(
            "dependency rules: ok ({} rule(s), {} workspace package(s))",
            report.rules, report.packages,
        ));
        return Ok(());
    }

    let total = report.violations.len();
    let displayed = if all || total <= limit {
        &report.violations[..]
    } else {
        &report.violations[..limit]
    };
    let mut msg = format!("dependency rules: {total} violation(s)\n");
    for violation in displayed {
        msg.push_str("  ");
        msg.push_str(&dependency_rules::format_violation(violation));
        msg.push('\n');
    }
    if displayed.len() < total {
        msg.push_str(&format!(
            "  +{} more (--all to see)\n",
            total - displayed.len()
        ));
    }
    output::error(msg.trim_end());

    Err(DevError::Build("dependency rules failed".into()))
}

#[allow(clippy::too_many_arguments)]
fn run_clippy_phase(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    package: Option<&str>,
    raw: bool,
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
    let multi = sweeps.len() > 1;

    let mut results: Vec<SweepResult> = Vec::with_capacity(sweeps.len());
    for sweep in sweeps {
        // Always run with --message-format=json so the lint code
        // (`message.code.code`) is populated on every diagnostic. cargo's
        // pretty-printed stderr only includes the `= note: #[warn(rule)]`
        // annotation on the first occurrence of each lint per crate,
        // which made bulk triage by rule impossible in text mode.
        let mut args: Vec<String> = vec![
            "clippy".into(),
            // Keep checking independent branches of the graph after a unit
            // fails, instead of cargo's default fail-fast (which stops
            // scheduling new work at the first error and hides every lint
            // queued behind it).
            "--keep-going".into(),
            "--all-targets".into(),
            "--message-format=json".into(),
        ];
        // Scope to the sweep's packages (`-p <pkg>`) so `--features` is valid
        // in a virtual workspace, where cargo rejects features at the root.
        for pkg in &sweep.packages {
            args.push("-p".into());
            args.push(pkg.clone());
        }
        args.extend(sweep.cargo_feature_args.iter().cloned());
        if let Some(pkg) = package {
            args.push("--package".into());
            args.push(pkg.into());
        }
        // Cap lints at `warn` so a deny-level lint no longer aborts its
        // crate's compile: the crate still produces its .rmeta, so every
        // downstream crate is checked too, and one run surfaces every lint
        // across the whole workspace. Genuine (non-lint) compile errors are
        // unaffected and still fail. brokkr recovers the intent: it treats
        // every surfaced lint as a hard failure (see `event_to_clippy` and
        // the gate below), so nothing is silently downgraded.
        args.push("--".into());
        args.push("--cap-lints=warn".into());

        output::run_msg(&format!("cargo {}", args.join(" ")));

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // Apply the sweep's env to the clippy build too, so a build-affecting
        // var (codegen toggle, etc.) is set consistently across every phase -
        // clippy, the test pre-build, and the test run - not just the tests.
        let env_owned: Vec<(String, String)> = sweep
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let env_refs: Vec<(&str, &str)> = env_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let captured =
            output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;
        results.push(SweepResult {
            label: sweep.label.clone(),
            stdout: String::from_utf8_lossy(&captured.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
            success: captured.status.success(),
        });
    }

    // With `--cap-lints=warn`, a lint no longer makes cargo exit non-zero, so
    // the pass/fail decision is brokkr's own: any clippy diagnostic is a
    // failure, whatever its (capped) level. `any_failed` still catches genuine
    // non-lint compile errors, and the parse-failure case where cargo died
    // without emitting parseable diagnostics.
    let any_failed = results.iter().any(|r| !r.success);
    let any_diag = results.iter().any(|r| {
        !cargo_json::parse_cargo_diagnostics(&r.stdout).is_empty()
    });
    let failed = any_failed || any_diag;

    if !failed {
        // Clean: cap-lints leaves nothing to report when there are no lints.
        return Ok(());
    }

    if raw {
        for r in &results {
            if multi {
                output::error(&format!("[{}]", r.label));
            }
            output::error(&raw_clippy_text(r));
        }
        return Err(DevError::Build("clippy failed".into()));
    }

    output::error(&format_clippy_capped_multi(
        &results,
        project_root,
        limit,
        all,
        multi,
    ));
    Err(DevError::Build("clippy failed".into()))
}

/// Reconstruct cargo's terminal-style output for `--raw` mode.
///
/// With `--message-format=json` cargo no longer prints rendered
/// diagnostics to stderr - it emits them as the `rendered` field of
/// each compiler-message JSON event. `--raw` still wants the
/// terminal-style text, so concatenate the rendered fields and tack on
/// any cargo status messages on stderr (Compiling/Finished/etc).
/// Falls back to the raw streams when the parser found nothing - that's
/// the "cargo crashed and emitted non-JSON" case where the stderr / stdout
/// dump is the only useful thing left.
fn raw_clippy_text(r: &SweepResult) -> String {
    let events = cargo_json::parse_cargo_diagnostics(&r.stdout);
    let rendered: Vec<&str> = events
        .iter()
        .filter_map(|d| d.rendered.as_deref())
        .collect();

    if rendered.is_empty() {
        let mut out = String::new();
        out.push_str(&r.stderr);
        if !r.stdout.is_empty() {
            out.push_str(&r.stdout);
        }
        return out;
    }

    let mut out = String::new();
    for r in rendered {
        out.push_str(r);
        if !r.ends_with('\n') {
            out.push('\n');
        }
    }
    if !r.stderr.is_empty() {
        out.push_str(&r.stderr);
    }
    out
}

struct SweepResult {
    label: String,
    stdout: String,
    stderr: String,
    success: bool,
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

/// Multi-sweep version of the text formatter: parses each sweep's stdout
/// JSON, merges + dedups diagnostics, applies scope+limit, and tags each
/// line with its sweep label when `multi` is true. Falls back to per-sweep
/// raw streams when cargo failed but emitted no compiler-message events
/// (e.g. cargo itself crashed before reaching the diagnostic phase).
fn format_clippy_capped_multi(
    results: &[SweepResult],
    project_root: &Path,
    limit: usize,
    all: bool,
    multi: bool,
) -> String {
    let parses: Vec<(String, cargo_filter::ClippyParse)> = results
        .iter()
        .map(|r| {
            let parse = parse_clippy_from_json(&r.stdout, !r.success, true);
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
        return "cargo clippy: no issues".into();
    }

    let total_errors = merged.iter().filter(|m| m.diag.is_error).count();
    let total_warnings = merged.len() - total_errors;

    let (displayed, trailer) = if all {
        // `--all` is the bulk-triage view: sort so every hit of a single
        // lint clumps together. Errors first (more urgent), then within
        // each level by lint code, file, line, column. Cached keys keep
        // the location parsing to one pass per diagnostic.
        let mut refs: Vec<&MergedDiag<'_>> = merged.iter().collect();
        refs.sort_by_cached_key(|m| clippy_sort_key(m.diag));
        (refs, None)
    } else {
        let changed = scope::changed_files(project_root);
        let refs: Vec<&MergedDiag<'_>> = merged.iter().collect();
        let part = scope::partition(
            refs,
            |m| m.diag.path().unwrap_or_else(|| Path::new("")),
            limit,
            changed.as_ref(),
        );
        let trailer = scope::format_trailer(part.hidden_unscoped);
        (part.displayed, trailer)
    };

    let header = if multi {
        format!(
            "cargo clippy: {total_errors} errors, {total_warnings} warnings ({} sweeps)\n",
            results.len()
        )
    } else {
        format!("cargo clippy: {total_errors} errors, {total_warnings} warnings\n")
    };

    let mut out = header;
    for m in &displayed {
        out.push_str("  ");
        if multi
            && let Some(tag) = sweep_tag(&m.sweeps, results.len())
        {
            out.push_str(&tag);
            out.push(' ');
        }
        out.push_str(&m.diag.format_one());
        out.push('\n');
    }
    if let Some(t) = trailer {
        out.push_str("  ");
        out.push_str(&t);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Parse cargo's `--message-format=json` stdout into a [`ClippyParse`].
///
/// Walks each compiler-message JSON event and maps it to the formatter
/// primitive used by `merge_clippy` and `format_one()`. Diagnostics are
/// ordered errors-first, then warnings (stable within each). When cargo
/// failed and emitted no compiler-message events, sets `parse_failed` so
/// callers can fall back to dumping the raw streams.
fn parse_clippy_from_json(
    stdout: &str,
    sweep_failed: bool,
    gate: bool,
) -> cargo_filter::ClippyParse {
    let events = cargo_json::parse_cargo_diagnostics(stdout);
    let mut diagnostics: Vec<cargo_filter::ClippyDiagnostic> = events
        .iter()
        .map(|d| event_to_clippy(d, gate))
        .collect();

    // Errors first, then warnings; each half keeps discovery order.
    let (errors, warnings): (Vec<_>, Vec<_>) =
        std::mem::take(&mut diagnostics).into_iter().partition(|d| d.is_error);
    let mut sorted = errors;
    sorted.extend(warnings);

    let parse_failed = sweep_failed && sorted.is_empty();

    cargo_filter::ClippyParse {
        diagnostics: sorted,
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
fn event_to_clippy(d: &cargo_json::DiagnosticEvent, gate: bool) -> cargo_filter::ClippyDiagnostic {
    // Under the gate, every surfaced lint is a hard failure: brokkr ran clippy
    // with `--cap-lints=warn` only to complete the graph, so a diagnostic that
    // arrived at the capped `warning` level is really a deny. Restore `error`
    // for both the flag and the rendered header.
    let is_error = gate || d.level == "error";
    let level = if is_error { "error" } else { d.level.as_str() };
    let header = match &d.code {
        Some(c) => format!("{level}[{c}]"),
        None => level.to_string(),
    };
    let location = match (&d.file, d.line, d.column) {
        (Some(f), Some(l), Some(c)) => Some(format!("{f}:{l}:{c}")),
        _ => None,
    };
    let detail = extract_detail_from_event(d);
    cargo_filter::ClippyDiagnostic {
        is_error,
        header,
        location,
        message: d.message.clone(),
        detail,
    }
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

/// Sort key for `--all` bulk triage: errors before warnings, then by
/// lint code (so every hit of a rule clumps together), then file and
/// line for stable in-rule ordering. Bare `error` / `warning` headers
/// (no code) sort to the end of their level since the lint code is
/// the empty string for those.
fn clippy_sort_key(d: &cargo_filter::ClippyDiagnostic) -> (u8, String, String, u64, u64) {
    let level = if d.is_error { 0u8 } else { 1u8 };
    let lint = extract_lint_code(&d.header);
    // Push bare-level diagnostics to the end of their level by giving
    // them a key that sorts after any real code.
    let lint_key = if lint.is_empty() {
        "\u{10FFFF}".to_string()
    } else {
        lint.to_string()
    };
    let (file, line, col) = parse_location(d.location.as_deref());
    (level, lint_key, file, line, col)
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

/// Iterate `sweeps`, pre-building each sweep's `build_packages` and
/// then running `cargo test` for it. Fails fast on the first sweep
/// that fails (build or test), mirroring how the clippy phase
/// short-circuits on a non-zero status.
#[allow(clippy::too_many_arguments)]
fn run_test_phase(
    project: Option<Project>,
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    package: Option<&str>,
    raw: bool,
    doctests: bool,
    extra_args: &[String],
    mut timings: Option<&mut Vec<TestTiming>>,
) -> Result<(), DevError> {
    let multi = sweeps.len() > 1;
    // `brokkr check`'s test phase always runs `cargo test` without
    // `--release`, so each sweep's `build_packages` artefacts land in
    // `<target>/debug`. Tests that spawn the just-rebuilt binary read
    // BROKKR_TEST_BIN_DIR to skip the `cfg!(debug_assertions)` profile
    // guess (which silently lies when a workspace pins
    // `[profile.test]` overrides).
    let target_dir = build::project_info(Some(project_root))?.target_dir;
    let project_env = build_test_env(project, &target_dir, "debug");

    for sweep in sweeps {
        for pkg in &sweep.build_packages {
            run_sweep_pre_build(project_root, sweep, pkg, &project_env, raw)?;
        }

        let success = run_one_test_sweep(
            project_root,
            sweep,
            package,
            extra_args,
            &project_env,
            raw,
            doctests,
            multi,
            timings.as_deref_mut(),
        )?;
        if !success {
            return Err(DevError::Build("tests failed".into()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod scope_limit_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn style_violation(file: &str) -> crate::style::StyleViolation {
        crate::style::StyleViolation {
            file: PathBuf::from(file),
            line: 1,
            keyword: "if",
            content: String::new(),
            prev: String::new(),
        }
    }

    #[test]
    fn scope_first_retains_changed_file_violation_past_limit() {
        // One violation in a branch-changed file (`b.rs`) sorts last in file-walk
        // order, behind enough unscoped hits to overflow limit=2. Scope-first must
        // still surface it in full, capping only the unscoped overflow.
        let violations = vec![
            style_violation("a.rs"),
            style_violation("c.rs"),
            style_violation("d.rs"),
            style_violation("b.rs"),
        ];
        let changed: HashSet<PathBuf> = ["b.rs"].iter().map(PathBuf::from).collect();
        let (displayed, trailer) =
            scope_limit_with(violations, 2, false, |v| v.file.as_path(), Some(&changed));

        // Scoped `b.rs` is retained (ahead of the capped unscoped tail), 2 unscoped
        // shown, the last unscoped hidden into the trailer.
        let shown: Vec<&str> = displayed
            .iter()
            .map(|v| v.file.to_str().unwrap())
            .collect();
        assert!(shown.contains(&"b.rs"));
        assert_eq!(displayed.len(), 3); // 1 scoped + 2 unscoped
        assert_eq!(trailer.unwrap(), "+1 in unchanged files (--all to see)");
    }

    #[test]
    fn all_shows_everything_without_trailer() {
        let violations = vec![style_violation("a.rs"), style_violation("b.rs")];
        let (displayed, trailer) =
            scope_limit_with(violations, 1, true, |v| v.file.as_path(), None);
        assert_eq!(displayed.len(), 2);
        assert!(trailer.is_none());
    }
}

