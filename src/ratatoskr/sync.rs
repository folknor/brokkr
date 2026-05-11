//! Plan-3 sync orchestration: `sync-list` and `sync-smoke`.
//!
//! `sync-list` is pure brokkr - walk the configured sync-script
//! directory, parse frontmatter, print a sorted table. No ratatoskr or
//! sæhrimnir runtime dependency.
//!
//! `sync-smoke` builds the harness binary per `[ratatoskr.harness]`,
//! spawns sæhrimnir against the script's declared fixture, parses the
//! per-protocol ports out of the readiness sentinel, then spawns
//! `<harness binary> --test-harness <SCRIPT>` with the
//! `RATATOSKR_TEST_*_ENDPOINT` env-var family injected (only those
//! whose names ratatoskr's `brokkr.toml` has spelled out). When the
//! harness exits, brokkr SIGTERMs sæhrimnir with the
//! [`saehrimnir::SHUTDOWN_BUDGET`] before escalating to SIGKILL.

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
use crate::ratatoskr::artefacts::{self, ArtefactDir};
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

/// Drive `brokkr sync-bench` end-to-end:
///
/// 1. Validate config (same as sync-smoke).
/// 2. Resolve script + fixture from frontmatter.
/// 3. Bootstrap [`crate::config::ResolvedPaths`] + acquire the bench
///    lockfile via [`BenchHarness::new`]; build the harness binary.
/// 4. Allocate one top-level run dir
///    `.brokkr/ratatoskr/sync/<test>/run-N/` with `mock/` plus per-iter
///    `iter-K/harness/` subdirs. The whole bench shares one sæhrimnir
///    process - sæhrimnir is deterministic per fixture, so reusing it
///    across iterations keeps the iteration timing measuring the
///    sync code, not mock startup.
/// 5. Spawn sæhrimnir, get endpoints.
/// 6. For each iteration: spawn the harness binary with `BROKKR_MARKER_FIFO`,
///    `BROKKR_HARNESS_ARTEFACT_DIR=iter-K/harness`, `BROKKR_TEST_BIN_DIR`,
///    and the configured `RATATOSKR_TEST_*_ENDPOINT` family. Sidecar
///    captures `/proc` samples + phase markers; brokkr reads
///    `iter-K/harness/summary.json` after each iteration.
/// 7. Best-of-N is selected on the script's `SYNC_START` -> `SYNC_END`
///    marker span (falls back to wall-clock elapsed if the script
///    doesn't emit those markers). The best iteration's summary.json
///    metrics are stored as `meta.<key>` rows alongside the result;
///    sidecar data for the best iteration is what `brokkr sidecar
///    <uuid>` surfaces.
/// 8. Tear down sæhrimnir with the standard 1.5s budget.
#[allow(clippy::too_many_lines)] // entry point: validate + bootstrap + build + spawn + dispatch
pub fn run_sync_bench(req: &SyncBenchRequest<'_>) -> Result<(), DevError> {
    let cfg = req.dev_config.ratatoskr.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync-bench: no [ratatoskr] section in brokkr.toml. \
             Required to locate sæhrimnir and the harness binary."
                .into(),
        )
    })?;
    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync-bench: no [ratatoskr.harness] section in brokkr.toml. \
             Declare it with `package = \"<crate>\"` (and optional \
             `binary`, `features`, `debug`)."
                .into(),
        )
    })?;
    let mock_binary = require_path(&cfg.mock_server_binary, req.project_root, "mock_server_binary")?;
    let fixtures_dir = require_path(&cfg.fixtures_dir, req.project_root, "fixtures_dir")?;
    if !mock_binary.exists() {
        return Err(DevError::Config(format!(
            "sync-bench: sæhrimnir binary not found at {}. Build it first.",
            mock_binary.display()
        )));
    }
    if req.bench == 0 {
        return Err(DevError::Config("sync-bench: --bench must be >= 1".into()));
    }

    // Validate `--gate <name>` references a configured gate before
    // building/running anything - errors here cost nothing.
    if let Some(name) = req.gate {
        let gate = cfg.gate.get(name).ok_or_else(|| {
            DevError::Config(format!(
                "sync-bench: gate `{name}` not found in [ratatoskr.gate.*] of brokkr.toml"
            ))
        })?;
        if gate.metrics.is_empty() && !req.as_baseline {
            return Err(DevError::Config(format!(
                "sync-bench: gate `{name}` has no [ratatoskr.gate.{name}.metrics.*] rules. \
                 An empty rule set silently passes - add at least one rule, or use \
                 --as-baseline if you only want to record."
            )));
        }
    }

    let script_abs = Path::new(req.script).canonicalize().map_err(|e| {
        DevError::Config(format!("sync-bench: canonicalize script: {e}"))
    })?;
    if !script_abs.is_file() {
        return Err(DevError::Config(format!(
            "sync-bench: script not found or not a file: {}",
            req.script
        )));
    }
    let test_id = script_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| DevError::Config(format!("sync-bench: script has no stem: {}", req.script)))?
        .to_owned();

    let parsed = discover::parse_script(&script_abs, &test_id).map_err(|e| {
        DevError::Config(format!("sync-bench: parse script: {e}"))
    })?;
    let fixture_name = parsed.fixture.as_ref().ok_or_else(|| {
        DevError::Config(format!(
            "sync-bench: script {test_id} has no `-- fixture: <NAME>` frontmatter line."
        ))
    })?;
    let fixture_path = resolve_fixture(&fixtures_dir, fixture_name)?;

    // Bootstrap paths and stand up the bench harness. The harness
    // acquires the lockfile internally, so concurrent brokkr
    // invocations block here.
    let pi = context::bootstrap(None)?;
    let paths = context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;
    let harness = BenchHarness::new(
        &paths,
        req.project_root,
        None,
        Project::Ratatoskr,
        "sync-bench",
        req.force,
        false,
        None,
    )?
    .with_brokkr_args(req.brokkr_args.clone())
    .with_measure_mode(Some("bench"));

    let debug = req.profile_override.unwrap_or_else(|| harness_cfg.debug.unwrap_or(false));
    let built = build::build_for_harness(
        req.project_root,
        harness_cfg,
        debug,
        Some(&|pid| harness.lock().set_child_pid(pid)),
        Some(&|| harness.lock().clear_child_pid()),
        // isolate_pg=false: sync-bench has no outer SigtermGuard
        // (BenchHarness's sidecar installs its own per-iteration, but
        // it doesn't cover the build phase). PG-isolating cargo here
        // would orphan it on terminal Ctrl-C; --hard accepts the
        // single-PID kill (cargo reaps its own children on its way
        // down via SIGCHLD, but rustc workers may briefly orphan -
        // tracked as a known limit in TODO.md).
        false,
    )?;
    output::ratatoskr_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display(),
    ));

    let artefact_parent = req.project_root.join(SYNC_ARTEFACT_PARENT);
    let artefacts = ArtefactDir::allocate(&artefact_parent, &test_id, req.keep_artefacts)?;
    let mock_dir = artefacts.path().join("mock");
    fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;

    output::ratatoskr_msg(&format!(
        "running {test_id} (fixture: {fixture_name}, bench={})",
        req.bench
    ));

    let outcome = bench_loop(
        req,
        cfg,
        &built,
        &mock_binary,
        &fixture_path,
        &script_abs,
        artefacts.path(),
        &mock_dir,
        &paths.scratch_dir,
        &harness,
        fixture_name,
        debug,
    );

    match outcome {
        Ok(()) => {
            artefacts.finalize_success()?;
            Ok(())
        }
        Err(e) => {
            let path = artefacts.path().to_path_buf();
            artefacts.finalize_failure();
            output::ratatoskr_msg(&format!(
                "FAIL: {e} (artefacts preserved at {})",
                path.display()
            ));
            artefacts::emit_clean_hint();
            Err(e)
        }
    }
}

