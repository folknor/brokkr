// Top-level `[ratatoskr]` brokkr commands.

use std::collections::HashMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::config::DevConfig;
use crate::error::DevError;
use crate::lockfile::{self, LockContext};
use crate::output::{self, CapturedOutput};
use crate::artefacts::{self, ArtefactDir};
use crate::ratatoskr::build::{self, HarnessBuild};
use crate::ratatoskr::discover::{self, Expected, PreserveDataDir, ScriptInfo, SCRIPT_DIR};
use crate::ratatoskr::saehrimnir::{
    endpoint_env_pairs, require_path, resolve_fixture, MockServer,
};

/// Where per-test artefact directories live, relative to the project
/// root. Allocator under [`ArtefactDir`] creates `<this>/<test_id>/run-N/`.
const ARTEFACT_PARENT: &str = ".brokkr/ratatoskr";

/// Run one or more service-test iterations through the harness binary
/// built via `[ratatoskr.harness]`.
///
/// Acquires the global lockfile, builds the configured `[[check]]`
/// sweep once, then for each of `repeat` iterations allocates a fresh
/// `<artefact_parent>/<test_id>/run-N/`, spawns
/// `<binary> --test-harness <SCRIPT>` with `BROKKR_HARNESS_ARTEFACT_DIR`
/// and `BROKKR_TEST_BIN_DIR` set, captures stdout/stderr, writes them
/// alongside a `run.toml` and a copy of the script, then preserves or
/// drops the artefact dir based on outcome.
///
/// `repeat = 1` is the default and prints the existing single-shot
/// PASS/FAIL line. `repeat > 1` switches to soak mode: per-iteration
/// status line, optional bail on first failure (`!keep_going`),
/// summary at the end. The exit code is non-zero if any iteration
/// failed.
///
/// The harness binary itself - the Lua VM, `ServiceClient` userdata,
/// wait combinator, frame-log tap, `/proc` snapshot writer - lives in
/// ratatoskr's `app` crate behind the `test-helpers` feature and lands
/// in Phase 8. Until it does, `app --test-harness` errors out with
/// "unknown flag" and brokkr captures that into the artefact dir
/// faithfully; the plumbing here is structurally complete.
#[allow(clippy::too_many_arguments)] // entry point gathers CLI flags
#[allow(clippy::too_many_lines)] // linear orchestration: parse, build, mock-up, run, mock-down
pub fn service_test(
    project_root: &Path,
    dev_config: &DevConfig,
    script: &str,
    keep_artefacts: bool,
    profile_override: Option<bool>,
    repeat: u32,
    keep_going: bool,
) -> Result<(), DevError> {
    let script_path = Path::new(script);
    if !script_path.exists() {
        return Err(DevError::Config(format!(
            "service-test: script not found: {script}"
        )));
    }
    if script_path.is_dir() {
        // Directory form: sugar for `service-suite --filter <rel>` scoped
        // to the cohort under `<dir>`. Same code path, same artefact
        // layout, same per-script ceiling. `-N` becomes cycles over the
        // cohort; `--keep-going` and `--keep-artefacts` flow through.
        // `include_ignored` is left at the suite default (false); a user
        // who wants to soak ignored scripts goes through `service-suite`
        // directly.
        let filter = directory_filter(project_root, script_path)?;
        return service_suite(
            project_root,
            dev_config,
            Some(&filter),
            keep_artefacts,
            profile_override,
            keep_going,
            false,
            repeat,
        );
    }
    if !script_path.is_file() {
        return Err(DevError::Config(format!(
            "service-test: script path is not a regular file: {script}"
        )));
    }
    let script_abs = script_path.canonicalize().map_err(|e| {
        DevError::Config(format!(
            "service-test: failed to canonicalize script path {script}: {e}"
        ))
    })?;
    let test_id = script_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            DevError::Config(format!(
                "service-test: script path has no usable file stem: {script}"
            ))
        })?
        .to_owned();

    // Parse the script's frontmatter once: the ceiling and the
    // preserve-data-dir override apply to every iteration of a soak.
    let parsed = discover::parse_script(&script_abs, &test_id).map_err(|e| {
        DevError::Config(format!(
            "service-test: failed to read script {script}: {e}"
        ))
    })?;
    let ceiling = parsed.ceiling;
    let keep_on_success =
        keep_artefacts || parsed.preserve_data_dir == PreserveDataDir::OnSuccessToo;

    let harness_cfg = dev_config
        .ratatoskr
        .as_ref()
        .and_then(|r| r.harness.as_ref())
        .ok_or_else(|| {
            DevError::Config(
                "service-test: no [ratatoskr.harness] section in brokkr.toml. \
                 Declare `[ratatoskr.harness]` with `package = \"<crate>\"` \
                 (and optional `binary`, `features`, `debug`)."
                    .into(),
            )
        })?;

    let project_root_str = project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "service-test",
        project_root: &project_root_str,
    })?;
    // Cooperative SIGTERM for `brokkr kill`. See run_sync_smoke for rationale.
    let _sigterm = crate::shutdown::SigtermGuard::install();

    let debug = profile_override.unwrap_or_else(|| harness_cfg.debug.unwrap_or(false));
    let built = build::build_for_harness(
        project_root,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true, // isolate_pg: outer SigtermGuard active
    )?;
    output::ratatoskr_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display(),
    ));

    let artefact_parent = project_root.join(ARTEFACT_PARENT);
    let repeat = repeat.max(1);

    // If the script's frontmatter declares a fixture, spawn sæhrimnir
    // once before iteration starts and inject the per-protocol endpoint
    // env vars. The mock is reused across all soak iterations and torn
    // down after the run finishes (success, failure, or error).
    let mock_session = if let Some(fixture_name) = parsed.fixture.as_deref() {
        Some(FixtureSession::start(
            project_root,
            dev_config,
            &test_id,
            fixture_name,
            &artefact_parent,
            &_lock,
        )?)
    } else {
        None
    };
    let env_pairs: Vec<(&str, &str)> = mock_session
        .as_ref()
        .map(|s| s.env_pair_refs())
        .unwrap_or_default();

    let outcome = if repeat == 1 {
        run_single(
            &artefact_parent,
            &test_id,
            &built,
            &script_abs,
            project_root,
            keep_on_success,
            ceiling,
            &env_pairs,
            &_lock,
        )
    } else {
        run_soak(
            &artefact_parent,
            &test_id,
            &built,
            &script_abs,
            project_root,
            keep_on_success,
            ceiling,
            repeat,
            keep_going,
            &env_pairs,
            &_lock,
        )
    };

    if let Some(session) = mock_session {
        session.shutdown(&_lock);
    }
    outcome
}

