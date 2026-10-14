//! Shared sync-run setup, script resolution, and per-script orchestration.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::artefacts::ArtefactDir;
use crate::clean::layout::ProjectState;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::DevError;
use crate::git;
use crate::lockfile;
use crate::output;
use crate::output::format_duration;
use crate::ratatoskr::build::HarnessBuild;
use crate::ratatoskr::discover::{self, ScriptInfo, ScriptKind};
use crate::ratatoskr::saehrimnir::{
    ManagedMock, MockArtifactScope, MockOutcome, MockPaths, resolve_fixture,
};
use crate::ratatoskr::{HARNESS_MODE_FLAG, harness_env};

use super::gate::validate_reserved_gate;

// ---------------------------------------------------------------------------
// shared plumbing: config validation, script resolution, the per-script run
// ---------------------------------------------------------------------------

/// Validate the `[ratatoskr]` surface every sync run needs and resolve
/// sæhrimnir's paths. Shared by every sync shape so config failures happen
/// before the build.
pub(super) fn validate_sync_config<'a>(
    project_root: &Path,
    cfg: &'a RatatoskrConfig,
) -> Result<
    (
        &'a RatatoskrConfig,
        &'a crate::config::HarnessConfig,
        PathBuf,
        PathBuf,
    ),
    DevError,
> {
    validate_reserved_gate(cfg)?;
    let harness_cfg = crate::ratatoskr::require_harness_config(cfg, "sync")?;
    let paths = MockPaths::resolve(cfg, project_root)
        .map_err(|e| DevError::Config(format!("sync: {e}")))?;
    Ok((cfg, harness_cfg, paths.binary, paths.fixtures_dir))
}

/// A sync script resolved to everything a run needs: canonical path,
/// artefact key, fixture, ceiling. Produced for every selected script before
/// the lock and build.
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
    let fixture_name = match &info.kind {
        ScriptKind::Sync { fixture, .. } => fixture.clone(),
        ScriptKind::Service { .. } => {
            return Err(DevError::Config(format!(
                "sync: script {} was discovered as a service script",
                info.name
            )));
        }
    };
    let fixture_path = resolve_fixture(fixtures_dir, &fixture_name)?;
    Ok(ResolvedScript {
        script_abs,
        test_id,
        fixture_name,
        fixture_path,
        ceiling: info.ceiling,
    })
}

/// Resolve a script named on the command line through the shared path logic.
pub(super) fn resolve_single(
    script: &str,
    fixtures_dir: &Path,
) -> Result<ResolvedScript, DevError> {
    resolve_path(Path::new(script), fixtures_dir)
}

/// Resolve a script path supplied by config or the command line.
pub(super) fn resolve_path(
    script_path: &Path,
    fixtures_dir: &Path,
) -> Result<ResolvedScript, DevError> {
    if !script_path.is_file() {
        return Err(DevError::Config(format!(
            "sync: script not found or not a file: {}",
            script_path.display()
        )));
    }
    let stem = script_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            DevError::Config(format!(
                "sync: script has no stem: {}",
                script_path.display()
            ))
        })?;
    let parsed = discover::parse_sync_script(script_path, stem)
        .map_err(|e| DevError::Config(format!("sync: parse script: {e}")))?;
    resolve_info(&parsed, fixtures_dir)
}

