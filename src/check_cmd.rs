//! Implementation of the `check` command (clippy + tests).
//!
//! Both phases iterate the same list of "active sweeps" - each one a
//! cargo invocation with a specific feature flag set, optional
//! pre-built binary packages, and (for tests) optional libtest
//! filters. The list is built once at the top of `cmd_check` from
//! whichever of these inputs apply, in priority order:
//!
//! 1. CLI `--features` / `--no-default-features` flags → a single
//!    ad-hoc sweep that ignores `[[check]]` and any profile.
//! 2. CLI `--profile <name>` or `[test].default_profile` → the
//!    profile's resolved sweeps (each backed by a `[[check]]` entry,
//!    plus the profile's libtest filters).
//! 3. `[[check]]` entries are configured but no profile applies →
//!    every entry runs in declaration order with no libtest filters.
//! 4. None of the above → a single `--all-features` sweep, matching
//!    `brokkr check`'s pre-`[[check]]` behaviour for projects that
//!    haven't migrated.

use std::collections::HashMap;
use std::path::Path;

use crate::build;
use crate::cargo_filter;
use crate::cargo_json;
use crate::config::{CheckEntry, TestConfig};
use crate::error::DevError;
use crate::gremlins;
use crate::output;
use crate::profile::{self, ResolvedSweep};
use crate::project::Project;
use crate::scope;

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_check(
    project: Option<Project>,
    project_root: &Path,
    check_entries: &[CheckEntry],
    test_cfg: Option<&TestConfig>,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    profile_name: Option<&str>,
    raw: bool,
    json: bool,
    limit: usize,
    all: bool,
    fix_gremlins: bool,
    extra_args: &[String],
) -> Result<(), DevError> {
    let active_sweeps =
        decide_active_sweeps(check_entries, test_cfg, profile_name, features, no_default_features)?;

    run_gremlins(project_root, json, limit, all, fix_gremlins)?;
    run_clippy_phase(project_root, &active_sweeps, package, raw, json, limit, all)?;
    run_test_phase(project, project_root, &active_sweeps, package, raw, json, extra_args)?;
    if !json {
        output::result_msg("check passed");
    }
    Ok(())
}