/// One iteration's measured outcome. `marker_span_ms` is the
/// `SYNC_START` -> `SYNC_END` span when those markers fired; otherwise
/// `None` and `wall_clock_ms` is used for best-of-N selection.
struct IterOutcome {
    run_idx: usize,
    marker_span_ms: Option<i64>,
    wall_clock_ms: i64,
    summary: serde_json::Map<String, serde_json::Value>,
}

impl IterOutcome {
    fn elapsed_ms(&self) -> i64 {
        self.marker_span_ms.unwrap_or(self.wall_clock_ms)
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)] // bench loop: per-iter spawn + sidecar + summary ingest + post-loop record
fn bench_loop(
    req: &SyncBenchRequest<'_>,
    cfg: &RatatoskrConfig,
    built: &HarnessBuild,
    mock_binary: &Path,
    fixture_path: &Path,
    script_abs: &Path,
    run_root: &Path,
    mock_dir: &Path,
    scratch_dir: &Path,
    harness: &BenchHarness,
    fixture_name: &str,
    debug: bool,
) -> Result<(), DevError> {
    // PID published from inside spawn_observed - before readiness wait
    // - so a `brokkr kill --hard` arriving during startup finds it.
    // No outer `SigtermGuard` here because BenchHarness's sidecar
    // installs its own around the measured window - a nested install
    // would clobber the outer's `Drop` and restore SIG_DFL early.
    // Cooperative SIGTERM during build / between iterations therefore
    // still falls through to the default terminate action; see TODO.md
    // if we ever extend coverage.
    let mock = MockServer::spawn_observed(
        mock_binary,
        fixture_path,
        mock_dir,
        Some(&|pid| harness.lock().add_mock_pid(pid)),
        Some(&|pid| harness.lock().remove_mock_pid(pid)),
        // isolate_pg=false: same reason as the build call above. The
        // mock spawns BEFORE the bench loop where sidecar's
        // SigtermGuard takes over.
        false,
    )?;
    output::ratatoskr_msg(&format!("mock ready in {}", format_secs(mock.ready_elapsed())));
    let endpoint_envs = endpoint_env_pairs(cfg, mock.endpoints());

    let mut fifo = sidecar::SidecarFifo::create(scratch_dir)?;
    let fifo_path = fifo.path_str()?.to_owned();
    let bin_dir_str = built.bin_dir.display().to_string();
    let binary_str = built.binary.display().to_string();
    let script_str = script_abs.display().to_string();

    let mut sidecar_runs: Vec<sidecar::SidecarData> = Vec::with_capacity(req.bench);
    let mut best: Option<IterOutcome> = None;

    let bench_outcome = (|| -> Result<(), DevError> {
        for i in 0..req.bench {
            output::bench_msg(&format!("run {}/{}", i + 1, req.bench));
            harness.lock().set_progress(
                u32::try_from(i + 1).unwrap_or(u32::MAX),
                u32::try_from(req.bench).unwrap_or(u32::MAX),
            );
            if i > 0 {
                fifo.reopen()?;
            }

            let iter_dir = run_root.join(format!("iter-{}", i + 1));
            let iter_harness_dir = iter_dir.join("harness");
            fs::create_dir_all(&iter_harness_dir).map_err(DevError::Io)?;
            let iter_harness_str = iter_harness_dir.display().to_string();

            let mut env_pairs: Vec<(&str, &str)> = vec![
                ("BROKKR_MARKER_FIFO", fifo_path.as_str()),
                ("BROKKR_HARNESS_ARTEFACT_DIR", &iter_harness_str),
                ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
            ];
            for (name, value) in &endpoint_envs {
                env_pairs.push((name.as_str(), value.as_str()));
            }

            let start = Instant::now();
            let child = output::spawn_captured(
                &binary_str,
                &["--test-harness", &script_str],
                req.project_root,
                &env_pairs,
                // isolate_pg=true: sidecar installs its own
                // SigtermGuard around `run_sidecar` below, covering
                // the lifetime of this child.
                true,
            )?;
            let pid = child.id();
            harness.lock().set_child_pid(pid);

            let result = sidecar::run_sidecar(child, &mut fifo, i, start, None);
            // Iteration's child has reaped; clear so a stale PID can't
            // be SIGKILLed by `--hard` in the gap before the next iter.
            harness.lock().clear_child_pid();

            // Persist each iter's stdout/stderr so a later FAIL can
            // be reproduced without re-running. summary.json is
            // already in iter_harness_dir from the harness binary.
            fs::write(iter_dir.join("binary-stdout.log"), &result.stdout)
                .map_err(DevError::Io)?;
            fs::write(iter_dir.join("binary-stderr.log"), &result.stderr)
                .map_err(DevError::Io)?;

            if result.stopped_by_signal {
                sidecar_runs.push(result.data);
                return Err(DevError::Interrupted);
            }
            if !result.exit_status.success() {
                let stderr_tail = String::from_utf8_lossy(&result.stderr)
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                sidecar_runs.push(result.data);
                return Err(DevError::Config(format!(
                    "harness binary exited with {:?} on iter {}/{}\n--- last 5 stderr lines ---\n{stderr_tail}",
                    result.exit_status,
                    i + 1,
                    req.bench
                )));
            }

            let summary = read_summary_json(&iter_harness_dir)?;
            let marker_span_ms = sync_span_from_markers(&result.data.markers);
            let wall_clock_ms = i64::try_from(result.elapsed.as_millis()).unwrap_or(i64::MAX);
            let outcome = IterOutcome {
                run_idx: i,
                marker_span_ms,
                wall_clock_ms,
                summary,
            };

            output::bench_msg(&format!(
                "  iter {} -> {}ms{}",
                i + 1,
                outcome.elapsed_ms(),
                if marker_span_ms.is_none() {
                    " (wall-clock; no SYNC_START/SYNC_END markers)"
                } else {
                    ""
                },
            ));

            if best.as_ref().is_none_or(|b| outcome.elapsed_ms() < b.elapsed_ms()) {
                best = Some(outcome);
            }
            sidecar_runs.push(result.data);
        }
        Ok(())
    })();

    drop(fifo);
    let mock_outcome = mock.shutdown();
    harness.lock().clear_mock_pids();
    output::ratatoskr_msg(&format!(
        "mock shutdown in {}",
        format_secs(mock_outcome.shutdown_elapsed)
    ));

    bench_outcome?;

    let best = best.ok_or_else(|| DevError::Config("sync-bench: no successful iterations".into()))?;
    let elapsed_ms = best.elapsed_ms();

    let mut kv = summary_to_kv(&best.summary);
    if best.marker_span_ms.is_none() {
        kv.push(KvPair::text(
            "meta.timing_source",
            "wall_clock_no_markers",
        ));
    } else {
        kv.push(KvPair::text("meta.timing_source", "sync_markers"));
    }
    if mock_outcome.killed_after_budget {
        kv.push(KvPair::text("meta.mock_killed_after_budget", "true"));
    }

    let bench_result = BenchResult {
        elapsed_ms,
        kv,
        distribution: None,
        hotpath: None,
    };

    let bench_config = BenchConfig {
        command: "sync-bench".into(),
        mode: None,
        input_file: Some(fixture_name.to_owned()),
        input_mb: None,
        cargo_features: None,
        cargo_profile: CargoProfile::Release,
        runs: req.bench,
        cli_args: Some(format!("--test-harness {}", script_abs.display())),
        brokkr_args: None,
        metadata: Vec::new(),
    };

    let uuid = harness.record_result(&bench_config, &bench_result)?;
    // run_info=None: brokkr's sidecar provenance can carry pid / binary
    // hash / git commit, but the helper that builds it is private to
    // BenchHarness today. Sync-bench works without it; revisit if a
    // diagnostic plugin needs the metadata.
    harness.store_sidecar(uuid.as_deref(), &sidecar_runs, best.run_idx, None)?;

    output::ratatoskr_msg(&format!(
        "best-of-{}: {}ms (iter {})",
        req.bench,
        elapsed_ms,
        best.run_idx + 1,
    ));

    if let Some(gate_name) = req.gate {
        run_gate_hook(&GateHookCtx {
            req,
            cfg,
            gate_name,
            project_root: req.project_root,
            script_abs,
            fixture_name,
            debug,
            elapsed_ms,
            summary: &best.summary,
            sidecar_data: &sidecar_runs[best.run_idx],
        })?;
    }

    Ok(())
}

