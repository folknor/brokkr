// Convention scans and their findings; cargo diagnostic execution lives elsewhere.

use super::{Phase, cargo_line, fmt_wall, phase_inert, phase_ok, phase_time_left};
use crate::config::{
    DependencyRule, Diagnostics, GremlinsConfig, HeaderConfig, ManifestConfig, ScriptCheck, Stage,
    TextlintRule,
};
use crate::error::DevError;
use crate::rustc_text::{self, Block, BlockKind, Level};
use crate::{dependency_rules, globs, gremlins, output, scope};
use std::path::{Path, PathBuf};

/// One check invocation's working-tree snapshot, loaded the first time a
/// convention scanner or skipped-scope audit needs it. Lazy so a run whose
/// file scanners are skipped or unconfigured and has no selector scope to
/// audit never walks git; once loaded, every scanner judges the same file list
/// and the same bytes.
pub(super) struct LazyTree<'a> {
    root: &'a Path,
    cell: std::cell::OnceCell<scope::TreeSnapshot>,
}

impl<'a> LazyTree<'a> {
    pub(super) fn new(root: &'a Path) -> Self {
        Self {
            root,
            cell: std::cell::OnceCell::new(),
        }
    }

    pub(super) fn get(&self) -> Result<&scope::TreeSnapshot, DevError> {
        if let Some(tree) = self.cell.get() {
            return Ok(tree);
        }
        let loaded = scope::TreeSnapshot::load(self.root)?;
        Ok(self.cell.get_or_init(|| loaded))
    }
}

