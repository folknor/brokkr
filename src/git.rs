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
    // Exclude .brokkr/results.db (modified by benchmarks), *.md (docs),
    // brokkr.toml (host-local dataset/snapshot registrations, e.g. mutated by
    // `--as-snapshot`), and sluggrs' approved.png baselines - none of these
    // change what the built binary does, so they shouldn't mark a measured run
    // dirty.
    //
    // approved.png is here because `brokkr approve` was otherwise
    // self-blocking: it demands a clean tree, then writes into the tree, so the
    // first approval succeeded and every later one failed until you committed.
    // Approving N snapshots took N commits. The clean-tree demand exists so the
    // commit an approval is pinned to actually describes what rendered the
    // image; approved.png is that operation's *output*, so it cannot invalidate
    // the pin.
    let unstaged = Command::new("git")
        .args([
            "diff",
            "--quiet",
            "HEAD",
            "--",
            ":(exclude).brokkr/results.db",
            ":(exclude)*.md",
            ":(exclude)brokkr.toml",
            ":(exclude)snapshots/*/approved.png",
        ])
        .current_dir(workspace_root)
        .output();

    let staged = Command::new("git")
        .args([
            "diff",
            "--quiet",
            "--cached",
            "HEAD",
            "--",
            ":(exclude).brokkr/results.db",
            ":(exclude)*.md",
            ":(exclude)brokkr.toml",
            ":(exclude)snapshots/*/approved.png",
        ])
        .current_dir(workspace_root)
        .output();

    let untracked = Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            ":(exclude).brokkr/",
            ":(exclude)*.md",
            ":(exclude)brokkr.toml",
            ":(exclude)snapshots/*/approved.png",
        ])
        .current_dir(workspace_root)
        .output();

    let unstaged_ok = unstaged.as_ref().ok().is_some_and(|o| o.status.success());

    let staged_ok = staged.as_ref().ok().is_some_and(|o| o.status.success());

    let no_untracked = untracked
        .as_ref()
        .ok()
        .is_some_and(|o| o.status.success() && o.stdout.is_empty());

    unstaged_ok && staged_ok && no_untracked
}