/// Outcome of one harness-binary invocation.
///
/// `artefact_path` is `Some` when the run dir is preserved on disk -
/// always for failures, and for successes only when `--keep-artefacts`
/// was set. `exit_label` is the human-readable exit summary
/// (`exit=N`, `signal=N`, or `unknown exit`).
struct RunResult {
    succeeded: bool,
    elapsed_ms: u128,
    exit_label: String,
    artefact_path: Option<PathBuf>,
}

/// Single-run dispatch (`repeat == 1`). Mirrors the pre-soak output
/// shape exactly: one "running ..." line, one PASS/FAIL line.
#[allow(clippy::too_many_arguments)] // passes through the ceiling + keep flags from the entry point
fn run_single(
    artefact_parent: &Path,
    test_id: &str,
    built: &HarnessBuild,
    script_abs: &Path,
    project_root: &Path,
    keep_on_success: bool,
    ceiling: Duration,
    extra_env: &[(&str, &str)],
    lock: &lockfile::LockGuard,
) -> Result<(), DevError> {
    output::ratatoskr_msg(&format!(
        "running {test_id} against {}",
        built.binary.display()
    ));
    let artefacts = ArtefactDir::allocate(artefact_parent, test_id, keep_on_success)?;
    let result = spawn_and_capture(artefacts, built, script_abs, project_root, ceiling, extra_env, lock)?;
    if result.succeeded {
        output::ratatoskr_msg(&format!("PASS in {}ms", result.elapsed_ms));
        Ok(())
    } else {
        let dir = result
            .artefact_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<missing>".to_owned());
        output::ratatoskr_msg(&format!(
            "FAIL {} in {}ms (artefacts: {dir})",
            result.exit_label, result.elapsed_ms
        ));
        artefacts::emit_clean_hint();
        Err(DevError::ExitCode(1))
    }
}