pub(super) fn run_gremlins(
    tree: &LazyTree<'_>,
    config: Option<&GremlinsConfig>,
    fix: bool,
) -> Result<(), DevError> {
    // `[gremlins] disable = true` skips the whole phase - both the scan and
    // `--fix-gremlins`.
    if config.is_some_and(|c| c.disable) {
        phase_ok(Phase::Gremlins.label(), Some("disabled by config".into()));
        return Ok(());
    }

    let tree = tree.get()?;
    if fix {
        let fixed = gremlins::fix(tree, config)?;
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

    let scan = gremlins::scan_report(tree, config)?;
    let found = scan.violations;

    if found.is_empty() {
        phase_ok(
            Phase::Gremlins.label(),
            Some(output::count(scan.files, "file")),
        );
        return Ok(());
    }

    let total = found.len();
    let displayed = tree_order(found, tree, |g| g.path.as_path());

    let mut msg = format!("gremlins: {total} found\n");
    for g in &displayed {
        msg.push_str("  ");
        msg.push_str(&gremlins::format_one(g));
        msg.push('\n');
    }
    msg.push_str(
        "  hint: rerun with `brokkr check --fix-gremlins`; only characters with a defined replacement are rewritten\n",
    );
    output::error(msg.trim_end());
    Err(DevError::Reported("gremlins found".into()))
}

/// Order a phase's errors for display via [`scope::prioritize`]: the errors in
/// files with unstaged changes first, then the rest. Every error is displayed -
/// there is no cap. `get_path` maps an error to its file.
///
/// The unstaged set is computed here rather than once in `cmd_check` so the git
/// call is paid only when a phase actually has errors to display. Phases fail
/// fast, so at most one of them reaches this per invocation.
pub(super) fn scope_order<T>(
    violations: Vec<T>,
    project_root: &Path,
    get_path: impl Fn(&T) -> &Path,
) -> Vec<T> {
    let unstaged = scope::unstaged_files(project_root);
    scope::prioritize(violations, get_path, unstaged.as_ref())
}

/// [`scope_order`] for a convention scan, reading the unstaged set the
/// snapshot already took rather than asking git again.
fn tree_order<T>(
    violations: Vec<T>,
    tree: &scope::TreeSnapshot,
    get_path: impl Fn(&T) -> &Path,
) -> Vec<T> {
    scope::prioritize(violations, get_path, tree.unstaged_files())
}

/// The `[header]` phase: required text anywhere in a selected file, with a current-year check.
/// Inert unless the project has a `[header]` section.
pub(super) fn run_header(
    tree: &LazyTree<'_>,
    header_cfg: Option<&HeaderConfig>,
) -> Result<(), DevError> {
    let Some(cfg) = header_cfg else {
        phase_inert(Phase::Header);
        return Ok(());
    };
    let year = crate::header::current_utc_year()?;
    let expected = crate::header::expand(&cfg.pattern, year);

    let tree = tree.get()?;
    let scan = crate::header::scan_report(tree, cfg, year)?;
    let violations = scan.violations;

    if violations.is_empty() {
        phase_ok(
            Phase::Header.label(),
            Some(output::count(scan.files, "file")),
        );
        return Ok(());
    }

    report_findings(
        "header",
        "header check failed",
        &tree_order(violations, tree, |v| v.file.as_path()),
        |v| crate::header::format_one(v, &expected),
    )
}

/// The `[[textlint]]` phase: declarative forbid-a-pattern line rules. Inert
/// unless the project defines `[[textlint]]` entries.
pub(super) fn run_textlint(tree: &LazyTree<'_>, rules: &[TextlintRule]) -> Result<(), DevError> {
    if rules.is_empty() {
        phase_inert(Phase::Textlint);
        return Ok(());
    }

    let tree = tree.get()?;
    let scan = crate::textlint::scan(tree, rules)?;
    let violations = scan.violations;

    if violations.is_empty() {
        // Counts on the green line, matching `dependency rules`: "ok" alone
        // cannot distinguish a clean tree from a rule whose `paths` glob has
        // stopped matching anything.
        phase_ok(
            Phase::Textlint.label(),
            Some(format!(
                "{}, {}",
                output::count(rules.len(), "rule"),
                output::count(scan.files, "file")
            )),
        );
        return Ok(());
    }

    report_findings(
        "textlint",
        "textlint failed",
        &tree_order(violations, tree, |v| v.file.as_path()),
        crate::textlint::format_one,
    )
}

/// The `[manifest]` phase: native structural `Cargo.toml` conventions. Inert
/// unless the project has a `[manifest]` section with at least one check on.
pub(super) fn run_manifest(
    tree: &LazyTree<'_>,
    manifest_cfg: Option<&ManifestConfig>,
) -> Result<(), DevError> {
    let Some(cfg) = manifest_cfg else {
        phase_inert(Phase::Manifest);
        return Ok(());
    };

    let tree = tree.get()?;
    let scan = crate::manifest::scan_report(tree, cfg)?;
    let violations = scan.violations;

    if violations.is_empty() {
        phase_ok(
            Phase::Manifest.label(),
            Some(output::count(scan.files, "manifest")),
        );
        return Ok(());
    }

    report_findings(
        "manifest",
        "manifest check failed",
        &tree_order(violations, tree, |v| v.file.as_path()),
        crate::manifest::format_one,
    )
}

/// The selector-hygiene findings `brokkr check --audit` reports: configured
/// paths that select nothing, and exclusions that excuse everything their
/// selector picked. Resolved against the tracked file list alone; no content
/// is scanned and no phase runs. An ordinary check reports none of this.
pub(super) fn selector_findings(
    tree: &LazyTree<'_>,
    gremlins_cfg: Option<&GremlinsConfig>,
    header_cfg: Option<&HeaderConfig>,
    textlint_rules: &[TextlintRule],
    manifest_cfg: Option<&ManifestConfig>,
) -> Result<Vec<String>, DevError> {
    let files = tree.get()?.files();
    let mut out = Vec::new();
    gremlins_scope(files, gremlins_cfg, &mut out);
    header_scope(files, header_cfg, &mut out)?;
    textlint_scopes(files, textlint_rules, &mut out)?;
    manifest_scope(files, manifest_cfg, &mut out)?;
    Ok(out)
}

fn gremlins_scope(files: &[PathBuf], config: Option<&GremlinsConfig>, out: &mut Vec<String>) {
    let Some(config) = config.filter(|config| !config.disable) else {
        return;
    };
    if config.exclude.is_empty() {
        return;
    }
    let scannable: Vec<&Path> = files
        .iter()
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(crate::globs::is_scanned_extension)
        })
        .map(PathBuf::as_path)
        .collect();
    for entry in &config.exclude {
        let dir = Path::new(entry.trim_end_matches('/'));
        if !scannable
            .iter()
            .any(|path| *path == dir || path.starts_with(dir))
        {
            out.push(format!(
                "[gremlins].exclude entry {entry:?} matches no scannable tracked path"
            ));
        }
    }
    if !scannable.iter().any(|path| {
        !config
            .exclude
            .iter()
            .any(|entry| path.starts_with(Path::new(entry.trim_end_matches('/'))))
    }) {
        out.push("[gremlins].exclude removes every scannable tracked path".to_owned());
    }
}