/// Everything a per-script run needs that is invariant across a cohort:
/// validated config, resolved sæhrimnir binary, and the built harness.
pub(super) struct SyncRunner<'a> {
    pub(super) project_root: &'a Path,
    /// The composed config's `include` files, exempt from the clean-tree
    /// flag `run.toml` records.
    pub(super) included_config_files: &'a [PathBuf],
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
            let state = ProjectState::new(self.project_root);
            let artefact_parent = state.ratatoskr().sync_artifacts();
            let artefacts =
                ArtefactDir::allocate(&artefact_parent, &resolved.test_id, self.keep_artefacts)?;
            let harness_dir = artefacts.path().join("harness");
            fs::create_dir_all(&harness_dir).map_err(DevError::Io)?;

            match self.orchestrate(resolved, artefacts.path(), &harness_dir, &mut timings, lock) {
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
/// faithful summary (e.g. `FAIL in 400ms (mock 400ms)` if sæhrimnir died
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
            parts.push(format!("mock {}", format_duration(d)));
            total += d;
        }
        if let Some(d) = self.harness {
            parts.push(format!("harness {}", format_duration(d)));
            total += d;
        }
        if let Some(d) = self.mock_shutdown {
            parts.push(format!("shutdown {}", format_duration(d)));
            total += d;
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" in {} ({})", format_duration(total), parts.join(", "))
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
        run_dir: &Path,
        harness_dir: &Path,
        timings: &mut PhaseTimings,
        lock: &lockfile::LockGuard,
    ) -> Result<(), DevError> {
        // Publish the mock PID from INSIDE spawn_observed - before the
        // readiness wait - so a `brokkr kill --hard` landing during
        // sæhrimnir startup finds the mock and SIGKILLs it instead of
        // orphaning it.
        let mock = ManagedMock::start(
            self.cfg,
            self.mock_binary,
            &resolved.fixture_path,
            &resolved.fixture_name,
            MockArtifactScope::Run(run_dir),
            lock,
        )?;
        // Don't seed `child_pid` with the mock's PID - the captured runner's
        // `on_spawn` callback will publish the harness PID seconds from now,
        // and a transient `child_pid == mock_pid` window means a `--hard`
        // landing in that gap would SIGKILL the mock twice (once via
        // `mock_pid`, once via `child_pid`) and the harness not at all.
        timings.mock_ready = Some(mock.ready_elapsed());
        let endpoint_envs = mock.env_pair_refs();

        let script_str = resolved.script_abs.display().to_string();
        let common_env = harness_env(harness_dir, &self.built.bin_dir);
        let mut env_pairs = common_env.as_pairs().to_vec();
        for &(name, value) in &endpoint_envs {
            env_pairs.push((name, value));
        }

        // The frontmatter ceiling (or discover's generous default) keeps
        // a hung script from wedging the lockfile forever. This is the
        // smoke shape, not the bench shape - the ceiling is a watchdog,
        // not a measurement.
        let ceiling = resolved.ceiling;

        let binary_str = self.built.binary.display().to_string();
        // Guards nest; this one is the borrow the process-group isolation needs.
        let signal_guard = crate::shutdown::SigtermGuard::install();
        let deadline_capture = output::run_captured_with_env_and_deadline_with_options(
            &binary_str,
            &[HARNESS_MODE_FLAG, &script_str],
            self.project_root,
            &env_pairs,
            ceiling,
            Some(&|pid| lock.set_child_pid(pid)),
            crate::shutdown::ChildIsolation::ProcessGroup(&signal_guard),
            output::child::ChildOptions {
                capture_cap: output::child::CaptureCap::LARGE,
                sigterm_grace: crate::ratatoskr::saehrimnir::SHUTDOWN_BUDGET,
                ..output::child::ChildOptions::DEFAULT
            },
        );
        drop(signal_guard);

        // Capture harness elapsed before tearing down sæhrimnir so a
        // ceiling-kill or non-zero exit still surfaces a harness duration in
        // the summary line.
        if let Ok(dc) = deadline_capture.as_ref() {
            timings.harness = Some(dc.captured.elapsed);
        }

        // Whatever the harness did, sæhrimnir gets torn down next.
        let mock_outcome = mock.shutdown();
        lock.clear_child_pid();
        timings.mock_shutdown = mock_outcome
            .as_ref()
            .ok()
            .map(|outcome| outcome.shutdown_elapsed);

        let dc = deadline_capture?;
        fs::write(harness_dir.join("binary-stdout.log"), &dc.captured.stdout)
            .map_err(DevError::Io)?;
        fs::write(harness_dir.join("binary-stderr.log"), &dc.captured.stderr)
            .map_err(DevError::Io)?;
        let mock_outcome = mock_outcome?;
        write_run_toml(
            harness_dir,
            &resolved.script_abs,
            self.built,
            &dc,
            &mock_outcome,
            self.project_root,
            self.included_config_files,
        )?;

        if dc.killed_on_deadline() {
            return Err(DevError::Subprocess {
                program: binary_str,
                ending: crate::error::ProcessEnding::Signal(libc::SIGKILL),
                stderr: format!("brokkr killed the harness after ceiling {ceiling:?}"),
            });
        }
        dc.captured.check_success(&binary_str)?;
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
    let meta = SyncRunMetadata {
        brokkr_version: env!("CARGO_PKG_VERSION").to_owned(),
        script: script_abs.display().to_string(),
        binary: built.binary.display().to_string(),
        features: built.features_label.clone(),
        elapsed_ms: u64::try_from(dc.captured.elapsed.as_millis()).unwrap_or(u64::MAX),
        exit_code: dc.captured.code(),
        signal: dc.captured.signal(),
        killed_on_deadline: dc.killed_on_deadline(),
        git_commit: git.map(|g| g.commit.as_str().to_owned()),
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
    included_config_files: &[PathBuf],
) -> Result<(), DevError> {
    let git_info = git::collect_with_config_paths(project_root, included_config_files).ok();
    let body = render_run_toml(script_abs, built, dc, mock, git_info.as_ref())?;
    fs::write(
        harness_dir.parent().unwrap_or(harness_dir).join("run.toml"),
        body,
    )
    .map_err(DevError::Io)?;
    Ok(())
}
