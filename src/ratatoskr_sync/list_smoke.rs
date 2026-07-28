// Plan-3 sync orchestration: the index and run shapes of `brokkr sync`
// (bare and `<SCRIPT>`). The measured shape lives in `bench_gate.rs`.
//
// The index shape is pure brokkr - walk the configured sync-script
// directory, parse frontmatter, print a sorted table. No ratatoskr or
// sæhrimnir runtime dependency.
//
// The run shape builds the harness binary per `[ratatoskr.harness]`,
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
use crate::config::{DevConfig, GateConfig, HarnessConfig, RatatoskrConfig};
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
// sync (no SCRIPT) - the index
// ---------------------------------------------------------------------------

/// Bare `brokkr sync` - discover sync-test scripts under the configured
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
// sync --all - run the whole discovered cohort
// ---------------------------------------------------------------------------

/// `brokkr sync --all [--filter SUB]` - run every discovered sync script
/// unmeasured, in discovery order.
///
/// The sync-side answer to `service --all`, and the reason a directory
/// argument can now mean the same thing in both families. Scripts marked
/// `expected = ignored` in frontmatter are skipped unless
/// `include_ignored` is set - they reproduce known-broken behaviour and
/// would block an otherwise clean sweep.
///
/// Default is keep-going: a cohort exists to tell you what is broken, so
/// it reports every failure and exits non-zero if any script failed.
/// That is the opposite of `service --all`'s stop-on-first-failure
/// default, and deliberately so - each sync script owns its own artefact
/// dir, so a later failure never overwrites an earlier one's triage
/// material.
///
/// The harness is built once, before the loop - the cohort varies the
/// script, never the binary - and each script then reports exactly one
/// line. The alternative (loop over [`run_sync_smoke`]) meant one no-op
/// cargo invocation and four lines of repeated build boilerplate per
/// script; a 160-script sweep is read by scanning verdicts, so nothing
/// invariant belongs in the loop.
pub fn run_sync_all(req: &SyncAllRequest<'_>) -> Result<(), DevError> {
    let dir = sync_script_dir(req.project_root, req.dev_config.ratatoskr.as_ref());
    let scripts = discover::discover_at(&dir)?;

    if scripts.is_empty() {
        return Err(DevError::Config(format!(
            "sync --all: no sync-test scripts found under {}",
            dir.display()
        )));
    }

    let selected: Vec<&ScriptInfo> = scripts
        .iter()
        .filter(|s| {
            req.filter
                .is_none_or(|f| s.name.contains(f))
        })
        .filter(|s| req.include_ignored || s.expected.as_str() != "ignored")
        .collect();

    if selected.is_empty() {
        return Err(DevError::Config(format!(
            "sync --all: no scripts matched (filter: {}, {} discovered)",
            req.filter.unwrap_or("none"),
            scripts.len()
        )));
    }

    let (cfg, harness_cfg, mock_binary, fixtures_dir) =
        validate_sync_config(req.project_root, req.dev_config)?;

    output::ratatoskr_msg(&format!("sync cohort: {} script(s)", selected.len()));

    // Hold the global lock for the whole sweep, so another brokkr can't
    // interleave a build or bench between two scripts and the sweep can't
    // stall mid-way waiting behind one. With the build hoisted out of the
    // loop this is the path's only acquire; the re-entrant nesting story
    // lives with `--gate all` (see `lockfile::acquire`).
    let project_root_str = req.project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "sync",
        project_root: &project_root_str,
    })?;
    // One cooperative-SIGTERM guard for the whole sweep: the build and
    // every script run poll the same shutdown flag. See run_sync_smoke
    // for the mechanism.
    let _sigterm = crate::shutdown::SigtermGuard::install();

    let debug = req
        .profile_override
        .unwrap_or_else(|| harness_cfg.debug.unwrap_or(false));
    let built = build::build_for_harness(
        req.project_root,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true, // isolate_pg: SigtermGuard above bridges terminal signals
    )?;

    let runner = SyncRunner {
        project_root: req.project_root,
        cfg,
        built: &built,
        mock_binary: &mock_binary,
        keep_artefacts: req.keep_artefacts,
    };

    let mut failures: Vec<&str> = Vec::new();
    let mut any_preserved = false;
    for script in &selected {
        let outcome = match resolve_info(script, &fixtures_dir) {
            Ok(resolved) => runner.run_resolved(&resolved, &_lock),
            Err(e) => {
                output::ratatoskr_msg(&format!("{}: FAIL - {e}", script.name));
                failures.push(script.name.as_str());
                continue;
            }
        };
        match outcome.result {
            Ok(()) => {
                output::ratatoskr_msg(&format!("{}: PASS{}", script.name, outcome.summary));
            }
            Err(e) => {
                output::ratatoskr_msg(&format!("{}: FAIL{} - {e}", script.name, outcome.summary));
                if let Some(path) = outcome.preserved {
                    output::ratatoskr_msg(&format!("  artefacts preserved at {}", path.display()));
                    any_preserved = true;
                }
                failures.push(script.name.as_str());
            }
        }
    }

    if any_preserved {
        artefacts::emit_clean_hint();
    }
    let passed = selected.len() - failures.len();
    output::ratatoskr_msg(&format!("sync cohort: {passed}/{} passed", selected.len()));
    if failures.is_empty() {
        return Ok(());
    }
    // Each failure already printed its full error inline; the exit error
    // names them without repeating the messages.
    Err(DevError::Config(format!(
        "sync --all: {} of {} script(s) failed: {}",
        failures.len(),
        selected.len(),
        failures.join(", "),
    )))
}

