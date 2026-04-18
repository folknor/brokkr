//! Implementation of the `check` command (clippy + tests).

use std::path::Path;

use crate::cargo_filter;
use crate::cargo_json;
use crate::error::DevError;
use crate::output;
use crate::project::Project;

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_check(
    project: Project,
    project_root: &Path,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    raw: bool,
    json: bool,
    extra_args: &[String],
) -> Result<(), DevError> {
    run_clippy(project_root, features, no_default_features, package, raw, json)?;
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

fn run_clippy(
    project_root: &Path,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    raw: bool,
    json: bool,
) -> Result<(), DevError> {
    let mut args = vec!["clippy", "--all-targets"];
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

    if !json {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }

    let captured = output::run_captured("cargo", &args, project_root)?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    if json {
        let events = cargo_json::parse_cargo_diagnostics(&stdout, "clippy");
        let mut errors = 0usize;
        let mut warnings = 0usize;
        for event in &events {
            if let cargo_json::CheckEvent::Diagnostic(d) = event {
                match d.level.as_str() {
                    "error" => errors += 1,
                    "warning" => warnings += 1,
                    _ => {}
                }
            }
            cargo_json::emit(event);
        }
        if events.is_empty() && !captured.status.success() {
            cargo_json::emit_parse_error("clippy", &stdout, &stderr);
            errors += 1;
        }
        let status = if captured.status.success() { "ok" } else { "failed" };
        cargo_json::emit(&cargo_json::CheckEvent::DiagnosticSummary(
            cargo_json::DiagnosticSummaryEvent {
                tool: "clippy",
                status,
                errors,
                warnings,
            },
        ));
        if !captured.status.success() {
            return Err(DevError::Build("clippy failed".into()));
        }
        return Ok(());
    }

    if !captured.status.success() {
        if raw {
            output::error(&stderr);
        } else {
            output::error(&cargo_filter::filter_clippy(&stderr));
        }
        return Err(DevError::Build("clippy failed".into()));
    }

    if raw && !stderr.is_empty() {
        print!("{stderr}");
    } else if !raw {
        // Success path: surface any warnings the filter extracted so they
        // aren't silently dropped when the build passes.
        let filtered = cargo_filter::filter_clippy(&stderr);
        if filtered != "cargo clippy: no issues" {
            output::warn(&filtered);
        }
    }

    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
fn run_tests(
    project: Project,
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
        Project::Nidhogg => vec![("CARGO_TARGET_TMPDIR", "target/tmp")],
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
        let diag_events = cargo_json::parse_cargo_diagnostics(&json_text, "test");
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
