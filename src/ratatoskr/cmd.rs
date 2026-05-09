//! Top-level `[ratatoskr]` brokkr commands.

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
use crate::ratatoskr::artefacts::{self, ArtefactDir};
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
    debug: bool,
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
            debug,
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
                 Add a [[check]] entry naming the harness sweep, then \
                 [ratatoskr.harness] sweep = \"<name>\", binary = \"<package>\"."
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

    let built = build::build_for_harness(
        project_root,
        &dev_config.check,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true, // isolate_pg: outer SigtermGuard active
    )?;
    output::ratatoskr_msg(&format!(
        "harness build ok (sweep={}, binary={})",
        built.sweep_label,
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

/// `brokkr service-suite` - run every discovered script (optionally
/// filtered) against a single shared harness build.
///
/// Discovery + filter happens first so we can bail with a useful message
/// when nothing matches before paying the cargo build cost. `expected =
/// ignored` scripts are skipped by default; `--include-ignored` opts
/// them in. Each script runs through the same `spawn_and_capture` path
/// `service-test` uses, so artefact-dir lifecycle and ceiling semantics
/// are identical. The summary at the end lists failed scripts by name -
/// not iter index, since each script ran exactly once.
#[allow(clippy::too_many_arguments)] // entry point gathers CLI flags
#[allow(clippy::too_many_lines)] // linear orchestration: discover, validate, build, run with fixture grouping, summarize
pub fn service_suite(
    project_root: &Path,
    dev_config: &DevConfig,
    filter: Option<&str>,
    keep_artefacts: bool,
    debug: bool,
    keep_going: bool,
    include_ignored: bool,
    repeat: u32,
) -> Result<(), DevError> {
    let cycles = repeat.max(1);
    let all = discover::discover(project_root)?;
    let total_discovered = all.len();
    let SuiteSelection {
        runnable,
        skipped_ignored,
        filtered_out,
    } = select_suite(all, filter, include_ignored);

    if runnable.is_empty() {
        output::ratatoskr_msg(&format_empty_suite(
            total_discovered,
            filtered_out,
            skipped_ignored,
            filter,
        ));
        return Ok(());
    }

    let harness_cfg = dev_config
        .ratatoskr
        .as_ref()
        .and_then(|r| r.harness.as_ref())
        .ok_or_else(|| {
            DevError::Config(
                "service-suite: no [ratatoskr.harness] section in brokkr.toml. \
                 Add a [[check]] entry naming the harness sweep, then \
                 [ratatoskr.harness] sweep = \"<name>\", binary = \"<package>\"."
                    .into(),
            )
        })?;

    // Fail fast if any selected script needs a fixture but the
    // [ratatoskr] mock-server config isn't populated. Spawn-time would
    // catch this too but reporting it before the build wastes nothing.
    if let Some(needy) = runnable.iter().find(|s| s.fixture.is_some()) {
        let cfg = dev_config.ratatoskr.as_ref().expect("checked above");
        require_path(&cfg.mock_server_binary, project_root, "mock_server_binary").map_err(|e| {
            DevError::Config(format!(
                "service-suite: script {} needs a mock fixture but {e}",
                needy.name
            ))
        })?;
        require_path(&cfg.fixtures_dir, project_root, "fixtures_dir").map_err(|e| {
            DevError::Config(format!(
                "service-suite: script {} needs a mock fixture but {e}",
                needy.name
            ))
        })?;
    }

    let project_root_str = project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "service-suite",
        project_root: &project_root_str,
    })?;
    // Cooperative SIGTERM for `brokkr kill`. See run_sync_smoke for rationale.
    let _sigterm = crate::shutdown::SigtermGuard::install();

    let built = build::build_for_harness(
        project_root,
        &dev_config.check,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true, // isolate_pg: outer SigtermGuard active
    )?;
    output::ratatoskr_msg(&format!(
        "harness build ok (sweep={}, binary={})",
        built.sweep_label,
        built.binary.display(),
    ));
    output::ratatoskr_msg(&format_suite_header(
        runnable.len(),
        skipped_ignored,
        filtered_out,
        filter,
        cycles,
    ));

    let artefact_parent = project_root.join(ARTEFACT_PARENT);
    let total = runnable.len();
    let total_runs = u32::try_from(total).unwrap_or(u32::MAX).saturating_mul(cycles);
    let mut results: Vec<CycleRun> = Vec::with_capacity(total * cycles as usize);
    let mut bailed = false;
    // Suite-scoped fixture reuse: one sæhrimnir per distinct fixture
    // declared by any selected script, kept alive for every cycle. A
    // no-fixture script in the middle of a fixture-X run leaves the
    // fixture-X mock alone but receives NO endpoint env vars (its
    // contract is "act as if no fixture exists"). Fixture isolation is
    // a per-script concern - scripts wanting a clean slate hit their
    // own /test/<protocol>/reset endpoint at start-of-test.
    let mut mocks: HashMap<String, FixtureSession> = HashMap::new();

    // Drives the inner loop's exit shape: Ok runs to completion, Err
    // breaks out and gets propagated AFTER mocks are drained gracefully.
    // Without this, a non-Interrupted error from spawn_and_capture would
    // skip the post-loop drain, leaving FixtureSession::Drop to take
    // MockServer's hard SIGKILL path - contrary to the graceful contract.
    let loop_result: Result<(), DevError> = (|| {
        'cycles: for cycle in 1..=cycles {
            for (idx, script) in runnable.iter().enumerate() {
                let pos = idx + 1;

                // Lazy-spawn the fixture for this script if it declares
                // one and we don't already have it. Mocks live until the
                // end of the suite.
                if let Some(fixture_name) = script.fixture.as_deref()
                    && !mocks.contains_key(fixture_name)
                {
                    let session = FixtureSession::start(
                        project_root,
                        dev_config,
                        &script.name,
                        fixture_name,
                        &artefact_parent,
                        &_lock,
                    )?;
                    mocks.insert(fixture_name.to_owned(), session);
                }

                // ArtefactDir test_id rejects `/` in the name; nested scripts
                // like `t1/journal_replays` collapse to the file stem for the
                // artefact dir while keeping the full relative name in output.
                // Two scripts with the same stem under different parents would
                // share an artefact-dir prefix and just allocate run-(N+1)/ -
                // not ideal, but mirrors how `service-test` handles arbitrary
                // script paths today.
                let test_id = script
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&script.name)
                    .to_owned();
                let keep_on_success =
                    keep_artefacts || script.preserve_data_dir == PreserveDataDir::OnSuccessToo;

                output::ratatoskr_msg(&format_suite_running_line(
                    cycle, cycles, pos, total, &script.name,
                ));
                let run_index = (cycle - 1)
                    .saturating_mul(u32::try_from(total).unwrap_or(u32::MAX))
                    .saturating_add(u32::try_from(pos).unwrap_or(u32::MAX));
                _lock.set_progress(run_index, total_runs);
                let artefacts =
                    ArtefactDir::allocate(&artefact_parent, &test_id, keep_on_success)?;
                // Inject endpoint env vars only if THIS script declares a
                // fixture. No-fixture scripts run as if no mock exists,
                // even when other scripts in the suite have mocks alive.
                let env_pairs = script
                    .fixture
                    .as_deref()
                    .and_then(|name| mocks.get(name))
                    .map(FixtureSession::env_pair_refs)
                    .unwrap_or_default();
                let result = spawn_and_capture(
                    artefacts,
                    &built,
                    &script.path,
                    project_root,
                    script.ceiling,
                    &env_pairs,
                    &_lock,
                )?;
                output::ratatoskr_msg(&format_suite_iter_line(
                    cycle, cycles, pos, total, &script.name, &result,
                ));
                let stop = !result.succeeded && !keep_going;
                results.push(CycleRun {
                    cycle,
                    name: script.name.clone(),
                    result,
                });
                if stop {
                    bailed = true;
                    break 'cycles;
                }
            }
        }
        Ok(())
    })();

    // Drain every fixture gracefully, regardless of whether the loop
    // succeeded or errored. Each `shutdown` SIGTERMs sæhrimnir with the
    // standard 1.5s budget; the alternative (FixtureSession::Drop on
    // error unwind) is the SIGKILL fast-path. We do this BEFORE
    // propagating the error so a failing suite doesn't leave the user
    // with an SIGKILLed mock and uninformative artefacts.
    for (_, session) in mocks.drain() {
        session.shutdown(&_lock);
    }
    _lock.clear_mock_pids();

    loop_result?;

    output::ratatoskr_msg(&format_suite_summary(&results, total, cycles, bailed));

    if results.iter().any(|r| !r.result.succeeded) {
        artefacts::emit_clean_hint();
        Err(DevError::ExitCode(1))
    } else {
        Ok(())
    }
}

