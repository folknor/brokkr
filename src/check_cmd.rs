//! Implementation of the `check` command (clippy + tests).
//!
//! Clippy is run as one or more "sweeps" - each sweep is a cargo invocation
//! with a specific feature set. Default behavior on a project with no
//! `[check]` config is a single `--all-features` sweep (today's behavior).
//! When `[check] consumer_features = [...]` is set, a second sweep runs with
//! `--no-default-features --features <consumer_features>` so feature-gated
//! proc-macro expansions can't silently mask lints library consumers see.
//! User-supplied `--features` / `--no-default-features` short-circuit to a
//! single sweep with their flags.

use std::collections::HashMap;
use std::path::Path;

use crate::cargo_filter;
use crate::cargo_json;
use crate::config::CheckConfig;
use crate::error::DevError;
use crate::gremlins;
use crate::output;
use crate::project::Project;
use crate::scope;

/// One clippy invocation. `label` is the sweep tag surfaced in text and JSON
/// output (`"all-features"`, `"consumer"`, or `"default"` for single-sweep
/// runs where no label is meaningful).
struct Sweep {
    label: &'static str,
    feature_args: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_check(
    project: Option<Project>,
    project_root: &Path,
    check_cfg: Option<&CheckConfig>,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    raw: bool,
    json: bool,
    limit: usize,
    all: bool,
    extra_args: &[String],
) -> Result<(), DevError> {
    run_gremlins(project_root, json, limit, all)?;
    run_clippy(
        project_root,
        check_cfg,
        features,
        no_default_features,
        package,
        raw,
        json,
        limit,
        all,
    )?;
    run_tests(
        project,
        project_root,
        features,
        no_default_features,
        package,
        raw,
        json,
        extra_args,
    )?;
    if !json {
        output::result_msg("check passed");
    }
    Ok(())
}

fn run_gremlins(
    project_root: &Path,
    json: bool,
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
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

/// Decide which clippy sweeps to run.
///
/// - User-supplied `--features` or `--no-default-features` → single sweep
///   with those flags (label `"default"` since the user is overriding).
/// - Otherwise → `--all-features` sweep, plus a `consumer` sweep when
///   `[check] consumer_features` is configured.
fn decide_sweeps(
    features: &[String],
    no_default_features: bool,
    check_cfg: Option<&CheckConfig>,
) -> Vec<Sweep> {
    if !features.is_empty() || no_default_features {
        let mut feature_args = Vec::new();
        if no_default_features {
            feature_args.push("--no-default-features".to_string());
        }
        if !features.is_empty() {
            feature_args.push("--features".to_string());
            feature_args.push(features.join(","));
        }
        return vec![Sweep {
            label: "default",
            feature_args,
        }];
    }

    let mut sweeps = vec![Sweep {
        label: "all-features",
        feature_args: vec!["--all-features".to_string()],
    }];

    if let Some(cfg) = check_cfg
        && !cfg.consumer_features.is_empty()
    {
        sweeps.push(Sweep {
            label: "consumer",
            feature_args: vec![
                "--no-default-features".to_string(),
                "--features".to_string(),
                cfg.consumer_features.join(","),
            ],
        });
    }

    sweeps
}

#[allow(clippy::too_many_arguments)]
fn run_clippy(
    project_root: &Path,
    check_cfg: Option<&CheckConfig>,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    raw: bool,
    json: bool,
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
    let sweeps = decide_sweeps(features, no_default_features, check_cfg);
    let multi = sweeps.len() > 1;

    let mut results: Vec<SweepResult> = Vec::with_capacity(sweeps.len());
    for sweep in &sweeps {
        let mut args: Vec<String> = vec!["clippy".into(), "--all-targets".into()];
        args.extend(sweep.feature_args.iter().cloned());
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
            label: sweep.label,
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
    label: &'static str,
    #[allow(dead_code)] // text path doesn't read stdout; JSON path does
    stdout: String,
    stderr: String,
    success: bool,
}

/// One row of merged-across-sweep clippy output for the text formatter.
struct MergedDiag<'a> {
    diag: &'a cargo_filter::ClippyDiagnostic,
    sweeps: Vec<&'static str>,
}

/// Merge clippy diagnostics across sweeps, deduplicating by
/// (header, location, message).
fn merge_clippy<'a>(
    parses: &'a [(SweepLabel, cargo_filter::ClippyParse)],
) -> Vec<MergedDiag<'a>> {
    let mut order: Vec<DiagKey> = Vec::new();
    let mut by_key: HashMap<DiagKey, MergedDiag<'a>> = HashMap::new();

