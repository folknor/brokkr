use std::path::Path;
use std::process::Command;

use crate::error::DevError;

/// Structured git state for the benchmark harness.
pub struct GitInfo {
    /// Short hash from `git rev-parse --short HEAD`.
    pub commit: String,
    /// First line of the commit message.
    pub subject: String,
    /// True when the working tree has no staged or unstaged changes.
    pub is_clean: bool,
}

/// Collect git information from the working directory.
pub fn collect(workspace_root: &Path) -> Result<GitInfo, DevError> {
    let commit = read_commit_hash(workspace_root)?;
    let subject = read_commit_subject(workspace_root)?;
    let is_clean = check_clean(workspace_root);

    Ok(GitInfo {
        commit,
        subject,
        is_clean,
    })
}

fn read_commit_hash(workspace_root: &Path) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(workspace_root)
        .output()
        .map_err(DevError::Io)?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".to_owned(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn read_commit_subject(workspace_root: &Path) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(workspace_root)
        .output()
        .map_err(DevError::Io)?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".to_owned(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn check_clean(workspace_root: &Path) -> bool {
    // Exclude .brokkr/results.db — it's tracked in git but modified by benchmarks.
    let unstaged = Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--", ":(exclude).brokkr/results.db"])
        .current_dir(workspace_root)
        .output();

    let staged = Command::new("git")
        .args(["diff", "--quiet", "--cached", "HEAD", "--", ":(exclude).brokkr/results.db"])
        .current_dir(workspace_root)
        .output();

    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(workspace_root)
        .output();

    let unstaged_ok = unstaged
        .as_ref()
        .ok()
        .is_some_and(|o| o.status.success());

    let staged_ok = staged
        .as_ref()
        .ok()
        .is_some_and(|o| o.status.success());

    let no_untracked = untracked
        .as_ref()
        .ok()
        .is_some_and(|o| o.status.success() && o.stdout.is_empty());

    unstaged_ok && staged_ok && no_untracked
}