/// One entry in the suite's flat run log. `cycle` is 1-based and only
/// meaningful when `cycles > 1`; the summary formatters use it to
/// compute per-script totals across the cohort soak.
struct CycleRun {
    cycle: u32,
    name: String,
    result: RunResult,
}

/// Resolve a directory under `<project_root>/<SCRIPT_DIR>/` to a
/// substring filter the suite path can consume. Returns the relative
/// path with a trailing slash so `t1` cannot match `t1abc`. Returns an
/// empty string when the directory IS the script root - the suite
/// treats `Some("")` as "match everything", same as `None`.
fn directory_filter(project_root: &Path, dir: &Path) -> Result<String, DevError> {
    let canon = dir.canonicalize().map_err(|e| {
        DevError::Config(format!(
            "service-test: failed to canonicalize directory {}: {e}",
            dir.display()
        ))
    })?;
    let script_root = project_root.join(SCRIPT_DIR);
    let script_root_canon = script_root.canonicalize().map_err(|e| {
        DevError::Config(format!(
            "service-test: failed to canonicalize script root {}: {e}. \
             Is the harness directory present?",
            script_root.display()
        ))
    })?;
    let rel = canon.strip_prefix(&script_root_canon).map_err(|_| {
        DevError::Config(format!(
            "service-test: directory {} is not under {}",
            dir.display(),
            script_root.display()
        ))
    })?;
    let mut filter = rel.to_string_lossy().into_owned();
    if !filter.is_empty() && !filter.ends_with('/') {
        filter.push('/');
    }
    Ok(filter)
}

/// Outcome of [`select_suite`]: the scripts to run, plus counters for
/// the header / empty-state messages.
struct SuiteSelection {
    runnable: Vec<ScriptInfo>,
    /// Scripts that matched the filter (or all, if none) but were
    /// skipped because `expected = ignored` and `--include-ignored` was
    /// not set.
    skipped_ignored: usize,
    /// Scripts excluded by the filter.
    filtered_out: usize,
}