/// Inputs for the post-bench gate hook.
struct GateHookCtx<'a> {
    req: &'a SyncBenchRequest<'a>,
    cfg: &'a RatatoskrConfig,
    gate_name: &'a str,
    project_root: &'a Path,
    script_abs: &'a Path,
    fixture_name: &'a str,
    debug: bool,
    elapsed_ms: i64,
    summary: &'a serde_json::Map<String, serde_json::Value>,
    sidecar_data: &'a sidecar::SidecarData,
}

/// Record the gated run into `gate.db`, then either: print the
/// `--as-baseline` paste-line and return, or look up the configured
/// per-hostname baseline and evaluate every metric rule. Returns
/// `DevError::Config` on any rule failure so sync-bench exits non-zero.
fn run_gate_hook(ctx: &GateHookCtx<'_>) -> Result<(), DevError> {
    let gate = ctx
        .cfg
        .gate
        .get(ctx.gate_name)
        .expect("gate existence already validated upstream");

    // Validate the gate's configured script matches the current
    // invocation. The gate is bound to a specific script; mixing them
    // up would silently compare against a different fixture's baseline.
    let configured_script = if gate.script.is_absolute() {
        gate.script.clone()
    } else {
        ctx.project_root.join(&gate.script)
    };
    let configured_canon = configured_script
        .canonicalize()
        .unwrap_or(configured_script.clone());
    if configured_canon != ctx.script_abs {
        return Err(DevError::Config(format!(
            "gate `{}`: configured script `{}` does not match invocation script `{}`",
            ctx.gate_name,
            gate.script.display(),
            ctx.script_abs.display(),
        )));
    }

    let hostname = crate::config::hostname()?;
    let git_info = git::collect(ctx.project_root)?;
    let profile_name = if ctx.debug { "debug" } else { "release" };

    let sidecar_blob = sidecar_to_json(ctx.sidecar_data);
    let meta_blob = meta_to_json(ctx.summary);

    let row = GateRow {
        git_commit: git_info.commit.clone(),
        dirty: !git_info.is_clean,
        hostname: hostname.clone(),
        gate_name: ctx.gate_name.to_owned(),
        script: ctx.script_abs.display().to_string(),
        fixture: ctx.fixture_name.to_owned(),
        profile: profile_name.into(),
        elapsed_ms: ctx.elapsed_ms,
        exit_code: 0,
        success: true,
        sidecar: serde_json::to_string(&sidecar_blob).unwrap_or_else(|_| "{}".into()),
        meta: serde_json::to_string(&meta_blob).unwrap_or_else(|_| "{}".into()),
    };

    let db_path = ctx.project_root.join(".brokkr/ratatoskr/gate.db");
    let db = GateDb::open(&db_path)?;
    let uuid = db.insert(&row)?;
    let short = &uuid[..8.min(uuid.len())];

    output::ratatoskr_msg(&format!(
        "gate `{}` recorded run {short} on host `{hostname}`",
        ctx.gate_name
    ));

    if ctx.req.as_baseline {
        output::ratatoskr_msg("--as-baseline: skipping evaluation. Add this line to brokkr.toml:");
        output::ratatoskr_msg(&format!(
            "    [ratatoskr.gate.{}.baseline]\n    {hostname} = \"{uuid}\"",
            ctx.gate_name
        ));
        return Ok(());
    }

    evaluate_against_baseline(&db, &hostname, ctx.gate_name, gate, &row)
}