/// Build the list of sweeps both phases iterate, applying the
/// priority ladder documented at the top of the file.
///
/// Returns `Err` only when the user asked for a `--profile` that
/// doesn't resolve. Every other branch always succeeds with at least
/// one sweep.
fn decide_active_sweeps(
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
            libtest_args: Vec::new(),
            cargo_test_filters: Vec::new(),
            name_filters: Vec::new(),
            env: std::collections::BTreeMap::new(),
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
    //    invocation, matching pre-redesign behaviour.
    Ok(vec![ResolvedSweep {
        label: "default".into(),
        cargo_feature_args: vec!["--all-features".into()],
        build_packages: Vec::new(),
        libtest_args: Vec::new(),
        cargo_test_filters: Vec::new(),
        name_filters: Vec::new(),
        env: std::collections::BTreeMap::new(),
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
    json: bool,
    limit: usize,
    all: bool,
    fix: bool,
) -> Result<(), DevError> {
    if fix {
        let fixed = gremlins::fix(project_root)?;
        if !json {
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
    }

    let found = gremlins::scan(project_root)?;

    if json {
        for g in &found {
            cargo_json::emit(&cargo_json::CheckEvent::Gremlin(
                cargo_json::GremlinEvent {
                    file: g.path.display().to_string(),
                    line: g.line,
                    column: g.column,
                    codepoint: format!("U+{:04X}", g.codepoint),
                    name: g.name,
                },
            ));
        }
        let status = if found.is_empty() { "ok" } else { "failed" };
        cargo_json::emit(&cargo_json::CheckEvent::GremlinSummary(
            cargo_json::GremlinSummaryEvent {
                status,
                found: found.len(),
            },
        ));
        if !found.is_empty() {
            return Err(DevError::Build("gremlins found".into()));
        }
        return Ok(());
    }

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
        let trailer = scope::format_trailer(part.hidden_scoped, part.hidden_unscoped);
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
    output::error(msg.trim_end());
    Err(DevError::Build("gremlins found".into()))
}

#[allow(clippy::too_many_arguments)]
fn run_clippy_phase(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    package: Option<&str>,
    raw: bool,
    json: bool,
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
    let multi = sweeps.len() > 1;

    let mut results: Vec<SweepResult> = Vec::with_capacity(sweeps.len());
    for sweep in sweeps {
        let mut args: Vec<String> = vec!["clippy".into(), "--all-targets".into()];
        args.extend(sweep.cargo_feature_args.iter().cloned());
        if let Some(pkg) = package {
            args.push("--package".into());
            args.push(pkg.into());
        }
        if json {
            args.push("--message-format=json".into());
        }

        if !json {
            output::run_msg(&format!("cargo {}", args.join(" ")));
        }

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let captured = output::run_captured("cargo", &arg_refs, project_root)?;
        results.push(SweepResult {
            label: sweep.label.clone(),
            stdout: String::from_utf8_lossy(&captured.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
            success: captured.status.success(),
        });
    }

    let any_failed = results.iter().any(|r| !r.success);

    if json {
        emit_json_clippy(&results);
        if any_failed {
            return Err(DevError::Build("clippy failed".into()));
        }
        return Ok(());
    }

    if any_failed {
        if raw {
            for r in &results {
                if multi {
                    output::error(&format!("[{}]", r.label));
                }
                output::error(&r.stderr);
            }
        } else {
            output::error(&format_clippy_capped_multi(
                &results,
                project_root,
                limit,
                all,
                multi,
            ));
        }
        return Err(DevError::Build("clippy failed".into()));
    }

    if raw {
        for r in &results {
            if !r.stderr.is_empty() {
                if multi {
                    println!("[{}]", r.label);
                }
                print!("{}", r.stderr);
            }
        }
        return Ok(());
    }

    // Success path: surface any warnings the parser extracted across all sweeps.
    let any_diag_or_failed = results.iter().any(|r| {
        let p = cargo_filter::parse_clippy(&r.stderr);
        !p.diagnostics.is_empty() || p.parse_failed
    });
    if any_diag_or_failed {
        output::warn(&format_clippy_capped_multi(
            &results,
            project_root,
            limit,
            all,
            multi,
        ));
    }

    Ok(())
}

struct SweepResult {
    label: String,
    #[allow(dead_code)] // text path doesn't read stdout; JSON path does
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

fn sweep_tag(sweeps: &[String]) -> Option<String> {
    match sweeps.len() {
        0 => None,
        1 => Some(format!("[{}]", sweeps[0])),
        2 => Some("[both]".to_string()),
        _ => Some(format!("[{}]", sweeps.join("+"))),
    }
}

/// Multi-sweep version of the text formatter: parses each sweep's stderr,
/// merges + dedups diagnostics, applies scope+limit, and tags each line
/// with its sweep label when `multi` is true. Falls back to per-sweep raw
/// stderr when any sweep's parse failed.
fn format_clippy_capped_multi(
    results: &[SweepResult],
    project_root: &Path,
    limit: usize,
    all: bool,
    multi: bool,
) -> String {
    let parses: Vec<(String, cargo_filter::ClippyParse)> = results
        .iter()
        .map(|r| (r.label.clone(), cargo_filter::parse_clippy(&r.stderr)))
        .collect();

    // Any sweep with parse_failed: fall back to raw aggregated stderr.
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
        (merged.iter().collect::<Vec<_>>(), None)
    } else {
        let changed = scope::changed_files(project_root);
        let refs: Vec<&MergedDiag<'_>> = merged.iter().collect();
        let part = scope::partition(
            refs,
            |m| m.diag.path().unwrap_or_else(|| Path::new("")),
            limit,
            changed.as_ref(),
        );
        let trailer = scope::format_trailer(part.hidden_scoped, part.hidden_unscoped);
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
            && let Some(tag) = sweep_tag(&m.sweeps)
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

/// JSON path: parse each sweep, dedup events across sweeps merging the
/// `sweeps` field, emit deduped diagnostics, then one summary per sweep.
fn emit_json_clippy(results: &[SweepResult]) {
    // Per-sweep parse + sweep-tagged events, plus per-sweep counts.
    let multi = results.len() > 1;
    let mut all_events: Vec<cargo_json::CheckEvent> = Vec::new();
    let mut per_sweep_counts: Vec<(String, usize, usize, bool)> =
        Vec::with_capacity(results.len());

    for r in results {
        let label_for_tag = if multi { Some(r.label.as_str()) } else { None };
        let events = cargo_json::parse_cargo_diagnostics(&r.stdout, "clippy", label_for_tag);
        let mut errors = 0usize;
        let mut warnings = 0usize;
        for e in &events {
            if let cargo_json::CheckEvent::Diagnostic(d) = e {
                match d.level.as_str() {
                    "error" => errors += 1,
                    "warning" => warnings += 1,
                    _ => {}
                }
            }
        }
        if events.is_empty() && !r.success {
            cargo_json::emit_parse_error("clippy", &r.stdout, &r.stderr);
            errors += 1;
        }
        per_sweep_counts.push((r.label.clone(), errors, warnings, r.success));
        all_events.extend(events);
    }

    // Dedup Diagnostic events by (level, code, file, line, column, message),
    // merging the `sweeps` field. Non-Diagnostic events pass through unchanged.
    let mut order: Vec<JsonDiagKey> = Vec::new();
    let mut by_key: HashMap<JsonDiagKey, cargo_json::DiagnosticEvent> = HashMap::new();
    let mut other: Vec<cargo_json::CheckEvent> = Vec::new();

    for e in all_events {
        match e {
            cargo_json::CheckEvent::Diagnostic(d) => {
                let key = JsonDiagKey::from(&d);
                if let Some(existing) = by_key.get_mut(&key) {
                    for s in &d.sweeps {
                        if !existing.sweeps.contains(s) {
                            existing.sweeps.push(s.clone());
                        }
                    }
                } else {
                    order.push(key.clone());
                    by_key.insert(key, d);
                }
            }
            other_event => other.push(other_event),
        }
    }

    for k in order {
        if let Some(d) = by_key.remove(&k) {
            cargo_json::emit(&cargo_json::CheckEvent::Diagnostic(d));
        }
    }
    for e in other {
        cargo_json::emit(&e);
    }

    for (label, errors, warnings, success) in per_sweep_counts {
        let status = if success { "ok" } else { "failed" };
        cargo_json::emit(&cargo_json::CheckEvent::DiagnosticSummary(
            cargo_json::DiagnosticSummaryEvent {
                tool: "clippy",
                sweep: if multi { Some(label) } else { None },
                status,
                errors,
                warnings,
            },
        ));
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct JsonDiagKey {
    level: String,
    code: Option<String>,
    file: Option<String>,
    line: Option<u64>,
    column: Option<u64>,
    message: String,
}

impl From<&cargo_json::DiagnosticEvent> for JsonDiagKey {
    fn from(d: &cargo_json::DiagnosticEvent) -> Self {
        JsonDiagKey {
            level: d.level.clone(),
            code: d.code.clone(),
            file: d.file.clone(),
            line: d.line,
            column: d.column,
            message: d.message.clone(),
        }
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
    json: bool,
    extra_args: &[String],
) -> Result<(), DevError> {
    let multi = sweeps.len() > 1;
    // `brokkr check`'s test phase always runs `cargo test` without
    // `--release`, so each sweep's `build_packages` artefacts land in
    // `<target>/debug`. Tests that spawn the just-rebuilt binary read
    // this var to skip the `cfg!(debug_assertions)` profile guess -
    // which silently lies when a workspace pins `[profile.test]`
    // overrides.
    let bin_dir = build::project_info(Some(project_root))?
        .target_dir
        .join("debug");
    let bin_dir_string = bin_dir.to_string_lossy().into_owned();
    let mut project_env: Vec<(&str, &str)> = Vec::new();
    for &(k, v) in &project_env_pairs(project) {
        project_env.push((k, v));
    }
    project_env.push(("BROKKR_TEST_BIN_DIR", &bin_dir_string));

    for sweep in sweeps {
        for pkg in &sweep.build_packages {
            run_sweep_pre_build(project_root, sweep, pkg, &project_env, raw, json)?;
        }

        let success = run_one_test_sweep(
            project_root,
            sweep,
            package,
            extra_args,
            &project_env,
            raw,
            json,
            multi,
        )?;
        if !success {
            return Err(DevError::Build("tests failed".into()));
        }
    }

    Ok(())
}

fn project_env_pairs(project: Option<Project>) -> Vec<(&'static str, &'static str)> {
    match project {
        Some(Project::Nidhogg) => vec![("CARGO_TARGET_TMPDIR", "target/tmp")],
        _ => Vec::new(),
    }
}

/// Build one binary package with the sweep's feature flags. Errors
/// surface compile failures the same way the test phase does: filter
/// the stderr through `cargo_filter::filter_clippy` (or pass it
/// through raw). JSON mode emits a `parse_error` synthetic Diagnostic.
fn run_sweep_pre_build(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: &str,
    project_env: &[(&str, &str)],
    raw: bool,
    json: bool,
) -> Result<(), DevError> {
    let mut args: Vec<String> = vec!["build".into()];
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    args.push("--package".into());
    args.push(package.into());

    if !json {
        output::run_msg(&format!(
            "cargo {} (sweep build: {})",
            args.join(" "),
            sweep.label
        ));
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;

    if captured.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if json {
        cargo_json::emit_parse_error("test-build", &stdout, &stderr);
    } else if raw {
        if !stderr.is_empty() {
            output::error(&stderr);
        }
    } else {
        output::error(&cargo_filter::filter_clippy(&stderr));
    }
    Err(DevError::Build(format!(
        "build failed for package '{package}' in sweep '{}'",
        sweep.label
    )))
}

/// Run one cargo test invocation for the given sweep. Returns
/// `Ok(true)` on pass, `Ok(false)` on test failure (already reported),
/// `Err(...)` on subprocess spawn failure. `multi` controls whether
/// the `cargo ... (sweep: <label>)` log line carries the suffix - in
/// single-sweep mode (legacy `--all-features` path or one [[check]]
/// entry) the label noise is unhelpful.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn run_one_test_sweep(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: Option<&str>,
    extra_args: &[String],
    project_env: &[(&str, &str)],
    raw: bool,
    json: bool,
    multi: bool,
) -> Result<bool, DevError> {
    let mut args: Vec<String> = vec!["test".into()];
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    if let Some(pkg) = package {
        args.push("--package".into());
        args.push(pkg.into());
    }
    for f in &sweep.cargo_test_filters {
        args.push(f.clone());
    }
    if json {
        args.push("--message-format=json".into());
    }

    // Anything that has to land after `--` is libtest's. Pass-through
    // `extra_args` go after libtest's flags so the user's last word
    // wins (e.g. their own `--test-threads=N` overriding the profile).
    let needs_separator = !sweep.libtest_args.is_empty()
        || !sweep.name_filters.is_empty()
        || !extra_args.is_empty();
    if needs_separator {
        args.push("--".into());
        for s in &sweep.libtest_args {
            args.push(s.clone());
        }
        for n in &sweep.name_filters {
            args.push(n.clone());
        }
        for e in extra_args {
            args.push(e.clone());
        }
    }

    if !json {
        let line = if multi {
            format!("cargo {} (sweep: {})", args.join(" "), sweep.label)
        } else {
            format!("cargo {}", args.join(" "))
        };
        output::run_msg(&line);
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    if json {
        let label_for_tag = if multi { Some(sweep.label.as_str()) } else { None };
        emit_json_test_sweep(label_for_tag, &stdout, &stderr, captured.status.success());
        return Ok(captured.status.success());
    }

    if !captured.status.success() {
        if raw {
            if !stderr.is_empty() {
                output::error(&stderr);
            }
            if !stdout.is_empty() {
                output::error(&stdout);
            }
        } else {
            output::error(&cargo_filter::filter_test(&stdout, &stderr));
        }
        return Ok(false);
    }

    if raw {
        if !stderr.is_empty() {
            print!("{stderr}");
        }
        if !stdout.is_empty() {
            print!("{stdout}");
        }
    } else {
        let filtered = cargo_filter::filter_clippy(&stderr);
        if filtered != "cargo clippy: no issues" {
            let relabeled = filtered.replacen("cargo clippy:", "cargo test:", 1);
            output::warn(&relabeled);
        }
    }
    Ok(true)
}

/// JSON path for one test invocation. `sweep_label` is `Some(name)`
/// in multi-sweep runs so downstream consumers can split per-sweep
/// counts; `None` collapses to the legacy single-sweep shape.
fn emit_json_test_sweep(sweep_label: Option<&str>, stdout: &str, stderr: &str, success: bool) {
    let mut json_lines: Vec<&str> = Vec::new();
    let mut test_lines: Vec<&str> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with('{') {
            json_lines.push(line);
        } else {
            test_lines.push(line);
        }
    }

    let json_text = json_lines.join("\n");
    let diag_events = cargo_json::parse_cargo_diagnostics(&json_text, "test", sweep_label);
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for event in &diag_events {
        if let cargo_json::CheckEvent::Diagnostic(d) = event {
            match d.level.as_str() {
                "error" => errors += 1,
                "warning" => warnings += 1,
                _ => {}
            }
        }
        cargo_json::emit(event);
    }
    if errors > 0 || warnings > 0 {
        let diag_status = if errors > 0 { "failed" } else { "ok" };
        cargo_json::emit(&cargo_json::CheckEvent::DiagnosticSummary(
            cargo_json::DiagnosticSummaryEvent {
                tool: "test",
                sweep: sweep_label.map(str::to_owned),
                status: diag_status,
                errors,
                warnings,
            },
        ));
    }

    let parsed = cargo_filter::parse_test_output(&test_lines);
    for f in &parsed.failures {
        cargo_json::emit(&cargo_json::CheckEvent::TestFailure(
            cargo_json::TestFailureEvent {
                name: f.name.clone(),
                location: f.location.clone(),
                message: f.message.clone(),
            },
        ));
    }

    if parsed.failures.is_empty() && diag_events.is_empty() && !success {
        cargo_json::emit_parse_error("test", stdout, stderr);
    }

    if parsed.suites > 0 {
        let test_status = if parsed.failed > 0 { "failed" } else { "ok" };
        cargo_json::emit(&cargo_json::CheckEvent::TestSummary(
            cargo_json::TestSummaryEvent {
                status: test_status,
                sweep: sweep_label.map(str::to_owned),
                passed: parsed.passed,
                failed: parsed.failed,
                ignored: parsed.ignored,
                filtered_out: parsed.filtered_out,
                suites: parsed.suites,
                duration_seconds: parsed.duration.map(|d| (d * 100.0).round() / 100.0),
            },
        ));
    }
}

/// Combine the sweep's profile-defined env with the project's
/// always-set vars (e.g. nidhogg's `CARGO_TARGET_TMPDIR`). Sweep
/// values come first; project values append (so a sweep can shadow a
/// project default if it really needs to).
fn merged_env(
    sweep_env: &std::collections::BTreeMap<String, String>,
    project_env: &[(&str, &str)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        sweep_env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for &(k, v) in project_env {
        if !out.iter().any(|(ek, _)| ek == k) {
            out.push((k.to_owned(), v.to_owned()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity
    )]
    use super::*;

    #[test]
    fn decide_active_sweeps_legacy_default_when_nothing_configured() {
        let sweeps = decide_active_sweeps(&[], None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--all-features"]);
        assert!(sweeps[0].build_packages.is_empty());
        assert!(sweeps[0].libtest_args.is_empty());
    }

    #[test]
    fn decide_active_sweeps_cli_features_create_ad_hoc() {
        // --features commands → single ad-hoc sweep, ignores `[[check]]`
        // and any profile entirely.
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            None,
            None,
            &["commands".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--features", "commands"]);
        // No build_packages on ad-hoc - the user is spot-checking.
        assert!(sweeps[0].build_packages.is_empty());
    }

    #[test]
    fn decide_active_sweeps_no_default_features_alone_is_ad_hoc() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
        }];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], true).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--no-default-features"]);
    }

    #[test]
    fn decide_active_sweeps_check_entries_no_profile() {
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["a".into(), "b".into()],
                no_default_features: false,
                build_packages: vec!["pbfhogg-cli".into()],
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
            },
        ];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--features", "a,b"]);
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert!(sweeps[0].libtest_args.is_empty());
        assert_eq!(sweeps[1].label, "consumer");
    }

    #[test]
    fn decide_active_sweeps_default_profile_when_no_explicit() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["tier2::"]
include_ignored = false
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].libtest_args, vec!["--skip", "tier2::"]);
    }

    #[test]
    fn decide_active_sweeps_explicit_profile_overrides_default() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]

[profiles.full]
sweeps = ["all"]
include_ignored = true
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), Some("full"), &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert!(sweeps[0].libtest_args.contains(&"--include-ignored".into()));
    }

    #[test]
    fn decide_active_sweeps_profile_without_test_section_errors() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
        }];
        let err = decide_active_sweeps(&entries, None, Some("tier1"), &[], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--profile tier1"), "got: {err}");
    }

    #[test]
    fn sweep_tag_formats() {
        assert_eq!(sweep_tag(&[]), None);
        assert_eq!(sweep_tag(&["consumer".into()]), Some("[consumer]".into()));
        assert_eq!(
            sweep_tag(&["all-features".into(), "consumer".into()]),
            Some("[both]".into())
        );
    }

    #[test]
    fn merge_clippy_dedups_and_combines_sweeps() {
        let stderr_a = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: y [needless_pass_by_value]
 --> src/bar.rs:2:1
  |
";
        let stderr_b = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: z [too_many_lines]
 --> src/baz.rs:3:1
  |
";
        let parses = vec![
            ("all-features".to_owned(), cargo_filter::parse_clippy(stderr_a)),
            ("consumer".to_owned(), cargo_filter::parse_clippy(stderr_b)),
        ];
        let merged = merge_clippy(&parses);
        // 3 unique diagnostics: foo (both), bar (a), baz (b).
        assert_eq!(merged.len(), 3);
        let foo = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("foo.rs"))
            .unwrap();
        assert_eq!(foo.sweeps, vec!["all-features", "consumer"]);
        let bar = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("bar.rs"))
            .unwrap();
        assert_eq!(bar.sweeps, vec!["all-features"]);
        let baz = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("baz.rs"))
            .unwrap();
        assert_eq!(baz.sweeps, vec!["consumer"]);
    }
}