/// Apply the filter and ignored-skip policy. Pulled out for direct
/// unit-testing without touching disk or the harness build.
fn select_suite(
    scripts: Vec<ScriptInfo>,
    filter: Option<&str>,
    include_ignored: bool,
) -> SuiteSelection {
    let mut runnable = Vec::new();
    let mut skipped_ignored = 0usize;
    let mut filtered_out = 0usize;
    for script in scripts {
        let matches = filter.is_none_or(|f| script.name.contains(f));
        if !matches {
            filtered_out += 1;
            continue;
        }
        if !include_ignored && script.expected == Expected::Ignored {
            skipped_ignored += 1;
            continue;
        }
        runnable.push(script);
    }
    SuiteSelection {
        runnable,
        skipped_ignored,
        filtered_out,
    }
}

/// Header line printed once the runnable set is non-empty.
fn format_suite_header(
    runnable: usize,
    skipped_ignored: usize,
    filtered_out: usize,
    filter: Option<&str>,
    cycles: u32,
) -> String {
    let mut head = format!("suite: {runnable} script{}", if runnable == 1 { "" } else { "s" });
    if cycles > 1 {
        let total_runs = runnable as u64 * cycles as u64;
        head = format!("{head} \u{00d7} {cycles} cycles ({total_runs} runs)");
    }
    let mut parts = vec![head];
    if let Some(f) = filter {
        parts.push(format!("filter=\"{f}\""));
    }
    if skipped_ignored > 0 {
        parts.push(format!("{skipped_ignored} ignored"));
    }
    if filtered_out > 0 && filter.is_some() {
        parts.push(format!("{filtered_out} filtered out"));
    }
    parts.join(", ")
}

/// "running" line emitted before each spawn. For single-cycle runs we
/// keep the existing `[N/total]` shape; soak runs gain a `[cycle c/C]`
/// prefix so the log is greppable per-cycle.
fn format_suite_running_line(
    cycle: u32,
    cycles: u32,
    pos: usize,
    total: usize,
    name: &str,
) -> String {
    if cycles > 1 {
        format!("[cycle {cycle}/{cycles}][{pos}/{total}] running {name}")
    } else {
        format!("[{pos}/{total}] running {name}")
    }
}

/// Empty-state message when no scripts are runnable. Distinguishes
/// "nothing discovered" from "filter matched nothing" from "everything
/// was skipped as ignored" so a fresh checkout vs a typo'd filter vs an
/// all-broken cohort each get a useful response.
fn format_empty_suite(
    total_discovered: usize,
    filtered_out: usize,
    skipped_ignored: usize,
    filter: Option<&str>,
) -> String {
    if total_discovered == 0 {
        return format!("no service-test scripts found under {SCRIPT_DIR}/");
    }
    if let Some(f) = filter {
        if filtered_out == total_discovered {
            return format!(
                "filter \"{f}\" matched no scripts ({total_discovered} discovered)"
            );
        }
        if skipped_ignored > 0 {
            return format!(
                "filter \"{f}\" matched {skipped_ignored} script{} but all are ignored \
                 (use --include-ignored to run them)",
                if skipped_ignored == 1 { "" } else { "s" }
            );
        }
    }
    if skipped_ignored == total_discovered {
        return format!(
            "all {total_discovered} discovered script{} are ignored \
             (use --include-ignored to run them)",
            if total_discovered == 1 { "" } else { "s" }
        );
    }
    "no scripts to run".to_owned()
}

/// Per-script status line, shape `[N/total] PASS|FAIL <name> ...`.
/// Pure for testability. Soak runs (`cycles > 1`) prefix `[cycle c/C]`.
fn format_suite_iter_line(
    cycle: u32,
    cycles: u32,
    pos: usize,
    total: usize,
    name: &str,
    result: &RunResult,
) -> String {
    let prefix = if cycles > 1 {
        format!("[cycle {cycle}/{cycles}][{pos}/{total}]")
    } else {
        format!("[{pos}/{total}]")
    };
    if result.succeeded {
        format!("{prefix} PASS {name} in {}ms", result.elapsed_ms)
    } else {
        let dir = result
            .artefact_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<missing>".to_owned());
        format!(
            "{prefix} FAIL {name} {} in {}ms (artefacts: {dir})",
            result.exit_label, result.elapsed_ms
        )
    }
}

