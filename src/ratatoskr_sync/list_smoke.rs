// Plan-3 sync orchestration: `sync-list` and `sync-smoke`.
//
// `sync-list` is pure brokkr - walk the configured sync-script
// directory, parse frontmatter, print a sorted table. No ratatoskr or
// sæhrimnir runtime dependency.
//
// `sync-smoke` builds the harness binary per `[ratatoskr.harness]`,
// spawns sæhrimnir against the script's declared fixture, parses the
// per-protocol ports out of the readiness sentinel, then spawns
// `<harness binary> --test-harness <SCRIPT>` with the
// `RATATOSKR_TEST_*_ENDPOINT` env-var family injected (only those
// whose names ratatoskr's `brokkr.toml` has spelled out). When the
// harness exits, brokkr SIGTERMs sæhrimnir with the
// [`saehrimnir::SHUTDOWN_BUDGET`] before escalating to SIGKILL.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::build::CargoProfile;
use crate::config::{DevConfig, GateConfig, RatatoskrConfig};
use crate::context;
use crate::db::gate::{GateDb, GateRow};
use crate::db::{KvPair, KvValue};
use crate::error::DevError;
use crate::git;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::lockfile::{self, LockContext};
use crate::output;
use crate::project::Project;
use crate::artefacts::{self, ArtefactDir};
use crate::ratatoskr::build::{self, HarnessBuild};
use crate::ratatoskr::discover::{self, ScriptInfo};
use crate::ratatoskr::gate as gate_eval;
use crate::sidecar;
use crate::ratatoskr::saehrimnir::{
    endpoint_env_pairs, require_path, resolve_fixture, MockOutcome, MockServer,
};

/// Default location for sync-test scripts inside ratatoskr's tree, used
/// when `[ratatoskr] sync_script_dir` is unset. Sibling to the
/// service-harness directory; same `.lua` + frontmatter shape.
const DEFAULT_SYNC_SCRIPT_DIR: &str = "crates/app/tests/sync-harness";

/// Where per-test sync artefact dirs live relative to the project root.
const SYNC_ARTEFACT_PARENT: &str = ".brokkr/ratatoskr/sync";

// ---------------------------------------------------------------------------
// sync-list
// ---------------------------------------------------------------------------

/// `brokkr sync-list` - discover sync-test scripts under the configured
/// directory and print a sorted table. Empty-state message names the
/// expected directory so a fresh checkout (no harness scripts yet) gets
/// a useful response.
pub fn run_sync_list(project_root: &Path, dev_config: &DevConfig) -> Result<(), DevError> {
    let dir = sync_script_dir(project_root, dev_config.ratatoskr.as_ref());
    let scripts = discover::discover_at(&dir)?;
    let display_dir = dir.display();

    if scripts.is_empty() {
        output::ratatoskr_msg(&format!("no sync-test scripts found under {display_dir}"));
        output::ratatoskr_msg(
            "  (the sync-harness module / cohort has not landed in ratatoskr yet, or no scripts have been added)",
        );
        return Ok(());
    }

    output::ratatoskr_msg(&format!(
        "  {:<32} {:<10} {:<14} {:<10} {}",
        "Name", "Expected", "Fixture", "Protocol", "Description",
    ));
    output::ratatoskr_msg(&format!("  {}", "\u{2500}".repeat(78)));
    for ScriptInfo {
        name,
        description,
        expected,
        fixture,
        protocol,
        ..
    } in &scripts
    {
        output::ratatoskr_msg(&format!(
            "  {:<32} {:<10} {:<14} {:<10} {}",
            name,
            expected.as_str(),
            fixture.as_deref().unwrap_or("\u{2014}"),
            protocol.as_deref().unwrap_or("\u{2014}"),
            description.as_deref().unwrap_or("\u{2014}"),
        ));
    }
    Ok(())
}

/// Resolve the sync-script directory: explicit `[ratatoskr]
/// sync_script_dir` if set (relative paths join against the project
/// root), else the [`DEFAULT_SYNC_SCRIPT_DIR`] convention.
fn sync_script_dir(project_root: &Path, cfg: Option<&RatatoskrConfig>) -> PathBuf {
    let configured = cfg.and_then(|c| c.sync_script_dir.as_ref());
    match configured {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => project_root.join(p),
        None => project_root.join(DEFAULT_SYNC_SCRIPT_DIR),
    }
}

// ---------------------------------------------------------------------------
// sync-smoke
// ---------------------------------------------------------------------------

/// CLI inputs for `brokkr sync-smoke`. Pulled out so the orchestration
/// body can be smoke-tested with synthetic paths if needed.
pub struct SyncSmokeRequest<'a> {
    pub project_root: &'a Path,
    pub dev_config: &'a DevConfig,
    pub script: &'a str,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
}