/// Soak dispatch (`repeat > 1`). Emits a per-iteration status line and
/// a summary. With `keep_going = false` (default), bails on the first
/// failed iteration; the summary then reports "stopped at iter X/Y".
#[allow(clippy::too_many_arguments)] // mirrors the entry point's plumbing
fn run_soak(
    artefact_parent: &Path,
    test_id: &str,
    built: &HarnessBuild,
    script_abs: &Path,
    project_root: &Path,
    keep_on_success: bool,
    ceiling: Duration,
    repeat: u32,
    keep_going: bool,
    extra_env: &[(&str, &str)],
    lock: &lockfile::LockGuard,
) -> Result<(), DevError> {
    output::ratatoskr_msg(&format!(
        "running {test_id} against {} (-N {repeat}{})",
        built.binary.display(),
        if keep_going { ", --keep-going" } else { "" }
    ));

    let mut results: Vec<(u32, RunResult)> = Vec::new();
    for iter in 1..=repeat {
        lock.set_progress(iter, repeat);
        let artefacts = ArtefactDir::allocate(artefact_parent, test_id, keep_on_success)?;
        let result = spawn_and_capture(artefacts, built, script_abs, project_root, ceiling, extra_env, lock)?;
        output::ratatoskr_msg(&format_iter_line(iter, repeat, &result));
        let stop = !result.succeeded && !keep_going;
        results.push((iter, result));
        if stop {
            break;
        }
    }

    output::ratatoskr_msg(&format_soak_summary(&results, repeat));

    if results.iter().any(|(_, r)| !r.succeeded) {
        artefacts::emit_clean_hint();
        Err(DevError::ExitCode(1))
    } else {
        Ok(())
    }
}

/// Spawn the harness binary against `script_abs`, write its outputs +
/// metadata into the supplied artefact dir, then finalize the dir per
/// the binary's exit status. Returns a [`RunResult`] describing the
/// outcome; spawn-level failures (binary missing, etc.) propagate as
/// `Err` after dropping a `spawn-error.txt` breadcrumb in the dir.
///
/// `ceiling` bounds the wall-clock the harness binary may run for - if
/// it exceeds the budget brokkr SIGKILLs the child and reports the run
/// as `ceiling=<ceiling>` so the failure surface is unambiguous.
fn spawn_and_capture(
    artefacts: ArtefactDir,
    built: &HarnessBuild,
    script_abs: &Path,
    project_root: &Path,
    ceiling: Duration,
    extra_env: &[(&str, &str)],
    lock: &lockfile::LockGuard,
) -> Result<RunResult, DevError> {
    let binary_str = built.binary.display().to_string();
    let script_str = script_abs.display().to_string();
    let artefact_path_str = artefacts.path().display().to_string();
    let bin_dir_str = built.bin_dir.display().to_string();

    let mut env_pairs: Vec<(&str, &str)> = vec![
        ("BROKKR_HARNESS_ARTEFACT_DIR", &artefact_path_str),
        ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
    ];
    env_pairs.extend_from_slice(extra_env);

    let deadline_capture = match output::run_captured_with_env_and_deadline(
        &binary_str,
        &["--test-harness", &script_str],
        project_root,
        &env_pairs,
        ceiling,
        Some(&|pid| lock.set_child_pid(pid)),
        true, // isolate_pg: caller's SigtermGuard active
    ) {
        Ok(c) => c,
        // `Interrupted` means the child spawned and ran fine until
        // `brokkr kill` reached us - no breadcrumb to write, just clear
        // the live PID and propagate.
        Err(DevError::Interrupted) => {
            lock.clear_child_pid();
            artefacts.finalize_failure();
            return Err(DevError::Interrupted);
        }
        Err(e) => {
            fs::write(
                artefacts.path().join("spawn-error.txt"),
                format!("failed to spawn {}: {e}\n", built.binary.display()),
            )
            .ok();
            artefacts.finalize_failure();
            return Err(e);
        }
    };
    // The child has reaped; clear the recorded PID so `brokkr kill --hard`
    // arriving in the gap before the next iteration doesn't SIGKILL a
    // PID-recycled-to-something-else.
    lock.clear_child_pid();

    let killed_on_deadline = deadline_capture.killed_on_deadline;
    let captured = deadline_capture.captured;

    write_artefacts(artefacts.path(), script_abs, built, &captured, project_root)?;

    let elapsed_ms = captured.elapsed.as_millis();
    let exit_label = if killed_on_deadline {
        format!("ceiling={}", format_duration(ceiling))
    } else {
        match (captured.status.code(), captured.status.signal()) {
            (Some(code), _) => format!("exit={code}"),
            (None, Some(sig)) => format!("signal={sig}"),
            (None, None) => "unknown exit".to_owned(),
        }
    };

    let succeeded = !killed_on_deadline && captured.status.success();
    if succeeded {
        let path = artefacts.path().to_path_buf();
        artefacts.finalize_success()?;
        // finalize_success deletes the dir unless `keep_on_success` was
        // set at allocation time; check disk for the truth.
        let preserved = path.exists();
        Ok(RunResult {
            succeeded: true,
            elapsed_ms,
            exit_label,
            artefact_path: preserved.then_some(path),
        })
    } else {
        let path = artefacts.path().to_path_buf();
        artefacts.finalize_failure();
        Ok(RunResult {
            succeeded: false,
            elapsed_ms,
            exit_label,
            artefact_path: Some(path),
        })
    }
}

