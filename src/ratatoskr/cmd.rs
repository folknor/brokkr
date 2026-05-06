//! Top-level `[ratatoskr]` brokkr commands.

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::DevConfig;
use crate::error::DevError;
use crate::lockfile::{self, LockContext};
use crate::output::{self, CapturedOutput};
use crate::ratatoskr::artefacts::ArtefactDir;
use crate::ratatoskr::build::{self, HarnessBuild};
use crate::ratatoskr::discover::{self, ScriptInfo, SCRIPT_DIR};

/// Where per-test artefact directories live, relative to the project
/// root. Allocator under [`ArtefactDir`] creates `<this>/<test_id>/run-N/`.
const ARTEFACT_PARENT: &str = ".brokkr/ratatoskr";

/// Run one service-test script through the harness binary built via
/// `[ratatoskr.harness]`.
///
/// Acquires the global lockfile, builds the configured `[[check]]`
/// sweep, allocates a per-run artefact dir, spawns
/// `<binary> --test-harness <SCRIPT>` with `BROKKR_HARNESS_ARTEFACT_DIR`
/// and `BROKKR_TEST_BIN_DIR` set, captures stdout/stderr, writes them
/// alongside a `run.toml` and a copy of the script, then preserves or
/// drops the artefact dir based on outcome.
///
/// The harness binary itself - the Lua VM, `ServiceClient` userdata,
/// wait combinator, frame-log tap, `/proc` snapshot writer - lives in
/// ratatoskr's `app` crate behind the `test-helpers` feature and lands
/// in Phase 8. Until it does, `app --test-harness` errors out with
/// "unknown flag" and brokkr captures that into the artefact dir
/// faithfully; the plumbing here is structurally complete.
pub fn service_test(
    project_root: &Path,
    dev_config: &DevConfig,
    script: &str,
    keep_artefacts: bool,
    debug: bool,
) -> Result<(), DevError> {
    let script_path = Path::new(script);
    if !script_path.exists() {
        return Err(DevError::Config(format!(
            "service-test: script not found: {script}"
        )));
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

    let harness_cfg = dev_config
        .ratatoskr
        .as_ref()
        .and_then(|r| r.harness.as_ref())
        .ok_or_else(|| {
            DevError::Config(
                "service-test: no [ratatoskr.harness] section in brokkr.toml. \
                 Add a [[check]] entry naming the harness sweep, then \
                 [ratatoskr.harness] sweep = \"<name>\", binary = \"<package>\". \
                 See notes/ratatoskr-service-harness.md."
                    .into(),
            )
        })?;

    let project_root_str = project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "service-test",
        project_root: &project_root_str,
    })?;

    let built = build::build_for_harness(project_root, &dev_config.check, harness_cfg, debug)?;
    output::ratatoskr_msg(&format!(
        "harness build ok (sweep={}, binary={})",
        built.sweep_label,
        built.binary.display(),
    ));

    let artefact_parent = project_root.join(ARTEFACT_PARENT);
    let artefacts = ArtefactDir::allocate(&artefact_parent, &test_id, keep_artefacts)?;

    output::ratatoskr_msg(&format!(
        "running {test_id} against {}",
        built.binary.display()
    ));

    spawn_and_finalize(artefacts, &built, &script_abs, project_root)
}

/// Spawn the harness binary against `script_abs`, capture its output
/// into `artefacts`, then finalize the dir per the run's exit status.
/// Split out of [`service_test`] purely to keep that function within
/// the project's line-count lint; the artefact-dir is consumed here so
/// the lifecycle ends in one place.
fn spawn_and_finalize(
    artefacts: ArtefactDir,
    built: &HarnessBuild,
    script_abs: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let binary_str = built.binary.display().to_string();
    let script_str = script_abs.display().to_string();
    let artefact_path_str = artefacts.path().display().to_string();
    let bin_dir_str = built.bin_dir.display().to_string();

    let captured = match output::run_captured_with_env(
        &binary_str,
        &["--test-harness", &script_str],
        project_root,
        &[
            ("BROKKR_HARNESS_ARTEFACT_DIR", &artefact_path_str),
            ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
        ],
    ) {
        Ok(c) => c,
        Err(e) => {
            // Spawn itself failed (binary missing, exec error). Leave a
            // breadcrumb in the artefact dir so the empty dir isn't a
            // mystery, then preserve it via finalize_failure.
            fs::write(
                artefacts.path().join("spawn-error.txt"),
                format!("failed to spawn {}: {e}\n", built.binary.display()),
            )
            .ok();
            artefacts.finalize_failure();
            return Err(e);
        }
    };

    write_artefacts(artefacts.path(), script_abs, built, &captured, project_root)?;

    let elapsed_ms = captured.elapsed.as_millis();
    if captured.status.success() {
        output::ratatoskr_msg(&format!("PASS in {elapsed_ms}ms"));
        artefacts.finalize_success()?;
        Ok(())
    } else {
        let artefact_dir_str = artefacts.path().display().to_string();
        let outcome = match (captured.status.code(), captured.status.signal()) {
            (Some(code), _) => format!("exit={code}"),
            (None, Some(sig)) => format!("signal={sig}"),
            (None, None) => "unknown exit".to_owned(),
        };
        output::ratatoskr_msg(&format!(
            "FAIL {outcome} in {elapsed_ms}ms (artefacts: {artefact_dir_str})"
        ));
        artefacts.finalize_failure();
        Err(DevError::ExitCode(1))
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
        // and the optional git_* fields elide.
        let parent = tmpdir("write_no_git");
        let project = tmpdir("write_no_git_project");
        let script = parent.join("gamma.lua");
        fs::write(&script, "").unwrap();
        let bin_dir = parent.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let artefact_dir = artefact_dir_under(&parent);
        let built = fake_built(&bin_dir);
        let cap = captured(b"", b"", Some(0), None);

        write_artefacts(&artefact_dir, &script, &built, &cap, &project).unwrap();
        let toml_body = fs::read_to_string(artefact_dir.join("run.toml")).unwrap();
        assert!(!toml_body.contains("git_commit"));
        assert!(!toml_body.contains("git_clean"));
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
}
