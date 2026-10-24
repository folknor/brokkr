//! Service cohort orchestration and shared fixture sessions.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::artefacts::{self, ArtefactDir};
use crate::clean::layout::ProjectState;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::DevError;
use crate::harness_build::{self, HarnessBuild};
use crate::lockfile;
use crate::output::{self, CapturedOutput};
use crate::ratatoskr::discover::{self, PreserveDataDir, SCRIPT_DIR, ScriptInfo, Selection};
use crate::ratatoskr::saehrimnir::{ManagedMock, MockArtifactScope, MockPaths, Protocol};

use super::single::{RunResult, spawn_and_capture};
#[cfg(test)]
use super::single::{classify_capture, format_iter_line, format_soak_summary};

/// `brokkr service --all` - run every discovered script (optionally
/// filtered) against a single shared harness build.
///
/// Discovery + filter happens first so we can bail with a useful message
/// when nothing matches before paying the cargo build cost. `expected =
/// ignored` scripts are skipped by default; `--include-ignored` opts
/// them in. Each script runs through the same `spawn_and_capture` path
/// the single-script shape uses, so artefact-dir lifecycle and ceiling
/// semantics are identical. The summary at the end lists failed scripts by name -
/// not iter index, since each script ran exactly once.
#[allow(clippy::too_many_lines)] // linear orchestration: discover, validate, build, run with fixture grouping, summarize
pub(crate) fn service_suite(req: &crate::request::ServiceSuiteRequest<'_>) -> Result<(), DevError> {
    let crate::request::ServiceSuiteRequest {
        project_root,
        build_root,
        included_config_files,
        ratatoskr,
        filter,
        keep_artefacts,
        profile_override,
        keep_going,
        include_ignored,
        repeat,
    } = *req;
    let cycles = repeat.get();
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

    let harness_cfg = crate::ratatoskr::require_harness_config(ratatoskr, "service")?;

    // Fail fast if any selected script needs a fixture but the
    // [ratatoskr] mock-server config isn't populated. Spawn-time would
    // catch this too but reporting it before the build wastes nothing.
    let mock_paths = if let Some(needy) = runnable.iter().find(|s| s.fixture().is_some()) {
        Some(MockPaths::resolve(ratatoskr, project_root).map_err(|e| {
            DevError::Config(format!(
                "service: script {} needs a mock fixture but {e}",
                needy.name
            ))
        })?)
    } else {
        None
    };
    let mut fixture_paths = HashMap::new();
    if let Some(paths) = &mock_paths {
        for script in &runnable {
            if let Some(fixture_name) = script.fixture() {
                let path = paths.fixture(fixture_name).map_err(|e| {
                    DevError::Config(format!(
                        "service: script {} declares fixture `{fixture_name}` but {e}",
                        script.name
                    ))
                })?;
                fixture_paths.insert(fixture_name.to_owned(), path);
            }
        }
    }

    let _lock = crate::context::reenter_command_lock()?;
    // Cooperative SIGTERM for `brokkr kill`. See run_sync_smoke for rationale.
    let sigterm = crate::shutdown::SigtermGuard::install();

    let built = harness_build::build_for_harness(
        build_root.as_path(),
        harness_cfg,
        crate::ratatoskr::harness_profile(profile_override, harness_cfg),
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        crate::shutdown::ChildIsolation::ProcessGroup(&sigterm),
    )?;
    output::ratatoskr_msg(&format_suite_header(
        runnable.len(),
        skipped_ignored,
        filtered_out,
        filter,
        cycles,
    ));

    let state = ProjectState::new(project_root);
    let ratatoskr_layout = state.ratatoskr();
    let artefact_parent = ratatoskr_layout.service_artifacts();
    let total = runnable.len();
    let total_runs = u32::try_from(total)
        .unwrap_or(u32::MAX)
        .saturating_mul(cycles);
    let mut results: Vec<CycleRun> = Vec::with_capacity(total * cycles as usize);
    let mut bailed = false;
    // Suite-scoped fixture reuse: one sæhrimnir per distinct fixture
    // declared by any selected script, kept alive for every cycle. A
    // no-fixture script in the middle of a fixture-X run leaves the
    // fixture-X mock alone but receives NO endpoint env vars (its
    // contract is "act as if no fixture exists"). Fixture isolation is
    // a per-script concern - scripts wanting a clean slate hit their
    // own /test/<protocol>/reset endpoint at start-of-test.
    let mut mocks: HashMap<String, FixtureSession<'_>> = HashMap::new();

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
                if let Some(fixture_name) = script.fixture()
                    && !mocks.contains_key(fixture_name)
                {
                    let paths = mock_paths.as_ref().ok_or_else(|| {
                        DevError::Internal(
                            "service fixture passed preflight without mock paths".into(),
                        )
                    })?;
                    let fixture_path = fixture_paths.get(fixture_name).ok_or_else(|| {
                        DevError::Internal(format!(
                            "service fixture `{fixture_name}` passed preflight without a resolved path"
                        ))
                    })?;
                    let session = FixtureSession::start(
                        project_root,
                        ratatoskr,
                        fixture_name,
                        paths,
                        fixture_path,
                        &_lock,
                    )?;
                    mocks.insert(fixture_name.to_owned(), session);
                }

                // ArtefactDir test_id rejects `/` in the name; nested scripts
                // like `t1/journal_replays` collapse to the file stem for the
                // artefact dir while keeping the full relative name in output.
                // Two scripts with the same stem under different parents would
                // share an artefact-dir prefix and just allocate run-(N+1)/ -
                // not ideal, but mirrors how the single-script shape handles
                // arbitrary script paths today.
                let test_id = script
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&script.name)
                    .to_owned();
                let keep_on_success =
                    keep_artefacts || script.preserve_data_dir == PreserveDataDir::OnSuccessToo;

                // No per-script "running" line: the result line lands as
                // soon as the script exits (one line per run, like `sync
                // --all`), the ceiling bounds how long a hang can stay
                // silent, and `brokkr lock` shows live progress + PID for
                // the in-flight script from another shell.
                let run_index = (cycle - 1)
                    .saturating_mul(u32::try_from(total).unwrap_or(u32::MAX))
                    .saturating_add(u32::try_from(pos).unwrap_or(u32::MAX));
                _lock.set_progress(run_index, total_runs);
                let artefacts = ArtefactDir::allocate(&artefact_parent, &test_id, keep_on_success)?;
                // Inject endpoint env vars only if THIS script declares a
                // fixture. No-fixture scripts run as if no mock exists,
                // even when other scripts in the suite have mocks alive.
                let env_pairs = script
                    .fixture()
                    .and_then(|name| mocks.get(name))
                    .map(FixtureSession::env_pair_refs)
                    .unwrap_or_default();
                let result = spawn_and_capture(
                    artefacts,
                    &built,
                    &script.path,
                    project_root,
                    build_root.as_path(),
                    included_config_files,
                    script.ceiling,
                    &env_pairs,
                    &_lock,
                )?;
                output::ratatoskr_msg(&format_suite_iter_line(
                    cycle,
                    cycles,
                    pos,
                    total,
                    &script.name,
                    &result,
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
    // standard graceful-shutdown budget; the alternative (FixtureSession::Drop on
    // error unwind) is the SIGKILL fast-path. We do this BEFORE
    // propagating the error so a failing suite doesn't leave the user
    // with an SIGKILLed mock and uninformative artefacts.
    let mut shutdown_error = None;
    for (_, session) in mocks.drain() {
        if let Err(error) = session.shutdown()
            && shutdown_error.is_none()
        {
            shutdown_error = Some(error);
        }
    }

    loop_result?;
    if let Some(error) = shutdown_error {
        return Err(error);
    }

    output::ratatoskr_msg(&format_suite_summary(&results, total, cycles, bailed));

    if results.iter().any(|r| !r.result.succeeded) {
        artefacts::emit_clean_hint(output::ratatoskr_msg);
        Err(DevError::Reported("service suite failed".into()))
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
pub(super) fn directory_filter(project_root: &Path, dir: &Path) -> Result<String, DevError> {
    let canon = dir.canonicalize().map_err(|e| {
        DevError::Config(format!(
            "service: failed to canonicalize directory {}: {e}",
            dir.display()
        ))
    })?;
    let script_root = project_root.join(SCRIPT_DIR);
    let script_root_canon = script_root.canonicalize().map_err(|e| {
        DevError::Config(format!(
            "service: failed to canonicalize script root {}: {e}. \
             Is the harness directory present?",
            script_root.display()
        ))
    })?;
    let rel = canon.strip_prefix(&script_root_canon).map_err(|_| {
        DevError::Config(format!(
            "service: directory {} is not under {}",
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
        if !include_ignored && script.selection == Selection::Skip {
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
    let mut head = format!(
        "suite: {runnable} script{}",
        if runnable == 1 { "" } else { "s" }
    );
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
        return format!("no service-harness scripts found under {SCRIPT_DIR}/");
    }
    if let Some(f) = filter {
        if filtered_out == total_discovered {
            return format!("filter \"{f}\" matched no scripts ({total_discovered} discovered)");
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
        format!(
            "{prefix} PASS {name} in {}",
            crate::output::format_duration_ms(result.elapsed_ms)
        )
    } else {
        let dir = result
            .artefact_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<missing>".to_owned());
        format!(
            "{prefix} FAIL {name} {} in {} (artefacts: {dir})",
            result.exit_label,
            crate::output::format_duration_ms(result.elapsed_ms)
        )
    }
}

/// Trailing summary for the suite. Pure for testability.
///
/// Single-cycle (`cycles == 1`) keeps the existing one-liner shapes so
/// cohort logs don't churn for the common case. Soak mode
/// (`cycles > 1`) emits a cohort-level header line followed by an
/// indented per-script `pass/total` table so the user can see which
/// script broke without scrolling through 500 status lines.
fn format_suite_summary(results: &[CycleRun], total: usize, cycles: u32, bailed: bool) -> String {
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
             (min {}, max {}, avg {})",
            crate::output::format_duration_ms(min),
            crate::output::format_duration_ms(max),
            crate::output::format_duration_ms(avg)
        )
    } else if bailed {
        // First failure stops the cohort; report the cycle and script.
        let first_fail = results
            .iter()
            .find(|r| !r.result.succeeded)
            .expect("bailed implies a failure");
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
        head.push_str(&format!(
            "\n    {marker} {name:<name_width$}  {pass}/{runs}"
        ));
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
        format!(
            "suite: {pass_count}/{total} passed (min {}, max {}, avg {})",
            crate::output::format_duration_ms(min),
            crate::output::format_duration_ms(max),
            crate::output::format_duration_ms(avg)
        )
    } else if bailed {
        let first_fail = fail_names[0];
        format!("suite: stopped at {first_fail} ({pass_count} passed, {fail_count} failed)")
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
    features: String,
    /// The cargo profile the harness was built under.
    profile: String,
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
pub(super) fn write_artefacts(
    artefact_dir: &Path,
    script_abs: &Path,
    built: &HarnessBuild,
    captured: &CapturedOutput,
    build_root: &Path,
    included_config_files: &[std::path::PathBuf],
) -> Result<(), DevError> {
    let git = crate::git::collect_with_config_paths(build_root, included_config_files).ok();
    write_artefacts_with_git(artefact_dir, script_abs, built, captured, git.as_ref())
}

fn write_artefacts_with_git(
    artefact_dir: &Path,
    script_abs: &Path,
    built: &HarnessBuild,
    captured: &CapturedOutput,
    git: Option<&crate::git::GitInfo>,
) -> Result<(), DevError> {
    fs::write(artefact_dir.join("binary-stdout.log"), &captured.stdout)?;
    fs::write(artefact_dir.join("binary-stderr.log"), &captured.stderr)?;

    let script_filename: PathBuf = script_abs
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("script.lua"));
    fs::copy(script_abs, artefact_dir.join(&script_filename))?;

    let meta = RunMetadata {
        brokkr_version: env!("CARGO_PKG_VERSION").to_owned(),
        script: script_abs.display().to_string(),
        binary: built.binary.display().to_string(),
        features: built.features_label.clone(),
        profile: built.profile.label().to_owned(),
        elapsed_ms: u64::try_from(captured.elapsed.as_millis()).unwrap_or(u64::MAX),
        exit_code: captured.code(),
        signal: captured.signal(),
        git_commit: git.map(|g| g.commit.as_str().to_owned()),
        git_subject: git.map(|g| g.subject.clone()),
        git_clean: git.map(|g| g.is_clean),
    };
    let serialized = toml::to_string(&meta)
        .map_err(|e| DevError::Config(format!("service: failed to serialize run.toml: {e}")))?;
    fs::write(artefact_dir.join("run.toml"), serialized)?;
    Ok(())
}

/// Owns a running sæhrimnir process plus the per-protocol endpoint env
/// pairs that should be injected into the harness binary. Created when a
/// service-harness script declares a `-- fixture:` line; dropped (or
/// gracefully `shutdown`) once every dependent harness invocation has
/// returned.
///
/// The single-script shape creates one before iteration and drains it
/// after; the cohort creates one per distinct fixture, lazily on first
/// use (see the suite loop's `mocks` map).
pub(super) struct FixtureSession<'a> {
    mock: ManagedMock<'a>,
}

impl<'a> FixtureSession<'a> {
    /// Start a preflighted fixture in the service-wide artifact location.
    pub(super) fn start(
        project_root: &Path,
        cfg: &RatatoskrConfig,
        fixture_name: &str,
        paths: &MockPaths,
        fixture_path: &Path,
        lock: &'a lockfile::LockGuard,
    ) -> Result<Self, DevError> {
        let mock = ManagedMock::start(
            cfg,
            &paths.binary,
            fixture_path,
            fixture_name,
            MockArtifactScope::SharedFixture {
                project_root,
                fixture_name,
            },
            lock,
        )?;
        let ep = mock.endpoints();
        let endpoint_ports = Protocol::all()
            .map(|protocol| format!("{}={}", protocol.name(), ep.port(protocol)))
            .collect::<Vec<_>>()
            .join(" ");
        output::ratatoskr_msg(&format!(
            "mock-server up: fixture={fixture_name} {endpoint_ports}"
        ));
        Ok(Self { mock })
    }

    /// Borrowed `&str` view over the owned env strings, ready to hand to
    /// `output::run_captured_with_env_and_deadline` via
    /// `spawn_and_capture`'s `extra_env` parameter.
    pub(super) fn env_pair_refs(&self) -> Vec<(&str, &str)> {
        self.mock.env_pair_refs()
    }

    /// SIGTERM sæhrimnir and reap (with the standard graceful budget then
    /// SIGKILL escalation). Logs only if the shutdown went forceful so
    /// the happy path stays quiet.
    pub(super) fn shutdown(self) -> Result<(), DevError> {
        self.mock.shutdown().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use crate::ratatoskr::discover::ScriptKind;

    use super::*;

    /// A fresh scratch directory for one test. `name` must be unique in
    /// this module - the allocator asserts it.
    fn tmpdir(name: &str) -> PathBuf {
        crate::test_scratch::scratch("ratatoskr-cmd", name)
    }

    fn fake_built(bin_dir: &Path) -> HarnessBuild {
        HarnessBuild {
            binary: bin_dir.join("app"),
            bin_dir: bin_dir.to_path_buf(),
            features_label: "default".to_owned(),
            profile: crate::build::RustProfile::Release,
            identity: crate::measure::BuildIdentity {
                profile: crate::build::BuildKind::Cargo(crate::build::RustProfile::Release),
                features: None,
                provenance: crate::build_provenance::BuildProvenance::default(),
            },
        }
    }

    /// Build a `CapturedOutput` for unit testing without spawning a
    /// subprocess. A signal wins over an exit code, as it does for a real
    /// child.
    fn captured(
        stdout: &[u8],
        stderr: &[u8],
        exit_code: Option<i32>,
        signal: Option<i32>,
    ) -> CapturedOutput {
        let ending = match (exit_code, signal) {
            (_, Some(sig)) => crate::output::Ending::Signalled(sig),
            (Some(code), None) => crate::output::Ending::Exited(code),
            (None, None) => crate::output::Ending::Exited(0),
        };
        CapturedOutput {
            ending,
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

        write_artefacts(&artefact_dir, &script, &built, &cap, &parent, &[]).unwrap();

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
        assert!(toml_body.contains("features = \"default\""));
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

        write_artefacts(&artefact_dir, &script, &built, &cap, &parent, &[]).unwrap();
        let toml_body = fs::read_to_string(artefact_dir.join("run.toml")).unwrap();
        assert!(toml_body.contains("signal = 9"), "got: {toml_body}");
        assert!(!toml_body.contains("exit_code ="));
    }

    #[test]
    fn write_artefacts_omits_git_fields_without_git_metadata() {
        let parent = tmpdir("write_no_git");
        let script = parent.join("gamma.lua");
        fs::write(&script, "").unwrap();
        let bin_dir = parent.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let artefact_dir = artefact_dir_under(&parent);
        let built = fake_built(&bin_dir);
        let cap = captured(b"", b"", Some(0), None);

        write_artefacts_with_git(&artefact_dir, &script, &built, &cap, None).unwrap();
        let toml_body = fs::read_to_string(artefact_dir.join("run.toml")).unwrap();
        assert!(!toml_body.contains("git_commit"), "got: {toml_body}");
        assert!(!toml_body.contains("git_clean"), "got: {toml_body}");
    }

    #[test]
    fn capture_classifier_accepts_only_clean_success() {
        let ceiling = Duration::from_secs(1);
        let (succeeded, label) =
            classify_capture(&captured(b"", b"", Some(0), None), false, ceiling);
        assert!(succeeded);
        assert_eq!(label, "exit=0");

        let (succeeded, label) =
            classify_capture(&captured(b"", b"", Some(1), None), false, ceiling);
        assert!(!succeeded);
        assert_eq!(label, "exit=1");
    }

    #[test]
    fn capture_classifier_marks_deadline_as_failure_even_with_zero_exit() {
        let (succeeded, label) = classify_capture(
            &captured(b"", b"", Some(0), None),
            true,
            Duration::from_secs(1),
        );
        assert!(!succeeded);
        assert_eq!(label, "ceiling=1s");
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
        assert_eq!(summary, "soak: stopped at iter 3/10 (2 passed, 1 failed)");
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

    fn info(name: &str, selection: Selection) -> ScriptInfo {
        ScriptInfo {
            name: name.to_owned(),
            path: PathBuf::from(format!("{name}.lua")),
            description: None,
            selection,
            ceiling: Duration::from_secs(60),
            preserve_data_dir: PreserveDataDir::OnFailureOnly,
            kind: ScriptKind::Service {
                fixture: None,
                protocol: None,
            },
        }
    }

    #[test]
    fn select_suite_no_filter_skips_ignored_by_default() {
        let scripts = vec![
            info("alpha", Selection::Run),
            info("wedge", Selection::Skip),
            info("beta", Selection::Run),
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
            info("alpha", Selection::Run),
            info("wedge", Selection::Skip),
        ];
        let sel = select_suite(scripts, None, true);
        assert_eq!(sel.runnable.len(), 2);
        assert_eq!(sel.skipped_ignored, 0);
    }

    #[test]
    fn select_suite_filter_matches_substring() {
        let scripts = vec![
            info("t1/journal", Selection::Run),
            info("t1/replay", Selection::Run),
            info("boot/ping", Selection::Run),
        ];
        let sel = select_suite(scripts, Some("t1/"), false);
        let names: Vec<&str> = sel.runnable.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["t1/journal", "t1/replay"]);
        assert_eq!(sel.filtered_out, 1);
    }

    #[test]
    fn select_suite_filter_then_skip_ignored() {
        let scripts = vec![
            info("t1/journal", Selection::Run),
            info("t1/wedge", Selection::Skip),
            info("boot/ping", Selection::Run),
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
            format!("no service-harness scripts found under {SCRIPT_DIR}/")
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
        let line =
            format_suite_iter_line(1, 1, 3, 7, "boot/ping", &fail(380, "exit=1", "/tmp/run-1"));
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
            summary.starts_with("soak: stopped at cycle 2/50 beta (3 passed, 1 failed)"),
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
            summary.starts_with("soak: 5/6 cohort runs passed, 1 failed across 3 cycles"),
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