fn header_scope(
    files: &[PathBuf],
    config: Option<&HeaderConfig>,
    out: &mut Vec<String>,
) -> Result<(), DevError> {
    let Some(config) = config else {
        return Ok(());
    };
    let selected = select_glob_paths(files, &config.paths, "[header].paths", out)?;
    excluded_paths(
        &config.exempt,
        &selected,
        "[header].exempt",
        "[header].exempt excuses every file [header].paths selects",
        out,
    )
}

fn textlint_scopes(
    files: &[PathBuf],
    rules: &[TextlintRule],
    out: &mut Vec<String>,
) -> Result<(), DevError> {
    for rule in rules {
        let paths_field = format!("[[textlint]] {:?} paths", rule.name);
        let selected = select_glob_paths(files, &rule.paths, &paths_field, out)?;
        let exclude_field = format!("[[textlint]] {:?} exclude", rule.name);
        let all_excluded = format!(
            "[[textlint]] {:?}: exclude excuses every file paths selects",
            rule.name
        );
        excluded_paths(&rule.exclude, &selected, &exclude_field, &all_excluded, out)?;
    }
    Ok(())
}

fn manifest_scope(
    files: &[PathBuf],
    config: Option<&ManifestConfig>,
    out: &mut Vec<String>,
) -> Result<(), DevError> {
    let Some(config) = config else {
        return Ok(());
    };
    let default_paths = [String::from("**/Cargo.toml")];
    let paths = if config.paths.is_empty() {
        &default_paths[..]
    } else {
        &config.paths[..]
    };
    let candidates = select_glob_paths(files, paths, "[manifest] paths", out)?;
    let included = exclude_glob_paths(&config.exclude, &candidates, "[manifest] exclude", out)?;
    if included.is_empty() {
        out.push("[manifest] paths and exclude leave no manifest to scan".to_owned());
    }
    exclude_glob_paths(
        &config.shape_exclude,
        &included,
        "[manifest] shape_exclude",
        out,
    )?;
    Ok(())
}

fn select_glob_paths(
    paths: &[PathBuf],
    patterns: &[String],
    field: &str,
    out: &mut Vec<String>,
) -> Result<Vec<PathBuf>, DevError> {
    let matcher = globs::build_set(patterns, field)?;
    out.extend(globs::unmatched(patterns, paths, field)?);
    Ok(paths
        .iter()
        .filter(|path| globs::matches(&matcher, path))
        .cloned()
        .collect())
}

fn exclude_glob_paths(
    patterns: &[String],
    candidates: &[PathBuf],
    field: &str,
    out: &mut Vec<String>,
) -> Result<Vec<PathBuf>, DevError> {
    let matcher = globs::build_set(patterns, field)?;
    out.extend(globs::unmatched(patterns, candidates, field)?);
    Ok(candidates
        .iter()
        .filter(|path| !globs::matches(&matcher, path))
        .cloned()
        .collect())
}