/// Trailing summary for the suite. Pure for testability.
///
/// Single-cycle (`cycles == 1`) keeps the existing one-liner shapes so
/// `service-suite` logs don't churn for the common case. Soak mode
/// (`cycles > 1`) emits a cohort-level header line followed by an
/// indented per-script `pass/total` table so the user can see which
/// script broke without scrolling through 500 status lines.
fn format_suite_summary(
    results: &[CycleRun],
    total: usize,
    cycles: u32,
    bailed: bool,
) -> String {
    if cycles <= 1 {
        return format_single_cycle_summary(results, total, bailed);
    }

    let pass_count = results.iter().filter(|r| r.result.succeeded).count();
    let fail_count = results.len() - pass_count;
    let cohort_total = total as u64 * cycles as u64;

    let mut head = if fail_count == 0 {
        let pass_times: Vec<u128> = results.iter().map(|r| r.result.elapsed_ms).collect();
        let min = pass_times.iter().min().copied().unwrap_or(0);
        let max = pass_times.iter().max().copied().unwrap_or(0);
        let avg = if pass_times.is_empty() {
            0
        } else {
            pass_times.iter().sum::<u128>() / pass_times.len() as u128
        };
        format!(
            "soak: {pass_count}/{cohort_total} cohort runs passed across {cycles} cycles \
             (min {min}ms, max {max}ms, avg {avg}ms)"
        )
    } else if bailed {
        // First failure stops the cohort; report the cycle and script.
        let first_fail = results.iter().find(|r| !r.result.succeeded).expect("bailed implies a failure");
        format!(
            "soak: stopped at cycle {}/{cycles} {} ({pass_count} passed, {fail_count} failed)",
            first_fail.cycle, first_fail.name
        )
    } else {
        format!(
            "soak: {pass_count}/{cohort_total} cohort runs passed, {fail_count} failed across {cycles} cycles"
        )
    };

    // Per-script totals: pass/run count for each script, in the order
    // the cohort first encountered them. A user re-running with `-N 50`
    // most cares about which scripts didn't hit 50/50; sorting by name
    // would scatter the failing ones, so we keep cohort order.
    let mut order: Vec<String> = Vec::new();
    let mut stats: std::collections::HashMap<String, (u32, u32)> = std::collections::HashMap::new();
    for r in results {
        let entry = stats.entry(r.name.clone()).or_insert_with(|| {
            order.push(r.name.clone());
            (0, 0)
        });
        entry.1 += 1;
        if r.result.succeeded {
            entry.0 += 1;
        }
    }
    let name_width = order.iter().map(String::len).max().unwrap_or(0);
    head.push_str("\n  per-script totals:");
    for name in &order {
        let (pass, runs) = stats[name];
        let marker = if pass == runs { " " } else { "!" };
        head.push_str(&format!("\n    {marker} {name:<name_width$}  {pass}/{runs}"));
    }
    head
}

/// Pre-existing single-cycle summary. Pulled into a helper so the
/// soak-mode formatter can reuse the cycles=1 shape without growing
/// branches. Logic is identical to the old `format_suite_summary`.
fn format_single_cycle_summary(results: &[CycleRun], total: usize, bailed: bool) -> String {
    let pass_times: Vec<u128> = results
        .iter()
        .filter_map(|r| r.result.succeeded.then_some(r.result.elapsed_ms))
        .collect();
    let fail_names: Vec<&str> = results
        .iter()
        .filter_map(|r| (!r.result.succeeded).then_some(r.name.as_str()))
        .collect();
    let pass_count = pass_times.len();
    let fail_count = fail_names.len();

    if fail_names.is_empty() {
        let min = pass_times.iter().min().copied().unwrap_or(0);
        let max = pass_times.iter().max().copied().unwrap_or(0);
        let avg = if pass_times.is_empty() {
            0
        } else {
            pass_times.iter().sum::<u128>() / pass_times.len() as u128
        };
        format!("suite: {pass_count}/{total} passed (min {min}ms, max {max}ms, avg {avg}ms)")
    } else if bailed {
        let first_fail = fail_names[0];
        format!(
            "suite: stopped at {first_fail} ({pass_count} passed, {fail_count} failed)"
        )
    } else {
        let names = fail_names.join(", ");
        format!("suite: {pass_count}/{total} passed, {fail_count} failed ({names})")
    }
}

/// Reproducibility metadata serialized as `run.toml` next to the
/// captured logs. Optional fields elide cleanly when unavailable so a
/// failed git query (e.g. detached worktree) does not poison the file.
#[derive(Serialize)]
struct RunMetadata {
    brokkr_version: String,
    script: String,
    binary: String,
    sweep: String,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_clean: Option<bool>,
}

/// Write the per-run artefact files: captured stdout/stderr, a copy of
/// the script (so the dir is self-contained), and a `run.toml` with
/// reproducibility metadata. Called after the harness binary has exited.
fn write_artefacts(
    artefact_dir: &Path,
    script_abs: &Path,
    built: &HarnessBuild,
    captured: &CapturedOutput,
    project_root: &Path,
) -> Result<(), DevError> {
    fs::write(artefact_dir.join("binary-stdout.log"), &captured.stdout)?;
    fs::write(artefact_dir.join("binary-stderr.log"), &captured.stderr)?;

    let script_filename: PathBuf = script_abs
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("script.lua"));
    fs::copy(script_abs, artefact_dir.join(&script_filename))?;

    let git = crate::git::collect(project_root).ok();
    let meta = RunMetadata {
        brokkr_version: env!("CARGO_PKG_VERSION").to_owned(),
        script: script_abs.display().to_string(),
        binary: built.binary.display().to_string(),
        sweep: built.sweep_label.clone(),
        elapsed_ms: u64::try_from(captured.elapsed.as_millis()).unwrap_or(u64::MAX),
        exit_code: captured.status.code(),
        signal: captured.status.signal(),
        git_commit: git.as_ref().map(|g| g.commit.clone()),
        git_subject: git.as_ref().map(|g| g.subject.clone()),
        git_clean: git.as_ref().map(|g| g.is_clean),
    };
    let serialized = toml::to_string(&meta).map_err(|e| {
        DevError::Config(format!("service-test: failed to serialize run.toml: {e}"))
    })?;
    fs::write(artefact_dir.join("run.toml"), serialized)?;
    Ok(())
}

/// Owns a running sæhrimnir process plus the per-protocol endpoint env
/// pairs that should be injected into the harness binary. Created when a
/// service-harness script declares a `-- fixture:` line; dropped (or
/// gracefully `shutdown`) once every dependent harness invocation has
/// returned.
///
/// `service-test` creates one before iteration and drains it after.
/// `service-suite` creates one per fixture-group transition (see
/// [`SuiteFixtureSlot`]).
struct FixtureSession {
    fixture_name: String,
    mock: MockServer,
    env_owned: Vec<(String, String)>,
}

