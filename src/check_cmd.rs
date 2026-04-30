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
    msg.push_str("  hint: rerun with `brokkr check --fix-gremlins` to rewrite all banned chars in place\n");
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
        // Always run with --message-format=json so the lint code
        // (`message.code.code`) is populated on every diagnostic. cargo's
        // pretty-printed stderr only includes the `= note: #[warn(rule)]`
        // annotation on the first occurrence of each lint per crate,
        // which made bulk triage by rule impossible in text mode.
        let mut args: Vec<String> = vec![
            "clippy".into(),
            "--all-targets".into(),
            "--message-format=json".into(),
        ];
        args.extend(sweep.cargo_feature_args.iter().cloned());
        if let Some(pkg) = package {
            args.push("--package".into());
            args.push(pkg.into());
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
                output::error(&raw_clippy_text(r));
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
            let text = raw_clippy_text(r);
            if !text.is_empty() {
                if multi {
                    println!("[{}]", r.label);
                }
                print!("{text}");
            }
        }
        return Ok(());
    }

    // Success path: surface any warnings the parser extracted across all sweeps.
    let any_diag_or_failed = results.iter().any(|r| {
        let events = cargo_json::parse_cargo_diagnostics(&r.stdout, "clippy", None);
        let has_diag = events.iter().any(|e| matches!(e, cargo_json::CheckEvent::Diagnostic(_)));
        let parse_failed = !r.success && events.is_empty();
        has_diag || parse_failed
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
    let events = cargo_json::parse_cargo_diagnostics(&r.stdout, "clippy", None);
    let rendered: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            cargo_json::CheckEvent::Diagnostic(d) => d.rendered.as_deref(),
            _ => None,
        })
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
            let parse = parse_clippy_from_json(&r.stdout, !r.success);
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