/// Look up the pinned per-hostname baseline UUID in `brokkr.toml`,
/// fetch the row from `gate.db`, validate gate/script/fixture identity,
/// run rule evaluation, and emit a report. Returns
/// `DevError::Config("gate failed: ...")` on any rule failure.
fn evaluate_against_baseline(
    db: &GateDb,
    hostname: &str,
    gate_name: &str,
    gate: &GateConfig,
    current_row: &GateRow,
) -> Result<(), DevError> {
    let baseline_uuid = gate.baseline.get(hostname).ok_or_else(|| {
        DevError::Config(format!(
            "gate `{gate_name}`: no baseline pinned for host `{hostname}` in \
             [ratatoskr.gate.{gate_name}.baseline]. Record one with --as-baseline \
             and add the printed line to brokkr.toml."
        ))
    })?;
    let baseline_entry = db.lookup_baseline(baseline_uuid, hostname)?.ok_or_else(|| {
        DevError::Config(format!(
            "gate `{gate_name}`: baseline UUID `{baseline_uuid}` not found in gate.db \
             on host `{hostname}` - the pinned UUID was recorded on a different machine. \
             Record locally with --as-baseline and update brokkr.toml."
        ))
    })?;
    if baseline_entry.gate_name != gate_name {
        return Err(DevError::Config(format!(
            "gate `{gate_name}`: baseline row's gate is `{}` (mismatch)",
            baseline_entry.gate_name
        )));
    }
    if baseline_entry.script != current_row.script {
        return Err(DevError::Config(format!(
            "gate `{gate_name}`: baseline script `{}` != current `{}`",
            baseline_entry.script, current_row.script
        )));
    }
    if baseline_entry.fixture != current_row.fixture {
        return Err(DevError::Config(format!(
            "gate `{gate_name}`: baseline fixture `{}` != current `{}`",
            baseline_entry.fixture, current_row.fixture
        )));
    }

    let current_run = gate_eval::GateRun::from_parts(
        current_row.elapsed_ms,
        current_row.exit_code,
        current_row.success,
        serde_json::from_str(&current_row.sidecar).unwrap_or_default(),
        serde_json::from_str(&current_row.meta).unwrap_or_default(),
    );
    let baseline_run = gate_eval::GateRun::from_entry(&baseline_entry)?;

    let outcomes = gate_eval::evaluate(gate, &current_run, &baseline_run)?;
    let (report, any_failed) = gate_eval::format_report(&outcomes);
    let label_suffix = gate
        .baseline_label
        .as_deref()
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
    output::ratatoskr_msg(&format!(
        "gate `{gate_name}` vs baseline {}{label_suffix} ({} rules):",
        &baseline_entry.uuid[..8.min(baseline_entry.uuid.len())],
        outcomes.len(),
    ));
    if baseline_entry.dirty {
        output::ratatoskr_msg(
            "  [warn] baseline was recorded on a dirty git tree (--force); \
             consider re-recording on a clean checkout",
        );
    }
    if !report.is_empty() {
        for line in report.lines() {
            output::ratatoskr_msg(line);
        }
    }
    if any_failed {
        return Err(DevError::Config(format!(
            "gate `{gate_name}` FAILED: one or more rules breached"
        )));
    }
    output::ratatoskr_msg(&format!("gate `{gate_name}` PASSED"));
    Ok(())
}