/// Drive `brokkr sync-smoke` end-to-end:
///
/// 1. Validate config: `[ratatoskr.harness]`, `mock_server_binary`, and
///    `fixtures_dir` are all required. Endpoint env-var names are
///    optional - protocols without a configured spelling just don't
///    get an env var.
/// 2. Parse the script's frontmatter; require a `fixture: <NAME>`.
/// 3. Acquire the global lockfile.
/// 4. Build the harness binary per `[ratatoskr.harness]`.
/// 5. Allocate `.brokkr/ratatoskr/sync/<test>/run-N/` with `harness/`
///    and `mock/` subdirs.
/// 6. Spawn sæhrimnir with `--fixture <PATH>` + `--readiness-file
///    mock/readiness`; pipe its stderr to `mock/stderr.log` (its
///    primary log channel per plan 2).
/// 7. Wait for the readiness sentinel, parse endpoints.
/// 8. Spawn the harness binary with `BROKKR_HARNESS_ARTEFACT_DIR` ->
///    `harness/`, `BROKKR_TEST_BIN_DIR` -> the build's bin dir, plus
///    one `RATATOSKR_TEST_<PROTO>_ENDPOINT=...` per configured spelling.
///    Wait for exit (no ceiling for v0 - frontmatter ceiling can land
///    later if needed; matches the "smoke, not bench" framing).
/// 9. Tear down sæhrimnir: SIGTERM, [`SHUTDOWN_BUDGET`], SIGKILL.
/// 10. PASS/FAIL on the harness binary's exit code; sæhrimnir's outcome
///     is logged but not gating (a script may legitimately tear it down
///     early in scenarios).
#[allow(clippy::too_many_lines)] // linear orchestration: validate, build, allocate artefacts, run, finalize
pub fn run_sync_smoke(req: &SyncSmokeRequest<'_>) -> Result<(), DevError> {
    let cfg = req.dev_config.ratatoskr.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync-smoke: no [ratatoskr] section in brokkr.toml. \
             Required to locate sæhrimnir and the harness binary."
                .into(),
        )
    })?;
    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync-smoke: no [ratatoskr.harness] section in brokkr.toml. \
             Declare it with `package = \"<crate>\"` (and optional \
             `binary`, `features`, `debug`)."
                .into(),
        )
    })?;
    let mock_binary = require_path(&cfg.mock_server_binary, req.project_root, "mock_server_binary")?;
    let fixtures_dir = require_path(&cfg.fixtures_dir, req.project_root, "fixtures_dir")?;
    if !mock_binary.exists() {
        return Err(DevError::Config(format!(
            "sync-smoke: sæhrimnir binary not found at {}. Build it first.",
            mock_binary.display()
        )));
    }

    let script_path = Path::new(req.script);
    if !script_path.is_file() {
        return Err(DevError::Config(format!(
            "sync-smoke: script not found or not a file: {}",
            req.script
        )));
    }
    let script_abs = script_path.canonicalize().map_err(|e| {
        DevError::Config(format!("sync-smoke: canonicalize script: {e}"))
    })?;
    let test_id = script_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            DevError::Config(format!("sync-smoke: script has no stem: {}", req.script))
        })?
        .to_owned();

    let parsed = discover::parse_script(&script_abs, &test_id).map_err(|e| {
        DevError::Config(format!("sync-smoke: parse script: {e}"))
    })?;
    let fixture_name = parsed.fixture.as_ref().ok_or_else(|| {
        DevError::Config(format!(
            "sync-smoke: script {test_id} has no `-- fixture: <NAME>` frontmatter line. \
             Required so brokkr knows which sæhrimnir fixture to load."
        ))
    })?;
    let fixture_path = resolve_fixture(&fixtures_dir, fixture_name)?;

    let project_root_str = req.project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "sync-smoke",
        project_root: &project_root_str,
    })?;
    // Cooperative SIGTERM for `brokkr kill`. Installed right after the
    // lock so every captured subprocess from here on - cargo build,
    // sæhrimnir spawn (no flag-poll, but Drop will hard-kill on unwind),
    // the harness binary - sees the flag-poll path in
    // `output::run_captured_with_env_and_deadline`. Drops at function
    // end, before `_lock`.
    let _sigterm = crate::shutdown::SigtermGuard::install();

    let debug = req.profile_override.unwrap_or_else(|| harness_cfg.debug.unwrap_or(false));
    let built = build::build_for_harness(
        req.project_root,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true, // isolate_pg: SigtermGuard above bridges terminal signals
    )?;
    output::ratatoskr_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display(),
    ));

    let artefact_parent = req.project_root.join(SYNC_ARTEFACT_PARENT);
    let artefacts = ArtefactDir::allocate(&artefact_parent, &test_id, req.keep_artefacts)?;
    let harness_dir = artefacts.path().join("harness");
    let mock_dir = artefacts.path().join("mock");
    fs::create_dir_all(&harness_dir).map_err(DevError::Io)?;
    fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;

    output::ratatoskr_msg(&format!("running {test_id} (fixture: {fixture_name})"));

    let mut timings = PhaseTimings::default();
    let outcome = orchestrate(
        req,
        cfg,
        &built,
        &mock_binary,
        &fixture_path,
        &script_abs,
        &harness_dir,
        &mock_dir,
        &mut timings,
        &_lock,
    );

    let summary = timings.summary();
    match outcome {
        Ok(()) => {
            output::ratatoskr_msg(&format!("PASS{summary}"));
            artefacts.finalize_success()?;
            Ok(())
        }
        Err(e) => {
            output::ratatoskr_msg(&format!("FAIL{summary}: {e}"));
            let path = artefacts.path().to_path_buf();
            artefacts.finalize_failure();
            output::ratatoskr_msg(&format!("artefacts preserved at {}", path.display()));
            artefacts::emit_clean_hint();
            Err(e)
        }
    }
}