fn excluded_paths(
    patterns: &[String],
    candidates: &[PathBuf],
    field: &str,
    all_excluded: &str,
    out: &mut Vec<String>,
) -> Result<(), DevError> {
    let remaining = exclude_glob_paths(patterns, candidates, field, out)?;
    if !candidates.is_empty() && remaining.is_empty() {
        out.push(all_excluded.to_owned());
    }
    Ok(())
}

/// The shared finding block for file convention scans. Scanners own the
/// finding type and green counts; this owns the uncapped display and failure.
fn report_findings<T>(
    phase: &str,
    failure: &str,
    findings: &[T],
    render: impl Fn(&T) -> String,
) -> Result<(), DevError> {
    let mut msg = format!("{phase}: {}\n", output::count(findings.len(), "violation"));
    for finding in findings {
        msg.push_str("  ");
        msg.push_str(&render(finding));
        msg.push('\n');
    }
    output::error(msg.trim_end());
    Err(DevError::Reported(failure.into()))
}

/// How much of the phase ceiling a script-check entry leaves unspent: room for
/// the kill, the drain and the failure report before the phase watchdog would
/// fire on top of it.
const SCRIPT_CHECK_MARGIN: std::time::Duration = std::time::Duration::from_secs(10);

/// The `[[script_check]]` phase for one [`Stage`]: run the configured commands
/// that named this stage and assert each one's output matches its sentinel.
/// Inert when no entry sits at this stage. Every check runs (failures are
/// collected, not fail-fast) so one `brokkr check` surfaces all broken gates at
/// once. The command's exit code is ignored - only the output match decides
/// pass/fail; a spawn failure is a hard error, and an entry that outlives its
/// deadline fails as that entry. See [`crate::script_check`].
pub(super) fn run_script_checks(
    project_root: &Path,
    checks: &[ScriptCheck],
    stage: Stage,
) -> Result<(), DevError> {
    let checks: Vec<&ScriptCheck> = checks.iter().filter(|c| c.stage == stage).collect();
    if checks.is_empty() {
        phase_inert(Phase::ScriptCheck);
        return Ok(());
    }

    let total = checks.len();
    let mut failures: Vec<(&ScriptCheck, crate::script_check::Outcome)> = Vec::new();
    for check in checks {
        // Each entry gets what is left of the phase's ceiling, less a margin
        // for reporting, so a hung script is killed and fails as *that entry*
        // - its output shown, the rest of the stage still run - rather than
        // letting the phase watchdog end the whole run. Outside a phase clock
        // (`check --script NAME`) the phase ceiling itself is the bound.
        let budget = phase_time_left().unwrap_or_else(|| Phase::ScriptCheck.ceiling());
        let deadline = budget
            .saturating_sub(SCRIPT_CHECK_MARGIN)
            .max(std::time::Duration::from_secs(1));
        let outcome = crate::script_check::run_one(check, project_root, deadline)?;
        if !outcome.passed {
            failures.push((check, outcome));
        }
    }
    report_script_stage(total, &failures, "check")
}

/// Report one script-check stage from its failures: the stage's one green
/// line, or the failure block. `noun` names what was counted (`check`,
/// `background check`).
pub(super) fn report_script_stage(
    total: usize,
    failures: &[(&ScriptCheck, crate::script_check::Outcome)],
    noun: &str,
) -> Result<(), DevError> {
    // One line for the stage, not one per check: a passing gate's name carries
    // no information a reader can act on, and a project with twenty of them
    // buried the rest of the run under a wall of `ok`. The count is what makes
    // the line falsifiable - it says which corpus passed, so a stage that
    // silently stopped running its checks is visible as a shrinking number.
    // Failures are unaffected; each one still prints its captured output below.
    if failures.is_empty() {
        phase_ok(Phase::ScriptCheck.label(), Some(output::count(total, noun)));
        return Ok(());
    }
    if failures.len() < total {
        output::run_msg(&format!(
            "script-check: {} of {total} ok",
            total - failures.len()
        ));
    }

    let mut msg = format!("script-check: {} failed\n", failures.len());
    for (check, outcome) in failures {
        msg.push_str("  ");
        msg.push_str(&check.name);
        match outcome.timed_out {
            Some(deadline) => msg.push_str(&format!(
                ": killed after {} (its share of the script_check phase ceiling), output so far:\n",
                fmt_wall(deadline)
            )),
            None if outcome.capture_incomplete => msg.push_str(
                ": output capture incomplete (a writer kept a pipe open past the drain grace), output so far:\n",
            ),
            None if relevant_truncated(check.stream, outcome) => msg.push_str(&format!(
                ": {} exceeded the capture limit, so the match cannot be proven; retained prefix:\n",
                stream_label(check.stream)
            )),
            None => msg.push_str(&format!(
                ": {} did not match {:?}\n",
                stream_label(check.stream),
                check.expect
            )),
        }
        append_script_failure(&mut msg, check, outcome);
    }
    output::error(msg.trim_end());

    Err(DevError::Reported("script-check failed".into()))
}