/// Project a `SidecarData` into a flat JSON object usable by gate
/// `sidecar.<key>` selectors. Surfaces the summary fields plus a few
/// derived peaks/totals from the sample stream so common rules
/// (`sidecar.rss_peak_kb`, `sidecar.read_bytes`) work without the
/// gate doing its own aggregation.
fn sidecar_to_json(data: &sidecar::SidecarData) -> serde_json::Value {
    let mut peak_rss_kb: i64 = data.summary.vm_hwm_kb;
    let mut max_threads: i64 = 0;
    let mut last = data.samples.last();
    for s in &data.samples {
        peak_rss_kb = peak_rss_kb.max(s.rss_kb);
        max_threads = max_threads.max(s.num_threads);
        last = Some(s);
    }
    let (read_bytes, write_bytes, vol_cs, nonvol_cs, minflt, majflt) = match last {
        Some(s) => (s.read_bytes, s.write_bytes, s.vol_cs, s.nonvol_cs, s.minflt, s.majflt),
        None => (0, 0, 0, 0, 0, 0),
    };
    serde_json::json!({
        "vm_hwm_kb": data.summary.vm_hwm_kb,
        "rss_peak_kb": peak_rss_kb,
        "sample_count": data.summary.sample_count,
        "marker_count": data.summary.marker_count,
        "wall_time_ms": data.summary.wall_time_ms,
        "num_threads_max": max_threads,
        "read_bytes": read_bytes,
        "write_bytes": write_bytes,
        "vol_cs": vol_cs,
        "nonvol_cs": nonvol_cs,
        "minflt": minflt,
        "majflt": majflt,
    })
}