/// CLI inputs for `brokkr sync --all`.
pub struct SyncAllRequest<'a> {
    pub project_root: &'a Path,
    pub dev_config: &'a DevConfig,
    /// Substring match against the script's discovered name.
    pub filter: Option<&'a str>,
    /// Include scripts whose frontmatter says `expected: ignored`.
    pub include_ignored: bool,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
}

// ---------------------------------------------------------------------------
// sync <SCRIPT> - run one
// ---------------------------------------------------------------------------

/// CLI inputs for `brokkr sync <SCRIPT>`. Pulled out so the orchestration
/// body can be smoke-tested with synthetic paths if needed.
pub struct SyncSmokeRequest<'a> {
    pub project_root: &'a Path,
    pub dev_config: &'a DevConfig,
    pub script: &'a str,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
}

/// Drive the unmeasured `brokkr sync <SCRIPT>` end-to-end:
///
/// 1. Validate config: `[ratatoskr.harness]`, `mock_server_binary`, and
///    `fixtures_dir` are all required. Endpoint env-var names are
///    optional - protocols without a configured spelling just don't
///    get an env var.
/// 2. Parse the script's frontmatter; require a `fixture: <NAME>` and
///    resolve it - both before anything is built, so a bad script fails
///    fast.
/// 3. Acquire the global lockfile.
/// 4. Build the harness binary per `[ratatoskr.harness]`.
/// 5. Hand off to [`SyncRunner::run_resolved`]: allocate
///    `.brokkr/ratatoskr/sync/<test>/run-N/`, spawn sæhrimnir and the
///    harness binary, tear down, finalize the artefact dir.
/// 6. PASS/FAIL on the harness binary's exit code; sæhrimnir's outcome
///    is logged but not gating (a script may legitimately tear it down
///    early in scenarios).
pub fn run_sync_smoke(req: &SyncSmokeRequest<'_>) -> Result<(), DevError> {
    let (cfg, harness_cfg, mock_binary, fixtures_dir) =
        validate_sync_config(req.project_root, req.dev_config)?;
    let resolved = resolve_single(req.script, &fixtures_dir)?;

    let project_root_str = req.project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "sync",
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

    let runner = SyncRunner {
        project_root: req.project_root,
        cfg,
        built: &built,
        mock_binary: &mock_binary,
        keep_artefacts: req.keep_artefacts,
    };

    output::ratatoskr_msg(&format!(
        "running {} (fixture: {})",
        resolved.test_id, resolved.fixture_name
    ));

    let outcome = runner.run_resolved(&resolved, &_lock);
    match outcome.result {
        Ok(()) => {
            output::ratatoskr_msg(&format!("PASS{}", outcome.summary));
            Ok(())
        }
        Err(e) => {
            output::ratatoskr_msg(&format!("FAIL{}: {e}", outcome.summary));
            if let Some(path) = outcome.preserved {
                output::ratatoskr_msg(&format!("artefacts preserved at {}", path.display()));
                artefacts::emit_clean_hint();
            }
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// shared plumbing: config validation, script resolution, the per-script run
// ---------------------------------------------------------------------------

/// Validate the `[ratatoskr]` surface every sync run needs and resolve
/// sæhrimnir's paths. Shared by the single-script and cohort shapes so a
/// cohort fails up front instead of once per script.
fn validate_sync_config<'a>(
    project_root: &Path,
    dev_config: &'a DevConfig,
) -> Result<(&'a RatatoskrConfig, &'a HarnessConfig, PathBuf, PathBuf), DevError> {
    let cfg = dev_config.ratatoskr.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync: no [ratatoskr] section in brokkr.toml. \
             Required to locate sæhrimnir and the harness binary."
                .into(),
        )
    })?;
    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync: no [ratatoskr.harness] section in brokkr.toml. \
             Declare it with `package = \"<crate>\"` (and optional \
             `binary`, `features`, `debug`)."
                .into(),
        )
    })?;
    let mock_binary = require_path(&cfg.mock_server_binary, project_root, "mock_server_binary")?;
    let fixtures_dir = require_path(&cfg.fixtures_dir, project_root, "fixtures_dir")?;
    if !mock_binary.exists() {
        return Err(DevError::Config(format!(
            "sync: sæhrimnir binary not found at {}. Build it first.",
            mock_binary.display()
        )));
    }
    Ok((cfg, harness_cfg, mock_binary, fixtures_dir))
}