/// Render a `Duration` in the `<n>{ms,s,m,h}` shape `discover.rs` accepts.
/// Used for `ceiling=` exit labels so the failure surface mirrors the
/// frontmatter spelling.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return format!("{}ms", d.subsec_millis());
    }
    if d.subsec_millis() != 0 {
        return format!("{}ms", d.as_millis());
    }
    if secs.is_multiple_of(3600) {
        return format!("{}h", secs / 3600);
    }
    if secs.is_multiple_of(60) {
        return format!("{}m", secs / 60);
    }
    format!("{secs}s")
}

/// Format one soak iteration's status line. Pure for testability.
fn format_iter_line(iter: u32, total: u32, result: &RunResult) -> String {
    if result.succeeded {
        format!("iter {iter}/{total}: PASS in {}ms", result.elapsed_ms)
    } else {
        let dir = result
            .artefact_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<missing>".to_owned());
        format!(
            "iter {iter}/{total}: FAIL {} in {}ms (artefacts: {dir})",
            result.exit_label, result.elapsed_ms
        )
    }
}

/// Format the trailing soak summary. Pure for testability.
///
/// All-passed shape: `soak: N/total passed (min Xms, max Yms, avg Zms)`.
/// Stopped-early shape: `soak: stopped at iter F/total (P passed, 1 failed)`.
/// Keep-going shape: `soak: P/total passed, F failed (iters i, j, k)`.
fn format_soak_summary(results: &[(u32, RunResult)], total: u32) -> String {
    let pass_times: Vec<u128> = results
        .iter()
        .filter_map(|(_, r)| r.succeeded.then_some(r.elapsed_ms))
        .collect();
    let fail_iters: Vec<u32> = results
        .iter()
        .filter_map(|(i, r)| (!r.succeeded).then_some(*i))
        .collect();
    let pass_count = pass_times.len();
    let fail_count = fail_iters.len();
    let ran = u32::try_from(results.len()).unwrap_or(u32::MAX);
    let stopped_early = ran < total;

    if fail_iters.is_empty() {
        let min = pass_times.iter().min().copied().unwrap_or(0);
        let max = pass_times.iter().max().copied().unwrap_or(0);
        let avg = if pass_times.is_empty() {
            0
        } else {
            pass_times.iter().sum::<u128>() / pass_times.len() as u128
        };
        format!("soak: {pass_count}/{total} passed (min {min}ms, max {max}ms, avg {avg}ms)")
    } else if stopped_early {
        // keep_going = false path: by construction there's exactly one
        // failed iter and the loop bailed immediately after it.
        let first_fail = fail_iters[0];
        format!(
            "soak: stopped at iter {first_fail}/{total} ({pass_count} passed, {fail_count} failed)"
        )
    } else {
        let fail_str = fail_iters
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("soak: {pass_count}/{total} passed, {fail_count} failed (iters {fail_str})")
    }
}

/// `brokkr service-list` - print every discovered script with its
/// description and expected outcome. Empty-state message points at the
/// expected location so a fresh checkout (no harness module yet) still
/// gets a useful response.
pub fn service_list(project_root: &Path) -> Result<(), DevError> {
    let scripts = discover::discover(project_root)?;
    if scripts.is_empty() {
        output::ratatoskr_msg(&format!(
            "no service-test scripts found under {SCRIPT_DIR}/"
        ));
        output::ratatoskr_msg(
            "  (the harness module has not landed in ratatoskr yet, or no scripts have been added)",
        );
        return Ok(());
    }

    output::ratatoskr_msg(&format!(
        "  {:<40} {:<10} {}",
        "Name", "Expected", "Description",
    ));
    output::ratatoskr_msg(&format!("  {}", "\u{2500}".repeat(78)));
    for ScriptInfo {
        name,
        description,
        expected,
        ..
    } in &scripts
    {
        output::ratatoskr_msg(&format!(
            "  {:<40} {:<10} {}",
            name,
            expected.as_str(),
            description.as_deref().unwrap_or("\u{2014}"),
        ));
    }
    Ok(())
}