/// Project the harness-side `summary.json` map into the meta blob
/// stored in gate.db. Same scalar-only rule as `summary_to_kv`: bools
/// pass through (so `correct = true` stays evaluable), nested objects
/// and arrays are dropped.
fn meta_to_json(map: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (k, v) in map {
        match v {
            serde_json::Value::Number(_)
            | serde_json::Value::String(_)
            | serde_json::Value::Bool(_) => {
                out.insert(k.clone(), v.clone());
            }
            _ => {}
        }
    }
    serde_json::Value::Object(out)
}

/// Read `summary.json` from the harness's per-iter artefact dir. Missing
/// file is fine (the script may not have written one) - we surface an
/// empty map rather than failing, so a script that doesn't bother with
/// metrics still benches cleanly.
fn read_summary_json(harness_dir: &Path) -> Result<serde_json::Map<String, serde_json::Value>, DevError> {
    let path = harness_dir.join("summary.json");
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let text = fs::read_to_string(&path).map_err(DevError::Io)?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        DevError::Config(format!(
            "sync-bench: parse {}: {e}",
            path.display()
        ))
    })?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        other => Err(DevError::Config(format!(
            "sync-bench: {} root must be a JSON object, got {}",
            path.display(),
            value_kind(&other)
        ))),
    }
}

fn value_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Convert summary.json's flat top-level fields into KvPairs. Numeric
/// values become `Int` when the value is integer-valued (i64 fits) and
/// `Real` otherwise; strings become `Text`. Bools/null/array/nested
/// objects are skipped silently - first time a script needs nested
/// shape, lift it explicitly.
fn summary_to_kv(map: &serde_json::Map<String, serde_json::Value>) -> Vec<KvPair> {
    let mut out = Vec::with_capacity(map.len());
    for (key, value) in map {
        let prefixed = format!("meta.{key}");
        match value {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    out.push(KvPair {
                        key: prefixed,
                        value: KvValue::Int(i),
                    });
                } else if let Some(f) = n.as_f64() {
                    out.push(KvPair {
                        key: prefixed,
                        value: KvValue::Real(f),
                    });
                }
            }
            serde_json::Value::String(s) => {
                out.push(KvPair {
                    key: prefixed,
                    value: KvValue::Text(s.clone()),
                });
            }
            _ => {}
        }
    }
    out
}