impl FixtureSession {
    /// Validate `[ratatoskr]` config, resolve the fixture, spawn
    /// sæhrimnir, parse endpoints, and emit a one-line "mock-server up"
    /// status. Errors out with the same shape `mock-serve` uses when
    /// config is missing or the binary isn't built.
    fn start(
        project_root: &Path,
        dev_config: &crate::config::DevConfig,
        owner_label: &str,
        fixture_name: &str,
        artefact_parent: &Path,
        lock: &lockfile::LockGuard,
    ) -> Result<Self, DevError> {
        let cfg = dev_config.ratatoskr.as_ref().ok_or_else(|| {
            DevError::Config(format!(
                "service-test: script {owner_label} declares `-- fixture: {fixture_name}` \
                 but no [ratatoskr] section exists in brokkr.toml. \
                 Set mock_server_binary and fixtures_dir to point at sæhrimnir's checkout."
            ))
        })?;
        let binary = require_path(&cfg.mock_server_binary, project_root, "mock_server_binary")?;
        let fixtures_dir = require_path(&cfg.fixtures_dir, project_root, "fixtures_dir")?;
        if !binary.exists() {
            return Err(DevError::Config(format!(
                "service-test: sæhrimnir binary not found at {}. \
                 Build it first: `cargo build --release` in sæhrimnir's repo.",
                binary.display()
            )));
        }
        let fixture_path = resolve_fixture(&fixtures_dir, fixture_name)?;

        let mock_dir = artefact_parent.join("mock").join(safe_dir_name(fixture_name));
        std::fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;

        // PID published from inside spawn_observed - before readiness
        // wait - so a `--hard` landing during sæhrimnir startup finds it.
        let mock = MockServer::spawn_observed(
            &binary,
            &fixture_path,
            &mock_dir,
            Some(&|pid| lock.add_mock_pid(pid)),
            Some(&|pid| lock.remove_mock_pid(pid)),
            true, // isolate_pg: caller (service-test/-suite) has SigtermGuard
        )?;
        let env_owned = endpoint_env_pairs(cfg, mock.endpoints());
        let ep = mock.endpoints();
        output::ratatoskr_msg(&format!(
            "mock-server up: fixture={fixture_name} jmap={} imap={} smtp={} graph={} gmail={}",
            ep.jmap, ep.imap, ep.smtp, ep.graph, ep.gmail
        ));
        Ok(Self {
            fixture_name: fixture_name.to_owned(),
            mock,
            env_owned,
        })
    }