/// Parse cargo's `--message-format=json` stdout into a [`ClippyParse`].
///
/// Walks each compiler-message JSON event and maps it to the formatter
/// primitive used by `merge_clippy` and `format_one()`. Diagnostics are
/// ordered errors-first, then warnings (stable within each). When cargo
/// failed and emitted no compiler-message events, sets `parse_failed` so
/// callers can fall back to dumping the raw streams.
fn parse_clippy_from_json(stdout: &str, sweep_failed: bool) -> cargo_filter::ClippyParse {
    let events = cargo_json::parse_cargo_diagnostics(stdout, "clippy", None);
    let mut diagnostics: Vec<cargo_filter::ClippyDiagnostic> = events
        .iter()
        .filter_map(|e| match e {
            cargo_json::CheckEvent::Diagnostic(d) => Some(event_to_clippy(d)),
            _ => None,
        })
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
fn event_to_clippy(d: &cargo_json::DiagnosticEvent) -> cargo_filter::ClippyDiagnostic {
    let is_error = d.level == "error";
    let header = match &d.code {
        Some(c) => format!("{}[{}]", d.level, c),
        None => d.level.clone(),
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
        clippy::cognitive_complexity,
        clippy::useless_vec
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

    fn json_compiler_message(
        level: &str,
        code: Option<&str>,
        message: &str,
        file: &str,
        line: u64,
        col: u64,
    ) -> String {
        let code_field = match code {
            Some(c) => format!(r#""code":{{"code":"{c}"}},"#),
            None => "\"code\":null,".to_string(),
        };
        format!(
            r#"{{"reason":"compiler-message","message":{{{code_field}"level":"{level}","message":"{message}","spans":[{{"file_name":"{file}","line_start":{line},"column_start":{col},"line_end":{line},"column_end":{col},"is_primary":true}}],"children":[],"rendered":"rendered"}}}}"#
        )
    }

    #[test]
    fn json_to_clippy_uses_code_for_every_occurrence() {
        // Regression: in cargo's pretty-printed text, only the first
        // occurrence of each lint per crate carries a `= note: #[warn(rule)]`
        // line, so the old text scraper left subsequent warnings as bare
        // `warning`. With JSON ingestion every diagnostic carries
        // `message.code.code`, so they all keep the rule in the header.
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            219,
            9,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            228,
            9,
        ));

        let parsed = parse_clippy_from_json(&input, false);
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.diagnostics.len(), 2);
        for d in &parsed.diagnostics {
            assert_eq!(d.header, "warning[clippy::collapsible_if]");
        }
    }

    #[test]
    fn json_to_clippy_uses_primary_label_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/foo.rs","line_start":20,"column_start":5,"line_end":20,"column_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert_eq!(d.header, "error[E0308]");
        assert_eq!(
            d.format_one(),
            "error[E0308] src/foo.rs:20:5 mismatched types - expected `i32`, found `&str`"
        );
    }

    #[test]
    fn json_to_clippy_falls_back_to_child_note_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/lib.rs","line_start":42,"column_start":12,"line_end":42,"column_end":15,"is_primary":true,"label":"arguments to this function are incorrect"}],"children":[{"level":"note","message":"expected reference `&Vec<u8>`\n   found reference `&Vec<i32>`","spans":[]}],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert!(
            d.format_one()
                .contains("- expected reference `&Vec<u8>`, found reference `&Vec<i32>`"),
            "got: {}",
            d.format_one()
        );
    }

    #[test]
    fn json_to_clippy_no_code_falls_back_to_bare_level() {
        // Some diagnostics lack a code (e.g. cargo-emitted notes). The
        // header degrades gracefully to bare `warning` / `error`.
        let input = json_compiler_message(
            "warning",
            None,
            "something happened",
            "src/foo.rs",
            10,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].header, "warning");
    }

    #[test]
    fn json_to_clippy_orders_errors_before_warnings() {
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::redundant_closure"),
            "redundant closure",
            "src/a.rs",
            1,
            1,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "error",
            Some("E0308"),
            "mismatched types",
            "src/b.rs",
            2,
            2,
        ));
        let parsed = parse_clippy_from_json(&input, false);
        assert_eq!(parsed.diagnostics.len(), 2);
        assert!(parsed.diagnostics[0].is_error);
        assert!(!parsed.diagnostics[1].is_error);
    }

    #[test]
    fn json_to_clippy_sets_parse_failed_when_sweep_failed_with_no_events() {
        // cargo crashed before producing any compiler-message events.
        let parsed = parse_clippy_from_json("", true);
        assert!(parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    #[test]
    fn json_to_clippy_no_parse_failed_when_sweep_succeeded() {
        // Empty stdout but successful exit (clean compile). Not a parse
        // failure - just nothing to report.
        let parsed = parse_clippy_from_json("", false);
        assert!(!parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    fn diag(header: &str, location: &str) -> cargo_filter::ClippyDiagnostic {
        cargo_filter::ClippyDiagnostic {
            is_error: header.starts_with("error"),
            header: header.to_string(),
            location: Some(location.to_string()),
            message: "msg".to_string(),
            detail: None,
        }
    }

    #[test]
    fn clippy_sort_key_orders_errors_before_warnings() {
        let warn = diag("warning[clippy::aaaa]", "src/a.rs:1:1");
        let err = diag("error[E0308]", "src/z.rs:99:99");
        assert!(clippy_sort_key(&err) < clippy_sort_key(&warn));
    }

    #[test]
    fn clippy_sort_key_groups_same_lint_together() {
        // Three warnings - two with the same lint code on different files,
        // one with a different code in between alphabetically. After sort,
        // the same-lint pair should be adjacent.
        let mut diags = vec![
            diag("warning[clippy::collapsible_if]", "src/b.rs:1:1"),
            diag("warning[clippy::needless_return]", "src/a.rs:1:1"),
            diag("warning[clippy::collapsible_if]", "src/a.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].header, "warning[clippy::collapsible_if]");
        assert_eq!(diags[1].header, "warning[clippy::collapsible_if]");
        assert_eq!(diags[2].header, "warning[clippy::needless_return]");
        // Within the same lint, file order kicks in: a.rs before b.rs.
        assert_eq!(diags[0].location.as_deref(), Some("src/a.rs:1:1"));
        assert_eq!(diags[1].location.as_deref(), Some("src/b.rs:1:1"));
    }

    #[test]
    fn clippy_sort_key_orders_lines_numerically() {
        // Same lint, same file: line 9 before line 100 (lexical sort
        // would put 100 first - check we're parsing the integer).
        let mut diags = vec![
            diag("warning[clippy::xxx]", "src/a.rs:100:1"),
            diag("warning[clippy::xxx]", "src/a.rs:9:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].location.as_deref(), Some("src/a.rs:9:1"));
        assert_eq!(diags[1].location.as_deref(), Some("src/a.rs:100:1"));
    }

    #[test]
    fn clippy_sort_key_pushes_bare_level_to_end() {
        // A bare `warning` (no code) should sort after every coded
        // warning, since there's no useful key to group it with.
        let mut diags = vec![
            diag("warning", "src/a.rs:1:1"),
            diag("warning[clippy::zzz]", "src/b.rs:1:1"),
            diag("warning[clippy::aaa]", "src/c.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].header, "warning[clippy::aaa]");
        assert_eq!(diags[1].header, "warning[clippy::zzz]");
        assert_eq!(diags[2].header, "warning");
    }

    #[test]
    fn parse_location_handles_normal_path_line_col() {
        assert_eq!(
            parse_location(Some("src/foo.rs:10:5")),
            ("src/foo.rs".to_string(), 10, 5)
        );
    }

    #[test]
    fn parse_location_handles_none() {
        assert_eq!(parse_location(None), (String::new(), 0, 0));
    }

    #[test]
    fn extract_lint_code_pulls_bracketed_name() {
        assert_eq!(extract_lint_code("warning[clippy::foo]"), "clippy::foo");
        assert_eq!(extract_lint_code("error[E0308]"), "E0308");
        assert_eq!(extract_lint_code("warning"), "");
    }
}