/// A sync script resolved to everything a run needs: canonical path,
/// artefact key, fixture, ceiling. Produced before the lock and build in
/// the single shape (fail fast) and per script inside the cohort loop.
struct ResolvedScript {
    script_abs: PathBuf,
    /// Artefact-dir key - the script's file stem.
    test_id: String,
    fixture_name: String,
    fixture_path: PathBuf,
    ceiling: Duration,
}

/// Resolve a parsed script: canonicalize the path, derive the artefact
/// key from the stem, require the `fixture:` frontmatter and resolve it.
fn resolve_info(info: &ScriptInfo, fixtures_dir: &Path) -> Result<ResolvedScript, DevError> {
    let script_abs = info
        .path
        .canonicalize()
        .map_err(|e| DevError::Config(format!("sync: canonicalize script: {e}")))?;
    let test_id = script_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            DevError::Config(format!("sync: script has no stem: {}", script_abs.display()))
        })?
        .to_owned();
    let fixture_name = info.fixture.clone().ok_or_else(|| {
        DevError::Config(format!(
            "sync: script {} has no `-- fixture: <NAME>` frontmatter line. \
             Required so brokkr knows which sæhrimnir fixture to load.",
            info.name
        ))
    })?;
    let fixture_path = resolve_fixture(fixtures_dir, &fixture_name)?;
    Ok(ResolvedScript {
        script_abs,
        test_id,
        fixture_name,
        fixture_path,
        ceiling: info.ceiling,
    })
}

/// Resolve a script named on the command line: existence check, parse
/// frontmatter, then the shared [`resolve_info`].
fn resolve_single(script: &str, fixtures_dir: &Path) -> Result<ResolvedScript, DevError> {
    let script_path = Path::new(script);
    if !script_path.is_file() {
        return Err(DevError::Config(format!(
            "sync: script not found or not a file: {script}"
        )));
    }
    let stem = script_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| DevError::Config(format!("sync: script has no stem: {script}")))?;
    let parsed = discover::parse_script(script_path, stem)
        .map_err(|e| DevError::Config(format!("sync: parse script: {e}")))?;
    resolve_info(&parsed, fixtures_dir)
}

/// Everything a per-script run needs that is invariant across a cohort:
/// validated config, resolved sæhrimnir binary, and the built harness.
struct SyncRunner<'a> {
    project_root: &'a Path,
    cfg: &'a RatatoskrConfig,
    built: &'a HarnessBuild,
    mock_binary: &'a Path,
    keep_artefacts: bool,
}

/// Outcome of one script run. The caller owns the PASS/FAIL line - the
/// single and cohort shapes format it differently - so the pieces travel
/// separately: the phase-summary clause, the preserved artefact dir when
/// a failed run left one, and the result itself.
struct ScriptOutcome {
    summary: String,
    preserved: Option<PathBuf>,
    result: Result<(), DevError>,
}

impl SyncRunner<'_> {
    /// Run one resolved script against the prebuilt harness: allocate the
    /// artefact dir, orchestrate the two children, finalize. Prints no
    /// verdict - that is the caller's line - and never short-circuits the
    /// artefact-dir lifecycle: an early spawn error still finalizes as a
    /// failure with the dir preserved.
    fn run_resolved(&self, resolved: &ResolvedScript, lock: &lockfile::LockGuard) -> ScriptOutcome {
        let mut timings = PhaseTimings::default();
        let mut preserved = None;
        let result = (|| -> Result<(), DevError> {
            let artefact_parent = self.project_root.join(SYNC_ARTEFACT_PARENT);
            let artefacts =
                ArtefactDir::allocate(&artefact_parent, &resolved.test_id, self.keep_artefacts)?;
            let harness_dir = artefacts.path().join("harness");
            let mock_dir = artefacts.path().join("mock");
            fs::create_dir_all(&harness_dir).map_err(DevError::Io)?;
            fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;

            match self.orchestrate(resolved, &harness_dir, &mock_dir, &mut timings, lock) {
                Ok(()) => artefacts.finalize_success(),
                Err(e) => {
                    preserved = Some(artefacts.path().to_path_buf());
                    artefacts.finalize_failure();
                    Err(e)
                }
            }
        })();
        ScriptOutcome {
            summary: timings.summary(),
            preserved,
            result,
        }
    }
}