/// Render one failing script-check's captured output.
///
/// The captured stream is the diagnostic here. Brokkr never saw the command's
/// internals, only what it printed. The rendering selects a useful view and
/// redacts recognized credentials and the current home path; matching still
/// uses the original bytes. Two paths:
///
/// - `diagnostics = "rustc"` with at least one error finding: the error
///   findings alone, with a trailer counting the warning findings withheld.
///   Measured on the consuming config (a `cargo doc --workspace` gate): 27 denied
///   `broken-intra-doc-links` errors against several hundred
///   `private-intra-doc-links` warnings from every other crate - a 1:20
///   signal-to-noise ratio, with the signal uniformly at `error`. Printing the
///   failing level alone fit it in a third of a screen.
/// - Everything else: both streams with recognized credentials and the
///   current home path redacted for display.
///
/// A `rustc` entry with no error block falls through to the opaque view rather
/// than printing an empty section: a gate can fail because its sentinel never
/// appeared at all (the command died, or was stubbed), and that failure's
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
            ("stdout", rustc_text::rustc_blocks(&stdout)),
            ("stderr", rustc_text::rustc_blocks(&stderr)),
        ];
        if streams.iter().any(|(_, blocks)| {
            blocks.iter().any(|b| {
                b.level == Level::Error && rustc_text::classify_block(b) == BlockKind::Finding
            })
        }) {
            append_rustc_errors(msg, &streams);
            return;
        }
    }
    append_captured_stream(msg, "stdout", &outcome.stdout);
    if outcome.stdout_truncated {
        msg.push_str("    (stdout truncated at the capture limit)\n");
    }
    append_captured_stream(msg, "stderr", &outcome.stderr);
    if outcome.stderr_truncated {
        msg.push_str("    (stderr truncated at the capture limit)\n");
    }
}

/// Whether the stream(s) a check matches against overran the capture limit.
fn relevant_truncated(
    stream: crate::config::Stream,
    outcome: &crate::script_check::Outcome,
) -> bool {
    match stream {
        crate::config::Stream::Stdout => outcome.stdout_truncated,
        crate::config::Stream::Stderr => outcome.stderr_truncated,
        crate::config::Stream::Both => outcome.stdout_truncated || outcome.stderr_truncated,
    }
}