/// Per-phase wall-clock timings for sync-smoke. Each field is `None`
/// until the phase completes, so a spawn-side failure still produces a
/// faithful summary (e.g. `FAIL in 0.4s (mock 0.4s)` if sæhrimnir died
/// during readiness).
#[derive(Default)]
struct PhaseTimings {
    mock_ready: Option<Duration>,
    harness: Option<Duration>,
    mock_shutdown: Option<Duration>,
}

impl PhaseTimings {
    /// Render the trailing summary `(...)` clause for the PASS/FAIL line.
    /// Returns an empty string when no phases recorded - keeps the
    /// pre-spawn config-error path tidy.
    fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut total = Duration::ZERO;
        if let Some(d) = self.mock_ready {
            parts.push(format!("mock {}", format_secs(d)));
            total += d;
        }
        if let Some(d) = self.harness {
            parts.push(format!("harness {}", format_secs(d)));
            total += d;
        }
        if let Some(d) = self.mock_shutdown {
            parts.push(format!("shutdown {}", format_secs(d)));
            total += d;
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" in {} ({})", format_secs(total), parts.join(", "))
        }
    }
}

fn format_secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

/// The two-child orchestration body. Pulled into its own function so
/// the artefact-dir finalize calls are unconditional - even if this
/// returns early on a spawn error, [`run_sync_smoke`] still records
/// PASS/FAIL via the artefact-dir lifecycle.
#[allow(clippy::too_many_arguments)]
fn orchestrate(
    req: &SyncSmokeRequest<'_>,
    cfg: &RatatoskrConfig,
    built: &HarnessBuild,
    mock_binary: &Path,
    fixture_path: &Path,
    script_abs: &Path,
    harness_dir: &Path,
    mock_dir: &Path,
    timings: &mut PhaseTimings,
    lock: &lockfile::LockGuard,
) -> Result<(), DevError> {
    // Publish the mock PID from INSIDE spawn_observed - before the
    // readiness wait - so a `brokkr kill --hard` landing during
    // sæhrimnir startup finds the mock and SIGKILLs it instead of
    // orphaning it.
    let mock = MockServer::spawn_observed(
        mock_binary,
        fixture_path,
        mock_dir,
        Some(&|pid| lock.add_mock_pid(pid)),
        Some(&|pid| lock.remove_mock_pid(pid)),
        true, // isolate_pg: sync-smoke's outer SigtermGuard covers this
    )?;
    // Don't seed `child_pid` with the mock's PID - the captured runner's
    // `on_spawn` callback will publish the harness PID seconds from now,
    // and a transient `child_pid == mock_pid` window means a `--hard`
    // landing in that gap would SIGKILL the mock twice (once via
    // `mock_pid`, once via `child_pid`) and the harness not at all.
    timings.mock_ready = Some(mock.ready_elapsed());
    let endpoint_envs = endpoint_env_pairs(cfg, mock.endpoints());

    let bin_dir_str = built.bin_dir.display().to_string();
    let harness_dir_str = harness_dir.display().to_string();
    let script_str = script_abs.display().to_string();

    let mut env_pairs: Vec<(&str, &str)> = vec![
        ("BROKKR_HARNESS_ARTEFACT_DIR", &harness_dir_str),
        ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
    ];
    for (name, value) in &endpoint_envs {
        env_pairs.push((name.as_str(), value.as_str()));
    }

    // No ceiling for v0 - sync-smoke is the smoke shape, not the bench
    // shape. Use the script's frontmatter ceiling if set, else a
    // generous default so a hung script doesn't wedge the lockfile
    // forever.
    let ceiling = parsed_ceiling(script_abs)?;

    let binary_str = built.binary.display().to_string();
    let deadline_capture = output::run_captured_with_env_and_deadline(
        &binary_str,
        &["--test-harness", &script_str],
        req.project_root,
        &env_pairs,
        ceiling,
        Some(&|pid| lock.set_child_pid(pid)),
        true, // isolate_pg: outer SigtermGuard active
    );

    // Capture harness elapsed before tearing down sæhrimnir so a
    // ceiling-kill or non-zero exit still surfaces a harness duration in
    // the summary line.
    if let Ok(dc) = deadline_capture.as_ref() {
        timings.harness = Some(dc.captured.elapsed);
    }

    // Whatever the harness did, sæhrimnir gets torn down next.
    let mock_outcome = mock.shutdown();
    // sync-smoke has at most one mock alive at a time; clear all is the
    // honest call after that single mock drains.
    lock.clear_mock_pids();
    lock.clear_child_pid();
    timings.mock_shutdown = Some(mock_outcome.shutdown_elapsed);

    let dc = deadline_capture?;
    fs::write(harness_dir.join("binary-stdout.log"), &dc.captured.stdout).map_err(DevError::Io)?;
    fs::write(harness_dir.join("binary-stderr.log"), &dc.captured.stderr).map_err(DevError::Io)?;
    write_run_toml(harness_dir, mock_dir, script_abs, built, &dc, &mock_outcome)?;

    if dc.killed_on_deadline {
        return Err(DevError::Config(format!(
            "harness binary exceeded ceiling {ceiling:?}"
        )));
    }
    if !dc.captured.status.success() {
        return Err(DevError::Config(format!(
            "harness binary exited with {:?}",
            dc.captured.status
        )));
    }
    Ok(())
}

