//! Git worktree management for retroactive benchmarking.
//!
//! Creates a temporary worktree at a specific commit so we can build old code
//! while keeping data paths and the results DB in the main tree.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DevError;
use crate::output;

/// A temporary git worktree checked out at a specific commit.
pub struct Worktree {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Short commit hash (from rev-parse --short).
    pub commit: String,
    /// First line of the commit message.
    pub subject: String,
    /// Main tree root, needed for cleanup.
    project_root: PathBuf,
}

impl Worktree {
    /// Create a worktree at the given commit ref (hash, branch, tag, HEAD~N, etc.).
    ///
    /// The worktree is placed at `<project_root>/.brokkr/worktree/<short_hash>`.
    /// Returns an error if the commit doesn't exist or the worktree can't be created.
    pub fn create(project_root: &Path, commit_ref: &str) -> Result<Self, DevError> {
        // Validate the commit exists.
        let full_hash = run_git(project_root, &["rev-parse", "--verify", commit_ref])?;
        let _ = full_hash; // just validating — we use short hash below

        let short = run_git(project_root, &["rev-parse", "--short", commit_ref])?;
        let subject = run_git(project_root, &["log", "-1", "--format=%s", commit_ref])?;

        let worktree_dir = project_root
            .join(".brokkr")
            .join("worktree")
            .join(&short);

        // Clean up stale worktree at this path if it exists.
        if worktree_dir.exists() {
            output::run_msg(&format!("removing stale worktree at {}", worktree_dir.display()));
            drop(run_git(
                project_root,
                &["worktree", "remove", "--force", &worktree_dir.display().to_string()],
            ));
            // If git worktree remove failed, try manual cleanup.
            if worktree_dir.exists() {
                std::fs::remove_dir_all(&worktree_dir).map_err(|e| {
                    DevError::Config(format!(
                        "cannot remove stale worktree at {}: {e}",
                        worktree_dir.display()
                    ))
                })?;
                // Prune stale worktree bookkeeping.
                drop(run_git(project_root, &["worktree", "prune"]));
            }
        }

        output::run_msg(&format!("creating worktree for {short} ({subject})"));

        let worktree_str = worktree_dir.display().to_string();
        run_git(
            project_root,
            &["worktree", "add", "--detach", &worktree_str, commit_ref],
        )?;

        Ok(Self {
            path: worktree_dir,
            commit: short,
            subject,
            project_root: project_root.to_owned(),
        })
    }

    /// Remove the worktree and clean up git bookkeeping.
    pub fn remove(self) -> Result<(), DevError> {
        output::run_msg(&format!("removing worktree for {}", self.commit));
        let worktree_str = self.path.display().to_string();
        run_git(
            &self.project_root,
            &["worktree", "remove", "--force", &worktree_str],
        )?;
        Ok(())
    }
}

/// Run a git command in the given directory and return trimmed stdout.
fn run_git(cwd: &Path, args: &[&str]) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "git".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Subprocess {
            program: "git".into(),
            code: output.status.code(),
            stderr: stderr.trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
