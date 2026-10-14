//! Shared sync-run setup, script resolution, and per-script orchestration.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::artefacts::ArtefactDir;
use crate::config::HarnessConfig;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::DevError;
use crate::git;
use crate::lockfile;
use crate::output;
use crate::ratatoskr::build::HarnessBuild;
use crate::ratatoskr::discover::{self, ScriptInfo};
use crate::ratatoskr::saehrimnir::{
    MockOutcome, MockServer, endpoint_env_pairs, require_path, resolve_fixture,
};
use crate::ratatoskr::timing::format_secs;

/// Where per-test sync artefact dirs live relative to the project root.
pub(super) const SYNC_ARTEFACT_PARENT: &str = ".brokkr/ratatoskr/sync";

// ---------------------------------------------------------------------------
// shared plumbing: config validation, script resolution, the per-script run
// ---------------------------------------------------------------------------

/// Validate the `[ratatoskr]` surface every sync run needs and resolve
/// sæhrimnir's paths. Shared by the single-script and cohort shapes so a
/// cohort fails up front instead of once per script.
pub(super) fn validate_sync_config<'a>(
    project_root: &Path,
    cfg: &'a RatatoskrConfig,
) -> Result<(&'a RatatoskrConfig, &'a HarnessConfig, PathBuf, PathBuf), DevError> {
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
pub(super) struct ResolvedScript {
    pub(super) script_abs: PathBuf,
    /// Artefact-dir key - the script's file stem.
    pub(super) test_id: String,
    pub(super) fixture_name: String,
    pub(super) fixture_path: PathBuf,
    pub(super) ceiling: Duration,
}

/// Resolve a parsed script: canonicalize the path, derive the artefact
/// key from the stem, require the `fixture:` frontmatter and resolve it.
pub(super) fn resolve_info(
    info: &ScriptInfo,
    fixtures_dir: &Path,
) -> Result<ResolvedScript, DevError> {
    let script_abs = info
        .path
        .canonicalize()
        .map_err(|e| DevError::Config(format!("sync: canonicalize script: {e}")))?;
    let test_id = script_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            DevError::Config(format!(
                "sync: script has no stem: {}",
                script_abs.display()
            ))
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
pub(super) fn resolve_single(
    script: &str,
    fixtures_dir: &Path,
) -> Result<ResolvedScript, DevError> {
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
pub(super) struct SyncRunner<'a> {
    pub(super) project_root: &'a Path,
    pub(super) cfg: &'a RatatoskrConfig,
    pub(super) built: &'a HarnessBuild,
    pub(super) mock_binary: &'a Path,
    pub(super) keep_artefacts: bool,
}

/// Outcome of one script run. The caller owns the PASS/FAIL line - the
/// single and cohort shapes format it differently - so the pieces travel
/// separately: the phase-summary clause, the preserved artefact dir when
/// a failed run left one, and the result itself.
pub(super) struct ScriptOutcome {
    pub(super) summary: String,
    pub(super) preserved: Option<PathBuf>,
    pub(super) result: Result<(), DevError>,
}

impl SyncRunner<'_> {
    /// Run one resolved script against the prebuilt harness: allocate the
    /// artefact dir, orchestrate the two children, finalize. Prints no
    /// verdict - that is the caller's line - and never short-circuits the
    /// artefact-dir lifecycle: an early spawn error still finalizes as a
    /// failure with the dir preserved.
    pub(super) fn run_resolved(
        &self,
        resolved: &ResolvedScript,
        lock: &lockfile::LockGuard,
    ) -> ScriptOutcome {
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
            &resolved.script_abs,
            self.built,
            &dc,
            &mock_outcome,
            self.project_root,
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

/// Top-level `run.toml` for a sync run. Field names match service's
/// `run.toml` (`binary`, `elapsed_ms`, `exit_code`, `signal`, the `git_*`
/// trio) so one reader handles both; the sync-only parts are the deadline
/// flag and the `[mock]` table.
#[derive(Serialize)]
struct SyncRunMetadata {
    brokkr_version: String,
    script: String,
    binary: String,
    features: String,
    /// The harness binary's wall time.
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    killed_on_deadline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_clean: Option<bool>,
    /// Last, so it serializes as a trailing `[mock]` table.
    mock: SyncMockMetadata,
}

#[derive(Serialize)]
struct SyncMockMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    killed_after_budget: bool,
}

/// Render the sync `run.toml` body. Serialized, never `format!`ed: paths,
/// the features label and the git subject go into TOML strings, and any
/// `"` or `\` in them would otherwise produce a file no TOML reader can
/// open. Pure so the escaping is testable without a run.
pub(super) fn render_run_toml(
    script_abs: &Path,
    built: &HarnessBuild,
    dc: &output::DeadlineCapture,
    mock: &MockOutcome,
    git: Option<&git::GitInfo>,
) -> Result<String, DevError> {
    use std::os::unix::process::ExitStatusExt;
    let meta = SyncRunMetadata {
        brokkr_version: env!("CARGO_PKG_VERSION").to_owned(),
        script: script_abs.display().to_string(),
        binary: built.binary.display().to_string(),
        features: built.features_label.clone(),
        elapsed_ms: u64::try_from(dc.captured.elapsed.as_millis()).unwrap_or(u64::MAX),
        exit_code: dc.captured.status.code(),
        signal: dc.captured.status.signal(),
        killed_on_deadline: dc.killed_on_deadline,
        git_commit: git.map(|g| g.commit.clone()),
        git_subject: git.map(|g| g.subject.clone()),
        git_clean: git.map(|g| g.is_clean),
        mock: SyncMockMetadata {
            exit_code: mock.exit_code,
            signal: mock.signal,
            killed_after_budget: mock.killed_after_budget,
        },
    };
    toml::to_string(&meta)
        .map_err(|e| DevError::Config(format!("sync: failed to serialize run.toml: {e}")))
}

/// Write top-level `run.toml` with reproducibility metadata. Mock and
/// harness keep their own subdir state; this top-level file ties them
/// together for triage.
fn write_run_toml(
    harness_dir: &Path,
    script_abs: &Path,
    built: &HarnessBuild,
    dc: &output::DeadlineCapture,
    mock: &MockOutcome,
    project_root: &Path,
) -> Result<(), DevError> {
    let git_info = git::collect(project_root).ok();
    let body = render_run_toml(script_abs, built, dc, mock, git_info.as_ref())?;
    fs::write(
        harness_dir.parent().unwrap_or(harness_dir).join("run.toml"),
        body,
    )
    .map_err(DevError::Io)?;
    Ok(())
}