/// Per-phase wall-clock timings for an unmeasured `sync` run. Each field is `None`
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

impl SyncRunner<'_> {
    /// The two-child orchestration body. Separate from
    /// [`SyncRunner::run_resolved`] so the artefact-dir finalize calls
    /// are unconditional - even if this returns early on a spawn error,
    /// the caller still records PASS/FAIL via the artefact-dir lifecycle.
    fn orchestrate(
        &self,
        resolved: &ResolvedScript,
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
            self.mock_binary,
            &resolved.fixture_path,
            mock_dir,
            Some(&|pid| lock.add_mock_pid(pid)),
            Some(&|pid| lock.remove_mock_pid(pid)),
            true, // isolate_pg: the caller's SigtermGuard covers it
        )?;
        // Don't seed `child_pid` with the mock's PID - the captured runner's
        // `on_spawn` callback will publish the harness PID seconds from now,
        // and a transient `child_pid == mock_pid` window means a `--hard`
        // landing in that gap would SIGKILL the mock twice (once via
        // `mock_pid`, once via `child_pid`) and the harness not at all.
        timings.mock_ready = Some(mock.ready_elapsed());
        let endpoint_envs = endpoint_env_pairs(self.cfg, mock.endpoints());

        let bin_dir_str = self.built.bin_dir.display().to_string();
        let harness_dir_str = harness_dir.display().to_string();
        let script_str = resolved.script_abs.display().to_string();

        let mut env_pairs: Vec<(&str, &str)> = vec![
            ("BROKKR_HARNESS_ARTEFACT_DIR", &harness_dir_str),
            ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
        ];
        for (name, value) in &endpoint_envs {
            env_pairs.push((name.as_str(), value.as_str()));
        }

        // The frontmatter ceiling (or discover's generous default) keeps
        // a hung script from wedging the lockfile forever. This is the
        // smoke shape, not the bench shape - the ceiling is a watchdog,
        // not a measurement.
        let ceiling = resolved.ceiling;

        let binary_str = self.built.binary.display().to_string();
        let deadline_capture = output::run_captured_with_env_and_deadline(
            &binary_str,
            &["--test-harness", &script_str],
            self.project_root,
            &env_pairs,
            ceiling,
            Some(&|pid| lock.set_child_pid(pid)),
            true, // isolate_pg: the caller's SigtermGuard active
        );

        // Capture harness elapsed before tearing down sæhrimnir so a
        // ceiling-kill or non-zero exit still surfaces a harness duration in
        // the summary line.
        if let Ok(dc) = deadline_capture.as_ref() {
            timings.harness = Some(dc.captured.elapsed);
        }

        // Whatever the harness did, sæhrimnir gets torn down next.
        let mock_outcome = mock.shutdown();
        // this path has at most one mock alive at a time; clear all is the
        // honest call after that single mock drains.
        lock.clear_mock_pids();
        lock.clear_child_pid();
        timings.mock_shutdown = Some(mock_outcome.shutdown_elapsed);

        let dc = deadline_capture?;
        fs::write(harness_dir.join("binary-stdout.log"), &dc.captured.stdout)
            .map_err(DevError::Io)?;
        fs::write(harness_dir.join("binary-stderr.log"), &dc.captured.stderr)
            .map_err(DevError::Io)?;
        write_run_toml(
            harness_dir,
            mock_dir,
            &resolved.script_abs,
            self.built,
            &dc,
            &mock_outcome,
        )?;

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
// sync <SCRIPT> --bench N - measure one
// ---------------------------------------------------------------------------

/// CLI inputs for `brokkr sync <SCRIPT> --bench N`.
pub struct SyncBenchRequest<'a> {
    pub project_root: &'a Path,
    /// The code tree to build and read git state from, when it differs from
    /// `project_root`: the `--commit` worktree, or cwd when `brokkr.toml`
    /// lives one level up. `None` in the common case.
    ///
    /// Only the *harness build* moves. The script, the fixture, sæhrimnir,
    /// the artefact dir and both databases stay anchored to `project_root`:
    /// the same split-tree rule dellingr applies to its Lua workload, and
    /// for the same reason, that a baseline should vary the code under test
    /// and nothing else. The gate additionally depends on it, since it
    /// compares the baseline row's script path against the current run's.
    pub build_root: Option<&'a Path>,
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