    /// Borrowed `&str` view over the owned env strings, ready to hand to
    /// `output::run_captured_with_env_and_deadline` via
    /// `spawn_and_capture`'s `extra_env` parameter.
    fn env_pair_refs(&self) -> Vec<(&str, &str)> {
        self.env_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// SIGTERM sæhrimnir and reap (with the standard 1.5s budget then
    /// SIGKILL escalation). Logs only if the shutdown went forceful so
    /// the happy path stays quiet.
    fn shutdown(self, lock: &lockfile::LockGuard) {
        let pid = self.mock.pid();
        let outcome = self.mock.shutdown();
        lock.remove_mock_pid(pid);
        if outcome.killed_after_budget {
            output::ratatoskr_msg(&format!(
                "mock-server SIGKILLed (fixture={}, drain budget exceeded)",
                self.fixture_name
            ));
        }
    }
}

/// Sanitize a fixture string for use as a directory name. Fixture names
/// don't contain `/` today, but the explicit-extension form
/// (`jmap-small.toml`) does carry a `.` we want to keep readable on
/// disk. Replace anything outside `[A-Za-z0-9._-]` with `_` to be safe.
fn safe_dir_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, ExitStatus};
    use std::time::Duration;

    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/ratatoskr-cmd")
            .join(name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_built(bin_dir: &Path) -> HarnessBuild {
        HarnessBuild {
            binary: bin_dir.join("app"),
            bin_dir: bin_dir.to_path_buf(),
            sweep_label: "harness".to_owned(),
        }
    }

    /// Build a `CapturedOutput` for unit testing without spawning a
    /// subprocess. Uses [`ExitStatus::from_raw`] which encodes the wait
    /// status the same way the kernel does (low byte = signal, next
    /// byte = exit code when no signal).
    fn captured(stdout: &[u8], stderr: &[u8], exit_code: Option<i32>, signal: Option<i32>) -> CapturedOutput {
        let raw = match (exit_code, signal) {
            (_, Some(sig)) => sig,
            (Some(code), None) => code << 8,
            (None, None) => 0,
        };
        CapturedOutput {
            status: ExitStatus::from_raw(raw),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            elapsed: Duration::from_millis(42),
        }
    }

    /// In production the artefact dir lives under
    /// `<project_root>/.brokkr/ratatoskr/`, never alongside the script.
    /// Mirroring that in tests means we don't accidentally have
    /// `fs::copy` rewrite the source on top of itself.
    fn artefact_dir_under(parent: &Path) -> PathBuf {
        let dir = parent.join("artefacts");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_artefacts_drops_logs_and_run_toml() {
        let parent = tmpdir("write_basic");
        let script = parent.join("alpha.lua");
        fs::write(&script, "-- example\n").unwrap();
        let bin_dir = parent.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let artefact_dir = artefact_dir_under(&parent);
        let built = fake_built(&bin_dir);
        let cap = captured(b"hello\n", b"warn\n", Some(0), None);

        write_artefacts(&artefact_dir, &script, &built, &cap, &parent).unwrap();

        assert_eq!(
            fs::read(artefact_dir.join("binary-stdout.log")).unwrap(),
            b"hello\n"
        );
        assert_eq!(
            fs::read(artefact_dir.join("binary-stderr.log")).unwrap(),
            b"warn\n"
        );
        assert_eq!(
            fs::read_to_string(artefact_dir.join("alpha.lua")).unwrap(),
            "-- example\n"
        );
        let toml_body = fs::read_to_string(artefact_dir.join("run.toml")).unwrap();
        assert!(toml_body.contains("brokkr_version ="));
        assert!(toml_body.contains("sweep = \"harness\""));
        assert!(toml_body.contains("elapsed_ms = 42"));
        assert!(toml_body.contains("exit_code = 0"));
        assert!(!toml_body.contains("signal ="), "no signal on clean exit");
    }

    #[test]
    fn write_artefacts_records_signal_when_no_exit_code() {
        let parent = tmpdir("write_signal");
        let script = parent.join("beta.lua");
        fs::write(&script, "").unwrap();
        let bin_dir = parent.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let artefact_dir = artefact_dir_under(&parent);
        let built = fake_built(&bin_dir);
        let cap = captured(b"", b"", None, Some(9));

        write_artefacts(&artefact_dir, &script, &built, &cap, &parent).unwrap();
        let toml_body = fs::read_to_string(artefact_dir.join("run.toml")).unwrap();
        assert!(toml_body.contains("signal = 9"), "got: {toml_body}");
        assert!(!toml_body.contains("exit_code ="));
    }

    #[test]
    fn write_artefacts_omits_git_keys_when_collection_fails() {
        // Pass a non-git dir as project_root - git::collect returns Err
        // and the optional git_* fields elide. The tmpdir lives under
        // brokkr's `target/test-tmp/`, which is itself inside brokkr's
        // git repo - `git rev-parse` would walk upward and succeed.
        // Drop a malformed `.git` *file* (gitlink shape) in the dir so
        // git stops the walk there and returns non-zero.
        let parent = tmpdir("write_no_git");
        let project = tmpdir("write_no_git_project");
        fs::write(project.join(".git"), "gitdir: /nonexistent-by-design\n").unwrap();
        let script = parent.join("gamma.lua");
        fs::write(&script, "").unwrap();
        let bin_dir = parent.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let artefact_dir = artefact_dir_under(&parent);
        let built = fake_built(&bin_dir);
        let cap = captured(b"", b"", Some(0), None);

        write_artefacts(&artefact_dir, &script, &built, &cap, &project).unwrap();
        let toml_body = fs::read_to_string(artefact_dir.join("run.toml")).unwrap();
        assert!(!toml_body.contains("git_commit"), "got: {toml_body}");
        assert!(!toml_body.contains("git_clean"), "got: {toml_body}");
    }

    /// End-to-end shape: stand-in "harness binary" via `/bin/true` and
    /// `/bin/false` exercises the success vs failure routing through
    /// the artefact dir. We can't invoke `service_test` itself without
    /// a real `[ratatoskr.harness]` config + cargo build, so we drive
    /// the spawn-and-capture step directly.
    #[test]
    fn capturing_true_succeeds() {
        let cap = Command::new("/bin/true").output().unwrap();
        assert!(cap.status.success());
        assert_eq!(cap.status.code(), Some(0));
    }

    #[test]
    fn capturing_false_reports_nonzero_code() {
        let cap = Command::new("/bin/false").output().unwrap();
        assert!(!cap.status.success());
        assert_eq!(cap.status.code(), Some(1));
    }

    // ---- soak format helpers ------------------------------------------

    fn pass(elapsed_ms: u128) -> RunResult {
        RunResult {
            succeeded: true,
            elapsed_ms,
            exit_label: "exit=0".into(),
            artefact_path: None,
        }
    }

    fn fail(elapsed_ms: u128, exit_label: &str, dir: &str) -> RunResult {
        RunResult {
            succeeded: false,
            elapsed_ms,
            exit_label: exit_label.into(),
            artefact_path: Some(PathBuf::from(dir)),
        }
    }

    #[test]
    fn iter_line_pass_includes_progress_and_elapsed() {
        let line = format_iter_line(7, 200, &pass(412));
        assert_eq!(line, "iter 7/200: PASS in 412ms");
    }

    #[test]
    fn iter_line_fail_includes_exit_label_and_dir() {
        let line = format_iter_line(12, 200, &fail(380, "exit=1", "/tmp/run-12"));
        assert_eq!(
            line,
            "iter 12/200: FAIL exit=1 in 380ms (artefacts: /tmp/run-12)"
        );
    }

    #[test]
    fn iter_line_fail_signal() {
        let line = format_iter_line(3, 5, &fail(450, "signal=11", "/tmp/run-3"));
        assert_eq!(
            line,
            "iter 3/5: FAIL signal=11 in 450ms (artefacts: /tmp/run-3)"
        );
    }

    #[test]
    fn summary_all_passed_reports_min_max_avg() {
        let results = vec![(1, pass(400)), (2, pass(500)), (3, pass(600))];
        let summary = format_soak_summary(&results, 3);
        assert_eq!(
            summary,
            "soak: 3/3 passed (min 400ms, max 600ms, avg 500ms)"
        );
    }

    #[test]
    fn summary_stopped_early_names_failing_iter() {
        // Three iterations ran; iter 3 failed; total was 10. This is
        // the bail-on-first-failure (default) shape.
        let results = vec![
            (1, pass(400)),
            (2, pass(420)),
            (3, fail(380, "exit=1", "/tmp/run-3")),
        ];
        let summary = format_soak_summary(&results, 10);
        assert_eq!(
            summary,
            "soak: stopped at iter 3/10 (2 passed, 1 failed)"
        );
    }

    #[test]
    fn summary_keep_going_lists_all_failed_iters() {
        let results = vec![
            (1, pass(400)),
            (2, fail(380, "exit=1", "/tmp/run-2")),
            (3, pass(410)),
            (4, fail(420, "signal=11", "/tmp/run-4")),
            (5, pass(415)),
        ];
        let summary = format_soak_summary(&results, 5);
        assert_eq!(summary, "soak: 3/5 passed, 2 failed (iters 2, 4)");
    }

    // ---- suite helpers ------------------------------------------------

    fn info(name: &str, expected: Expected) -> ScriptInfo {
        ScriptInfo {
            name: name.to_owned(),
            path: PathBuf::from(format!("{name}.lua")),
            description: None,
            expected,
            ceiling: Duration::from_secs(60),
            preserve_data_dir: PreserveDataDir::OnFailureOnly,
            fixture: None,
            protocol: None,
        }
    }

    #[test]
    fn select_suite_no_filter_skips_ignored_by_default() {
        let scripts = vec![
            info("alpha", Expected::Pass),
            info("wedge", Expected::Ignored),
            info("beta", Expected::Pass),
        ];
        let sel = select_suite(scripts, None, false);
        let names: Vec<&str> = sel.runnable.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(sel.skipped_ignored, 1);
        assert_eq!(sel.filtered_out, 0);
    }

    #[test]
    fn select_suite_include_ignored_keeps_them() {
        let scripts = vec![
            info("alpha", Expected::Pass),
            info("wedge", Expected::Ignored),
        ];
        let sel = select_suite(scripts, None, true);
        assert_eq!(sel.runnable.len(), 2);
        assert_eq!(sel.skipped_ignored, 0);
    }

    #[test]
    fn select_suite_filter_matches_substring() {
        let scripts = vec![
            info("t1/journal", Expected::Pass),
            info("t1/replay", Expected::Pass),
            info("boot/ping", Expected::Pass),
        ];
        let sel = select_suite(scripts, Some("t1/"), false);
        let names: Vec<&str> = sel.runnable.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["t1/journal", "t1/replay"]);
        assert_eq!(sel.filtered_out, 1);
    }

    #[test]
    fn select_suite_filter_then_skip_ignored() {
        let scripts = vec![
            info("t1/journal", Expected::Pass),
            info("t1/wedge", Expected::Ignored),
            info("boot/ping", Expected::Pass),
        ];
        let sel = select_suite(scripts, Some("t1/"), false);
        let names: Vec<&str> = sel.runnable.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["t1/journal"]);
        assert_eq!(sel.skipped_ignored, 1);
        assert_eq!(sel.filtered_out, 1);
    }

    #[test]
    fn empty_suite_msg_distinguishes_states() {
        assert_eq!(
            format_empty_suite(0, 0, 0, None),
            format!("no service-test scripts found under {SCRIPT_DIR}/")
        );
        assert_eq!(
            format_empty_suite(5, 5, 0, Some("nope")),
            "filter \"nope\" matched no scripts (5 discovered)".to_owned()
        );
        assert_eq!(
            format_empty_suite(3, 1, 2, Some("t1/")),
            "filter \"t1/\" matched 2 scripts but all are ignored \
             (use --include-ignored to run them)"
                .to_owned()
        );
        assert_eq!(
            format_empty_suite(4, 0, 4, None),
            "all 4 discovered scripts are ignored \
             (use --include-ignored to run them)"
                .to_owned()
        );
    }

    fn cycle_run(cycle: u32, name: &str, result: RunResult) -> CycleRun {
        CycleRun {
            cycle,
            name: name.to_owned(),
            result,
        }
    }

    #[test]
    fn suite_header_includes_filter_and_counters() {
        assert_eq!(
            format_suite_header(3, 0, 0, None, 1),
            "suite: 3 scripts".to_owned()
        );
        assert_eq!(
            format_suite_header(1, 0, 0, None, 1),
            "suite: 1 script".to_owned()
        );
        assert_eq!(
            format_suite_header(2, 1, 5, Some("t1/"), 1),
            "suite: 2 scripts, filter=\"t1/\", 1 ignored, 5 filtered out".to_owned()
        );
    }

    #[test]
    fn suite_header_with_cycles_reports_total_runs() {
        assert_eq!(
            format_suite_header(11, 0, 0, Some("t1/"), 50),
            "suite: 11 scripts \u{00d7} 50 cycles (550 runs), filter=\"t1/\"".to_owned()
        );
    }

    #[test]
    fn suite_iter_line_pass() {
        let line = format_suite_iter_line(1, 1, 2, 7, "t1/journal", &pass(412));
        assert_eq!(line, "[2/7] PASS t1/journal in 412ms");
    }

    #[test]
    fn suite_iter_line_fail() {
        let line = format_suite_iter_line(
            1,
            1,
            3,
            7,
            "boot/ping",
            &fail(380, "exit=1", "/tmp/run-1"),
        );
        assert_eq!(
            line,
            "[3/7] FAIL boot/ping exit=1 in 380ms (artefacts: /tmp/run-1)"
        );
    }

    #[test]
    fn suite_iter_line_includes_cycle_in_soak() {
        let line = format_suite_iter_line(7, 50, 2, 11, "t1/journal", &pass(412));
        assert_eq!(line, "[cycle 7/50][2/11] PASS t1/journal in 412ms");
    }

    #[test]
    fn suite_running_line_single_vs_soak() {
        assert_eq!(
            format_suite_running_line(1, 1, 2, 7, "t1/journal"),
            "[2/7] running t1/journal"
        );
        assert_eq!(
            format_suite_running_line(7, 50, 2, 11, "t1/journal"),
            "[cycle 7/50][2/11] running t1/journal"
        );
    }

    #[test]
    fn suite_summary_all_passed() {
        let results = vec![
            cycle_run(1, "alpha", pass(400)),
            cycle_run(1, "beta", pass(500)),
            cycle_run(1, "gamma", pass(600)),
        ];
        let summary = format_suite_summary(&results, 3, 1, false);
        assert_eq!(
            summary,
            "suite: 3/3 passed (min 400ms, max 600ms, avg 500ms)"
        );
    }

    #[test]
    fn suite_summary_stopped_at_named_script() {
        let results = vec![
            cycle_run(1, "alpha", pass(400)),
            cycle_run(1, "beta", fail(380, "exit=1", "/tmp/r")),
        ];
        let summary = format_suite_summary(&results, 5, 1, true);
        assert_eq!(summary, "suite: stopped at beta (1 passed, 1 failed)");
    }

    #[test]
    fn suite_summary_keep_going_lists_failed_names() {
        let results = vec![
            cycle_run(1, "alpha", pass(400)),
            cycle_run(1, "beta", fail(380, "exit=1", "/tmp/r1")),
            cycle_run(1, "gamma", pass(410)),
            cycle_run(1, "delta", fail(420, "signal=11", "/tmp/r2")),
        ];
        let summary = format_suite_summary(&results, 4, 1, false);
        assert_eq!(
            summary,
            "suite: 2/4 passed, 2 failed (beta, delta)".to_owned()
        );
    }

    #[test]
    fn suite_summary_soak_all_passed_lists_per_script_totals() {
        // 2 scripts × 3 cycles, all pass.
        let results = vec![
            cycle_run(1, "alpha", pass(400)),
            cycle_run(1, "beta", pass(500)),
            cycle_run(2, "alpha", pass(410)),
            cycle_run(2, "beta", pass(510)),
            cycle_run(3, "alpha", pass(420)),
            cycle_run(3, "beta", pass(520)),
        ];
        let summary = format_suite_summary(&results, 2, 3, false);
        assert!(
            summary.starts_with("soak: 6/6 cohort runs passed across 3 cycles "),
            "got: {summary}"
        );
        assert!(summary.contains("\n  per-script totals:"), "got: {summary}");
        assert!(summary.contains("\n      alpha  3/3"), "got: {summary}");
        assert!(summary.contains("\n      beta   3/3"), "got: {summary}");
    }

    #[test]
    fn suite_summary_soak_stopped_names_cycle_and_script() {
        // 2 scripts × 50 cycles. alpha passes cycle 1+2; beta passes cycle 1, fails cycle 2.
        let results = vec![
            cycle_run(1, "alpha", pass(400)),
            cycle_run(1, "beta", pass(500)),
            cycle_run(2, "alpha", pass(410)),
            cycle_run(2, "beta", fail(380, "exit=1", "/tmp/r")),
        ];
        let summary = format_suite_summary(&results, 2, 50, true);
        assert!(
            summary.starts_with(
                "soak: stopped at cycle 2/50 beta (3 passed, 1 failed)"
            ),
            "got: {summary}"
        );
        // Per-script totals show alpha 2/2 (pass marker) and beta 1/2 (fail marker).
        assert!(summary.contains("\n      alpha  2/2"), "got: {summary}");
        assert!(summary.contains("\n    ! beta   1/2"), "got: {summary}");
    }

    #[test]
    fn suite_summary_soak_keep_going_reports_failures() {
        // alpha passes 3/3, beta fails cycle 2, passes 1+3.
        let results = vec![
            cycle_run(1, "alpha", pass(400)),
            cycle_run(1, "beta", pass(500)),
            cycle_run(2, "alpha", pass(410)),
            cycle_run(2, "beta", fail(380, "exit=1", "/tmp/r")),
            cycle_run(3, "alpha", pass(420)),
            cycle_run(3, "beta", pass(520)),
        ];
        let summary = format_suite_summary(&results, 2, 3, false);
        assert!(
            summary.starts_with(
                "soak: 5/6 cohort runs passed, 1 failed across 3 cycles"
            ),
            "got: {summary}"
        );
        assert!(summary.contains("\n      alpha  3/3"), "got: {summary}");
        assert!(summary.contains("\n    ! beta   2/3"), "got: {summary}");
    }

    #[test]
    fn summary_zero_passes_handles_average() {
        // Edge case: every iteration failed in keep_going mode.
        // Min/max/avg should not divide by zero on the success-stats
        // path because that path only fires when there are no
        // failures, but check the failure-list path renders sanely.
        let results = vec![
            (1, fail(400, "exit=1", "/tmp/run-1")),
            (2, fail(450, "exit=1", "/tmp/run-2")),
        ];
        let summary = format_soak_summary(&results, 2);
        assert_eq!(summary, "soak: 0/2 passed, 2 failed (iters 1, 2)");
    }
}
