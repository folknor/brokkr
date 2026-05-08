//! Plan-3 sync orchestration: `sync-list` and `sync-smoke`.
//!
//! `sync-list` is pure brokkr - walk the configured sync-script
//! directory, parse frontmatter, print a sorted table. No ratatoskr or
//! sæhrimnir runtime dependency.
//!
//! `sync-smoke` builds the harness sweep (plan 1's `[ratatoskr.harness]`),
//! spawns sæhrimnir against the script's declared fixture, parses the
//! per-protocol ports out of the readiness sentinel, then spawns
//! `<harness binary> --test-harness <SCRIPT>` with the
//! `RATATOSKR_TEST_*_ENDPOINT` env-var family injected (only those
//! whose names ratatoskr's `brokkr.toml` has spelled out). When the
//! harness exits, brokkr SIGTERMs sæhrimnir with the
//! [`saehrimnir::SHUTDOWN_BUDGET`] before escalating to SIGKILL.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::{DevConfig, RatatoskrConfig};
use crate::error::DevError;
use crate::lockfile::{self, LockContext};
use crate::output;
use crate::ratatoskr::artefacts::ArtefactDir;
use crate::ratatoskr::build::{self, HarnessBuild};
use crate::ratatoskr::discover::{self, ScriptInfo};
use crate::ratatoskr::process as proc_helpers;
use crate::ratatoskr::saehrimnir::{
    require_path, resolve_fixture, wait_for_endpoints, wait_with_deadline, Endpoints,
    READINESS_BUDGET, SHUTDOWN_BUDGET,
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
    pub debug: bool,
}