/// Re-parse the script's frontmatter to pick up the ceiling. The full
/// frontmatter was already parsed earlier; this small re-parse keeps
/// the orchestrate signature shorter at the cost of one extra read.
fn parsed_ceiling(script: &Path) -> Result<Duration, DevError> {
    let stem = script
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("sync-script");
    let info = discover::parse_script(script, stem)
        .map_err(|e| DevError::Config(format!("sync-smoke: re-parse ceiling: {e}")))?;
    Ok(info.ceiling)
}

/// Write top-level `run.toml` with reproducibility metadata. Mock and
/// harness keep their own subdir state; this top-level file ties them
/// together for triage.
fn write_run_toml(
    harness_dir: &Path,
    mock_dir: &Path,
    script_abs: &Path,
    built: &HarnessBuild,
    dc: &output::DeadlineCapture,
    mock: &MockOutcome,
) -> Result<(), DevError> {
    let mut s = format!(
        "brokkr_version = \"{}\"\nscript = \"{}\"\nharness_binary = \"{}\"\nfeatures = \"{}\"\nharness_elapsed_ms = {}\n",
        env!("CARGO_PKG_VERSION"),
        script_abs.display(),
        built.binary.display(),
        built.features_label,
        dc.captured.elapsed.as_millis(),
    );
    if let Some(code) = dc.captured.status.code() {
        s.push_str(&format!("harness_exit_code = {code}\n"));
    }
    if dc.killed_on_deadline {
        s.push_str("harness_killed_on_deadline = true\n");
    }
    s.push_str("\n[mock]\n");
    if let Some(code) = mock.exit_code {
        s.push_str(&format!("exit_code = {code}\n"));
    }
    if let Some(sig) = mock.signal {
        s.push_str(&format!("signal = {sig}\n"));
    }
    if mock.killed_after_budget {
        s.push_str("killed_after_budget = true\n");
    }

    fs::write(
        harness_dir
            .parent()
            .unwrap_or(harness_dir)
            .join("run.toml"),
        s,
    )
    .map_err(DevError::Io)?;
    let _mock_dir_anchor = mock_dir; // future: copy mock data dir on failure
    Ok(())
}

// ---------------------------------------------------------------------------
// sync-bench
// ---------------------------------------------------------------------------

/// CLI inputs for `brokkr sync-bench`.
pub struct SyncBenchRequest<'a> {
    pub project_root: &'a Path,
    pub dev_config: &'a DevConfig,
    pub script: &'a str,
    /// Number of measured iterations. Best-of-N reported and stored.
    pub bench: usize,
    /// Allow recording on a dirty git tree (results land under the `dirty`
    /// alias instead of being skipped). Mirrors the existing bench-flag
    /// semantics across pbfhogg/elivagar.
    pub force: bool,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
    /// Literal `brokkr <...>` invocation, threaded through for the
    /// `brokkr_args` column in results.db.
    pub brokkr_args: String,
    /// Run the named gate after the bench completes. See
    /// `docs/commands/ratatoskr-gate.md`.
    pub gate: Option<&'a str>,
    /// Record this run as a baseline candidate for the named gate;
    /// suppress evaluation. Only meaningful when `gate` is set.
    pub as_baseline: bool,
}