/// Append every error finding of a rustc-shaped failure, then one trailer
/// counting the warning findings that were not printed.
///
/// The trailer exists because the omission is a choice brokkr made, not one the
/// command made: a reader who sees only errors must be told the warnings were
/// there, or a `warning:`-shaped cause reads as absent rather than withheld.
fn append_rustc_errors(msg: &mut String, streams: &[(&str, Vec<Block>)]) {
    let mut warnings = 0usize;
    for (label, blocks) in streams {
        let errors: Vec<&Block> = blocks
            .iter()
            .filter(|b| {
                b.level == Level::Error && rustc_text::classify_block(b) == BlockKind::Finding
            })
            .collect();
        warnings += blocks
            .iter()
            .filter(|b| {
                b.level == Level::Warning && rustc_text::classify_block(b) == BlockKind::Finding
            })
            .count();
        if errors.is_empty() {
            continue;
        }
        msg.push_str(&format!("    --- {label} (errors) ---\n"));
        for block in errors {
            let text = crate::history::redact_secret_text(&block.text);
            for line in text.lines() {
                msg.push_str("    ");
                msg.push_str(line);
                msg.push('\n');
            }
        }
    }

    if warnings > 0 {
        msg.push_str(&format!(
            "    {} not shown\n",
            output::count(warnings, "warning")
        ));
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
/// An opaque check's verdict is typically its LAST line (that is what
/// `last-line` matches), while a command's fatal error is typically near its
/// first. Keep every captured line and redact likely credentials and the
/// current home path only while rendering; matching already used the original
/// output.
fn append_captured_stream(msg: &mut String, label: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(bytes);
    let text = crate::history::redact_secret_text(&text);
    msg.push_str(&format!("    --- {label} ---\n"));
    for line in text.lines() {
        msg.push_str("    ");
        msg.push_str(line);
        msg.push('\n');
    }
}

pub(super) fn run_dependency_rules(
    project_root: &Path,
    rules: &[DependencyRule],
    commands: bool,
) -> Result<(), DevError> {
    if rules.is_empty() {
        phase_inert(Phase::DependencyRules);
        return Ok(());
    }

    // The displayed argv is the query's own construction, so it cannot drift
    // from the loader. The loader memoizes per invocation, so the line names
    // the query the phase reads, which another phase may already have run.
    cargo_line(
        commands,
        &format!(
            "{} (dependency rules)",
            crate::cargo::metadata::Query::workspace().display()
        ),
    );
    let report = dependency_rules::check(project_root, rules)?;

    if report.violations.is_empty() {
        phase_ok(
            Phase::DependencyRules.label(),
            Some(format!(
                "{}, {}",
                output::count(report.rules, "rule"),
                output::count(report.packages, "workspace package"),
            )),
        );
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

    Err(DevError::Reported("dependency rules failed".into()))
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
pub(super) fn run_publish_cycle(project_root: &Path, commands: bool) -> Result<(), DevError> {
    // See `run_dependency_rules`: the argv comes from the query itself.
    cargo_line(
        commands,
        &format!(
            "{} (publish cycle)",
            crate::cargo::metadata::Query::workspace().display()
        ),
    );
    let cycles = crate::deps::publication_cycles(project_root)?;

    if cycles.is_empty() {
        phase_ok(Phase::PublishCycle.label(), None);
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

    Err(DevError::Reported("publish cycle failed".into()))
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
            timed_out: None,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            capture_incomplete: false,
        }
    }

    #[test]
    fn a_truncated_stream_is_marked() {
        let mut msg = String::new();
        let mut truncated = outcome("partial\n", "");
        truncated.stdout_truncated = true;
        append_script_failure(&mut msg, &check(Diagnostics::Opaque), &truncated);
        assert!(msg.contains("partial"));
        assert!(msg.contains("stdout truncated at the capture limit"));
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
        s.push_str(
            "error[E0432]: unresolved import `crate::nope`\n  --> crates/a/src/lib.rs:1:5\n",
        );
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
    fn rustc_without_an_error_block_falls_back_to_the_opaque_view() {
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
        append_script_failure(
            &mut msg,
            &check(Diagnostics::Opaque),
            &outcome("a\nb\nc\n", ""),
        );
        assert!(!msg.contains("hidden"));
        for line in ["a", "b", "c"] {
            assert!(msg.contains(line));
        }
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
        // A test-owned directory: whatever git can or cannot say about it,
        // ordering never drops an error.
        let root = crate::test_scratch::scratch("check_cmd_conventions", "not_a_repo");
        let displayed = scope_order(violations, &root, PathBuf::as_path);
        assert_eq!(displayed.len(), 2);
    }
}