/// Compute the millisecond span between the script-emitted `SYNC_START`
/// and `SYNC_END` markers, if both fired. Returns `None` when either
/// is missing - caller falls back to wall-clock. Multiple `SYNC_START`s
/// are tolerated (last one wins) so a script that warms up under the
/// same marker name doesn't break; first `SYNC_END` after the last
/// `SYNC_START` wins.
fn sync_span_from_markers(markers: &[sidecar::Marker]) -> Option<i64> {
    let last_start = markers.iter().rfind(|m| m.name == "SYNC_START")?;
    let end = markers
        .iter()
        .find(|m| m.name == "SYNC_END" && m.timestamp_us > last_start.timestamp_us)?;
    let span_us = end.timestamp_us.checked_sub(last_start.timestamp_us)?;
    Some(span_us / 1000)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::HarnessConfig;
    use crate::ratatoskr::saehrimnir::Endpoints;
    use std::path::Path;

    fn cfg_with_endpoints() -> RatatoskrConfig {
        RatatoskrConfig {
            harness: Some(HarnessConfig {
                package: "app".into(),
                binary: None,
                features: Vec::new(),
                debug: None,
            }),
            mock_server_binary: None,
            fixtures_dir: None,
            test_endpoint_env_jmap: Some("RATATOSKR_TEST_JMAP_ENDPOINT".into()),
            test_endpoint_env_imap: Some("RATATOSKR_TEST_IMAP_ENDPOINT".into()),
            test_endpoint_env_smtp: None,
            test_endpoint_env_graph: None,
            test_endpoint_env_gmail: Some("RATATOSKR_TEST_GMAIL_ENDPOINT".into()),
            test_endpoint_env_caldav: Some("RATATOSKR_TEST_CALDAV_ENDPOINT".into()),
            test_endpoint_env_people: Some("RATATOSKR_TEST_PEOPLE_ENDPOINT".into()),
            test_endpoint_env_gcal: Some("RATATOSKR_TEST_GCAL_ENDPOINT".into()),
            sync_script_dir: None,
            gate: std::collections::BTreeMap::new(),
        }
    }

    fn endpoints() -> Endpoints {
        Endpoints {
            jmap: 1001,
            imap: 1002,
            smtp: 1003,
            graph: 1004,
            gmail: 1005,
            caldav: 1006,
            people: 1007,
            gcal: 1008,
        }
    }

    #[test]
    fn endpoint_envs_only_emit_for_configured_protocols() {
        let cfg = cfg_with_endpoints();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        let names: Vec<&str> = pairs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "RATATOSKR_TEST_JMAP_ENDPOINT",
                "RATATOSKR_TEST_IMAP_ENDPOINT",
                "RATATOSKR_TEST_GMAIL_ENDPOINT",
                "RATATOSKR_TEST_CALDAV_ENDPOINT",
                "RATATOSKR_TEST_PEOPLE_ENDPOINT",
                "RATATOSKR_TEST_GCAL_ENDPOINT",
            ]
        );
    }

    #[test]
    fn endpoint_envs_use_http_for_json_over_http() {
        let cfg = cfg_with_endpoints();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        let jmap = pairs
            .iter()
            .find(|(n, _)| n == "RATATOSKR_TEST_JMAP_ENDPOINT")
            .unwrap();
        assert_eq!(jmap.1, "http://127.0.0.1:1001");
        let gmail = pairs
            .iter()
            .find(|(n, _)| n == "RATATOSKR_TEST_GMAIL_ENDPOINT")
            .unwrap();
        assert_eq!(gmail.1, "http://127.0.0.1:1005");
    }

    #[test]
    fn endpoint_envs_use_host_port_for_stream_protocols() {
        let cfg = cfg_with_endpoints();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        let imap = pairs
            .iter()
            .find(|(n, _)| n == "RATATOSKR_TEST_IMAP_ENDPOINT")
            .unwrap();
        assert_eq!(imap.1, "127.0.0.1:1002");
    }

    #[test]
    fn endpoint_envs_empty_when_nothing_configured() {
        let cfg = RatatoskrConfig::default();
        let pairs = endpoint_env_pairs(&cfg, &endpoints());
        assert!(pairs.is_empty());
    }

    #[test]
    fn sync_script_dir_falls_back_to_default() {
        let root = Path::new("/proj");
        let resolved = sync_script_dir(root, None);
        assert_eq!(resolved, root.join(DEFAULT_SYNC_SCRIPT_DIR));
    }

    #[test]
    fn sync_script_dir_respects_relative_override() {
        let root = Path::new("/proj");
        let cfg = RatatoskrConfig {
            sync_script_dir: Some(PathBuf::from("custom/sync")),
            ..RatatoskrConfig::default()
        };
        let resolved = sync_script_dir(root, Some(&cfg));
        assert_eq!(resolved, root.join("custom/sync"));
    }

    fn marker(name: &str, ts_us: i64) -> sidecar::Marker {
        sidecar::Marker {
            marker_idx: 0,
            timestamp_us: ts_us,
            name: name.to_owned(),
        }
    }

    #[test]
    fn sync_span_uses_last_start_and_first_following_end() {
        // Warmup-then-measure: SYNC_START fires twice; the LAST one is
        // the measured start. SYNC_END after that is the measured end.
        let markers = vec![
            marker("SYNC_START", 1_000_000),
            marker("warmup_done", 2_000_000),
            marker("SYNC_START", 5_000_000),
            marker("SYNC_END", 12_500_000),
        ];
        assert_eq!(sync_span_from_markers(&markers), Some(7500));
    }

    #[test]
    fn sync_span_returns_none_without_markers() {
        let markers: Vec<sidecar::Marker> = Vec::new();
        assert_eq!(sync_span_from_markers(&markers), None);
    }

    #[test]
    fn sync_span_returns_none_with_only_start() {
        let markers = vec![marker("SYNC_START", 1_000_000)];
        assert_eq!(sync_span_from_markers(&markers), None);
    }

    #[test]
    fn sync_span_returns_none_with_end_before_start() {
        // Pathological: SYNC_END before any SYNC_START. Treat as
        // missing rather than negative.
        let markers = vec![
            marker("SYNC_END", 500_000),
            marker("SYNC_START", 1_000_000),
        ];
        assert_eq!(sync_span_from_markers(&markers), None);
    }

    #[test]
    fn summary_to_kv_emits_int_real_text() {
        let mut map = serde_json::Map::new();
        map.insert("count".into(), serde_json::json!(42));
        map.insert("ratio".into(), serde_json::json!(2.5));
        map.insert("provider".into(), serde_json::json!("jmap"));
        map.insert("nested".into(), serde_json::json!({"skipped": true}));
        map.insert("flag".into(), serde_json::json!(true));
        map.insert("nullable".into(), serde_json::Value::Null);
        let kv = summary_to_kv(&map);
        // Only int + real + string survive; nested/bool/null skipped.
        assert_eq!(kv.len(), 3);
        let by_key = |k: &str| kv.iter().find(|p| p.key == k).map(|p| p.value.clone());
        assert!(matches!(by_key("meta.count"), Some(KvValue::Int(42))));
        match by_key("meta.ratio") {
            Some(KvValue::Real(v)) => assert!((v - 2.5).abs() < 1e-9),
            _ => panic!("expected meta.ratio=Real"),
        }
        match by_key("meta.provider") {
            Some(KvValue::Text(s)) => assert_eq!(s, "jmap"),
            _ => panic!("expected meta.provider=Text(jmap)"),
        }
    }

    fn sum_tmp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/sync-summary")
            .join(name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_summary_json_missing_returns_empty_map() {
        let dir = sum_tmp("missing");
        let map = read_summary_json(&dir).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn read_summary_json_parses_object() {
        let dir = sum_tmp("ok");
        fs::write(
            dir.join("summary.json"),
            r#"{"messages_synced":100,"final_db_size_bytes":4096}"#,
        )
        .unwrap();
        let map = read_summary_json(&dir).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("messages_synced").and_then(serde_json::Value::as_i64),
            Some(100)
        );
    }

    #[test]
    fn read_summary_json_rejects_non_object_root() {
        let dir = sum_tmp("array");
        fs::write(dir.join("summary.json"), "[1,2,3]").unwrap();
        let err = read_summary_json(&dir).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"), "got: {err}");
    }

    #[test]
    fn read_summary_json_surfaces_parse_error() {
        let dir = sum_tmp("malformed");
        fs::write(dir.join("summary.json"), "{not valid json").unwrap();
        let err = read_summary_json(&dir).unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    #[test]
    fn sync_script_dir_keeps_absolute_override() {
        let cfg = RatatoskrConfig {
            sync_script_dir: Some(PathBuf::from("/srv/scripts")),
            ..RatatoskrConfig::default()
        };
        let resolved = sync_script_dir(Path::new("/proj"), Some(&cfg));
        assert_eq!(resolved, PathBuf::from("/srv/scripts"));
    }
}