    for (label, parsed) in parses {
        for d in &parsed.diagnostics {
            let key = DiagKey::from(d);
            if let Some(existing) = by_key.get_mut(&key) {
                if !existing.sweeps.contains(label) {
                    existing.sweeps.push(label);
                }
            } else {
                order.push(key.clone());
                by_key.insert(
                    key,
                    MergedDiag {
                        diag: d,
                        sweeps: vec![label],
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

type SweepLabel = &'static str;

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

fn sweep_tag(sweeps: &[&'static str]) -> Option<String> {
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
    let parses: Vec<(SweepLabel, cargo_filter::ClippyParse)> = results
        .iter()
        .map(|r| (r.label, cargo_filter::parse_clippy(&r.stderr)))
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
    let mut per_sweep_counts: Vec<(SweepLabel, usize, usize, bool)> =
        Vec::with_capacity(results.len());

    for r in results {
        let label_for_tag = if multi { Some(r.label) } else { None };
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
        per_sweep_counts.push((r.label, errors, warnings, r.success));
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
                            existing.sweeps.push(s);
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

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
fn run_tests(
    project: Option<Project>,
    project_root: &Path,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    raw: bool,
    json: bool,
    extra_args: &[String],
) -> Result<(), DevError> {
    let mut args = vec!["test"];
    if no_default_features {
        args.push("--no-default-features");
    }
    let joined = features.join(",");
    if !features.is_empty() {
        args.push("--features");
        args.push(&joined);
    } else if !no_default_features {
        args.push("--all-features");
    }
    if let Some(pkg) = package {
        args.push("--package");
        args.push(pkg);
    }
    if json {
        args.push("--message-format=json");
    }
    if !extra_args.is_empty() {
        let extra_refs: Vec<&str> = extra_args.iter().map(String::as_str).collect();
        args.extend_from_slice(&extra_refs);
    }

    if !json {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }

    // Nidhogg tests need CARGO_TARGET_TMPDIR set.
    let env: Vec<(&str, &str)> = match project {
        Some(Project::Nidhogg) => vec![("CARGO_TARGET_TMPDIR", "target/tmp")],
        _ => vec![],
    };

    let captured = if env.is_empty() {
        output::run_captured("cargo", &args, project_root)?
    } else {
        output::run_captured_with_env("cargo", &args, project_root, &env)?
    };

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    if json {
        // Split stdout: JSON lines → compile diagnostics, non-JSON → test output.
        let mut json_lines: Vec<&str> = Vec::new();
        let mut test_lines: Vec<&str> = Vec::new();
        for line in stdout.lines() {
            if line.starts_with('{') {
                json_lines.push(line);
            } else {
                test_lines.push(line);
            }
        }

        // Emit compile diagnostics.
        let json_text = json_lines.join("\n");
        let diag_events = cargo_json::parse_cargo_diagnostics(&json_text, "test", None);
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
                    sweep: None,
                    status: diag_status,
                    errors,
                    warnings,
                },
            ));
        }

        // Emit test results.
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

        if parsed.failures.is_empty() && diag_events.is_empty() && !captured.status.success() {
            cargo_json::emit_parse_error("test", &stdout, &stderr);
        }

        // Only emit TestSummary when tests actually ran. On pure compile
        // failures, suites == 0 and an all-zero summary would falsely imply
        // an executed-but-empty test phase.
        if parsed.suites > 0 {
            let test_status = if parsed.failed > 0 { "failed" } else { "ok" };
            cargo_json::emit(&cargo_json::CheckEvent::TestSummary(
                cargo_json::TestSummaryEvent {
                    status: test_status,
                    passed: parsed.passed,
                    failed: parsed.failed,
                    ignored: parsed.ignored,
                    filtered_out: parsed.filtered_out,
                    suites: parsed.suites,
                    duration_seconds: parsed.duration.map(|d| (d * 100.0).round() / 100.0),
                },
            ));
        }

        if !captured.status.success() {
            return Err(DevError::Build("tests failed".into()));
        }
        return Ok(());
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
        return Err(DevError::Build("tests failed".into()));
    }

    if raw {
        if !stderr.is_empty() {
            print!("{stderr}");
        }
        if !stdout.is_empty() {
            print!("{stdout}");
        }
    } else {
        // Success path: surface any compile warnings from the test build
        // (cargo test rebuilds with cfg(test), which can flag warnings the
        // earlier clippy pass didn't see).
        let filtered = cargo_filter::filter_clippy(&stderr);
        if filtered != "cargo clippy: no issues" {
            // Relabel so the [warn] line says "cargo test" not "cargo clippy".
            let relabeled = filtered.replacen("cargo clippy:", "cargo test:", 1);
            output::warn(&relabeled);
        }
    }

    Ok(())
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
    fn decide_sweeps_default_no_config_is_all_features_only() {
        let sweeps = decide_sweeps(&[], false, None);
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[0].feature_args, vec!["--all-features"]);
    }

    #[test]
    fn decide_sweeps_with_consumer_config_runs_two() {
        let cfg = CheckConfig {
            consumer_features: vec!["commands".into()],
        };
        let sweeps = decide_sweeps(&[], false, Some(&cfg));
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(
            sweeps[1].feature_args,
            vec!["--no-default-features", "--features", "commands"]
        );
    }

    #[test]
    fn decide_sweeps_consumer_empty_falls_back_to_single() {
        let cfg = CheckConfig {
            consumer_features: Vec::new(),
        };
        let sweeps = decide_sweeps(&[], false, Some(&cfg));
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all-features");
    }

    #[test]
    fn decide_sweeps_user_features_override_short_circuits() {
        let cfg = CheckConfig {
            consumer_features: vec!["commands".into()],
        };
        let sweeps = decide_sweeps(&["foo".into(), "bar".into()], false, Some(&cfg));
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].feature_args, vec!["--features", "foo,bar"]);
    }

    #[test]
    fn decide_sweeps_no_default_features_overrides() {
        let cfg = CheckConfig {
            consumer_features: vec!["commands".into()],
        };
        let sweeps = decide_sweeps(&[], true, Some(&cfg));
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].feature_args, vec!["--no-default-features"]);
    }

    #[test]
    fn sweep_tag_formats() {
        assert_eq!(sweep_tag(&[]), None);
        assert_eq!(sweep_tag(&["consumer"]), Some("[consumer]".into()));
        assert_eq!(
            sweep_tag(&["all-features", "consumer"]),
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
            ("all-features", cargo_filter::parse_clippy(stderr_a)),
            ("consumer", cargo_filter::parse_clippy(stderr_b)),
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