/// Drive `brokkr sync-smoke` end-to-end:
///
/// 1. Validate config: `[ratatoskr.harness]`, `mock_server_binary`, and
///    `fixtures_dir` are all required. Endpoint env-var names are
///    optional - protocols without a configured spelling just don't
///    get an env var.
/// 2. Parse the script's frontmatter; require a `fixture: <NAME>`.
/// 3. Acquire the global lockfile.
/// 4. Build the harness sweep (same feature contract as `brokkr check`).
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
pub fn run_sync_smoke(req: &SyncSmokeRequest<'_>) -> Result<(), DevError> {
    let cfg = req.dev_config.ratatoskr.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync-smoke: no [ratatoskr] section in brokkr.toml. \
             Required to locate sæhrimnir and the harness sweep."
                .into(),
        )
    })?;
    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "sync-smoke: no [ratatoskr.harness] section in brokkr.toml. \
             Required to know which sweep to build."
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

    let built = build::build_for_harness(
        req.project_root,
        &req.dev_config.check,
        harness_cfg,
        req.debug,
    )?;
    output::ratatoskr_msg(&format!(
        "harness build ok (sweep={}, binary={})",
        built.sweep_label,
        built.binary.display(),
    ));

    let artefact_parent = req.project_root.join(SYNC_ARTEFACT_PARENT);
    let artefacts = ArtefactDir::allocate(&artefact_parent, &test_id, req.keep_artefacts)?;
    let harness_dir = artefacts.path().join("harness");
    let mock_dir = artefacts.path().join("mock");
    fs::create_dir_all(&harness_dir).map_err(DevError::Io)?;
    fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;

    output::ratatoskr_msg(&format!("running {test_id} (fixture: {fixture_name})"));

    let outcome = orchestrate(
        req,
        cfg,
        &built,
        &mock_binary,
        &fixture_path,
        &script_abs,
        &harness_dir,
        &mock_dir,
    );

    match outcome {
        Ok(()) => {
            output::ratatoskr_msg("PASS");
            artefacts.finalize_success()?;
            Ok(())
        }
        Err(e) => {
            output::ratatoskr_msg(&format!("FAIL: {e}"));
            let path = artefacts.path().to_path_buf();
            artefacts.finalize_failure();
            output::ratatoskr_msg(&format!("artefacts preserved at {}", path.display()));
            Err(e)
        }
    }
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
) -> Result<(), DevError> {
    let readiness = mock_dir.join("readiness");
    let mock_stderr_log = mock_dir.join("stderr.log");
    let mock_stderr = File::create(&mock_stderr_log).map_err(DevError::Io)?;

    let fixture_str = fixture_path.display().to_string();
    let readiness_str = readiness.display().to_string();
    let mut mock_child = Command::new(mock_binary)
        .args([
            "--fixture",
            &fixture_str,
            "--readiness-file",
            &readiness_str,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(mock_stderr)
        .spawn()
        .map_err(|e| DevError::Subprocess {
            program: mock_binary.display().to_string(),
            code: None,
            stderr: format!("failed to spawn: {e}"),
        })?;
    let mock_pid = mock_child.id();

    let endpoints = match wait_for_endpoints(&mut mock_child, &readiness, READINESS_BUDGET) {
        Ok(ep) => ep,
        Err(e) => {
            // Best-effort cleanup; sæhrimnir might already be dead, in
            // which case ESRCH is fine. Stderr already captured to
            // mock_stderr_log so the user can diagnose.
            let _term = proc_helpers::send_signal(mock_pid, libc::SIGTERM);
            let _reaped = mock_child.wait();
            return Err(e);
        }
    };

    let endpoint_envs = endpoint_env_pairs(cfg, &endpoints);
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
    );

    // Whatever the harness did, sæhrimnir gets torn down next.
    let mock_outcome = teardown_mock(&mut mock_child, mock_pid);

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

/// Build a list of `(env_var_name, value)` pairs for the protocols whose
/// `test_endpoint_env_<proto>` field is set in `[ratatoskr]`. Owned
/// strings so the caller can hand `&str` views into
/// `run_captured_with_env_and_deadline` without lifetime gymnastics.
///
/// URL shapes match what ratatoskr's existing client code expects:
/// HTTP origins for the JSON-over-HTTP protocols, `host:port` for the
/// stream protocols.
fn endpoint_env_pairs(cfg: &RatatoskrConfig, endpoints: &Endpoints) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(name) = &cfg.test_endpoint_env_jmap {
        out.push((name.clone(), format!("http://127.0.0.1:{}", endpoints.jmap)));
    }
    if let Some(name) = &cfg.test_endpoint_env_imap {
        out.push((name.clone(), format!("127.0.0.1:{}", endpoints.imap)));
    }
    if let Some(name) = &cfg.test_endpoint_env_smtp {
        out.push((name.clone(), format!("127.0.0.1:{}", endpoints.smtp)));
    }
    if let Some(name) = &cfg.test_endpoint_env_graph {
        out.push((name.clone(), format!("http://127.0.0.1:{}", endpoints.graph)));
    }
    if let Some(name) = &cfg.test_endpoint_env_gmail {
        out.push((name.clone(), format!("http://127.0.0.1:{}", endpoints.gmail)));
    }
    out
}

/// Outcome shape recorded for sæhrimnir in `run.toml`. None of these
/// fields gate PASS/FAIL - the harness binary's exit code is the test's
/// verdict. They're purely diagnostic.
struct MockOutcome {
    exit_code: Option<i32>,
    signal: Option<i32>,
    killed_after_budget: bool,
}

/// SIGTERM sæhrimnir, grant [`SHUTDOWN_BUDGET`], escalate to SIGKILL on
/// timeout. Always reaps the child so the caller never sees a zombie.
fn teardown_mock(child: &mut std::process::Child, pid: u32) -> MockOutcome {
    use std::os::unix::process::ExitStatusExt;

    let _term = proc_helpers::send_signal(pid, libc::SIGTERM);
    match wait_with_deadline(child, pid, SHUTDOWN_BUDGET) {
        Ok(status) => {
            // wait_with_deadline returns Ok even after SIGKILL because
            // it always reaps. Distinguish the two paths via signal().
            let killed_after_budget = matches!(status.signal(), Some(s) if s == libc::SIGKILL);
            MockOutcome {
                exit_code: status.code(),
                signal: status.signal(),
                killed_after_budget,
            }
        }
        Err(_) => MockOutcome {
            exit_code: None,
            signal: None,
            killed_after_budget: true,
        },
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
        "brokkr_version = \"{}\"\nscript = \"{}\"\nharness_binary = \"{}\"\nsweep = \"{}\"\nharness_elapsed_ms = {}\n",
        env!("CARGO_PKG_VERSION"),
        script_abs.display(),
        built.binary.display(),
        built.sweep_label,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::HarnessConfig;
    use std::path::Path;

    fn cfg_with_endpoints() -> RatatoskrConfig {
        RatatoskrConfig {
            harness: Some(HarnessConfig {
                sweep: "harness".into(),
                binary: "app".into(),
            }),
            mock_server_binary: None,
            fixtures_dir: None,
            test_endpoint_env_jmap: Some("RATATOSKR_TEST_JMAP_ENDPOINT".into()),
            test_endpoint_env_imap: Some("RATATOSKR_TEST_IMAP_ENDPOINT".into()),
            test_endpoint_env_smtp: None,
            test_endpoint_env_graph: None,
            test_endpoint_env_gmail: Some("RATATOSKR_TEST_GMAIL_ENDPOINT".into()),
            sync_script_dir: None,
        }
    }

    fn endpoints() -> Endpoints {
        Endpoints {
            jmap: 1001,
            imap: 1002,
            smtp: 1003,
            graph: 1004,
            gmail: 1005,
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
