//! Git worktree management for retroactive benchmarking.
//!
//! Creates a persistent worktree at a specific commit so we can build old
//! code while keeping data paths and the results DB in the main tree.
//! Worktrees are reused across runs (the cargo `target/` inside survives)
//! and garbage-collected via `brokkr clean --worktrees`.
//!
//! The worktree is placed as a sibling to the project root (not inside it)
//! so that relative path dependencies (e.g. `../pbfhogg`) still resolve.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DevError;
use crate::output;

/// A persistent git worktree checked out at a specific commit.
///
/// Created on demand by `Worktree::create` and reused on subsequent runs at
/// the same commit. Use `brokkr clean --worktrees` to garbage collect.
pub struct Worktree {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Short commit hash (from rev-parse --short).
    pub commit: String,
    /// First line of the commit message.
    pub subject: String,
}

impl Worktree {
    /// Create a worktree at the given commit ref (hash, branch, tag, HEAD~N, etc.).
    ///
    /// The worktree is placed at `<parent>/.brokkr-worktree-<project>-<short_hash>`
    /// as a sibling to the project root, so that relative path dependencies
    /// (e.g. `../pbfhogg`) resolve correctly.
    ///
    /// Worktrees are persistent across runs: if one already exists at the
    /// computed path and its HEAD matches the requested commit, it is reused.
    /// This preserves the cargo `target/` inside, so subsequent
    /// `--bench`/`--hotpath`/`--alloc` runs at the same commit don't pay the
    /// full rebuild cost. Use `brokkr clean --worktrees` to garbage collect.
    pub fn create(project_root: &Path, commit_ref: &str) -> Result<Self, DevError> {
        // Validate the commit exists and resolve to a full hash for comparison.
        let full_hash = run_git(project_root, &["rev-parse", "--verify", commit_ref])?;

        let short = run_git(project_root, &["rev-parse", "--short", commit_ref])?;
        let subject = run_git(project_root, &["log", "-1", "--format=%s", commit_ref])?;

        // Place worktree as a sibling so relative path deps still work.
        let parent = project_root
            .parent()
            .ok_or_else(|| DevError::Config("project root has no parent directory".into()))?;
        let project_name = project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project");
        let worktree_dir = parent.join(format!(".brokkr-worktree-{project_name}-{short}"));

        // Reuse path: if a worktree already exists at this path and its HEAD
        // matches the requested commit, skip remove + re-add.
        if worktree_dir.exists()
            && let Ok(head) = run_git(&worktree_dir, &["rev-parse", "HEAD"])
            && head == full_hash
        {
            output::run_msg(&format!("reusing worktree for {short} ({subject})"));
            return Ok(Self {
                path: worktree_dir,
                commit: short,
                subject,
            });
        }

        // Stale (different commit, or git lost track of the dir): force-remove.
        if worktree_dir.exists() {
            output::run_msg(&format!(
                "removing stale worktree at {}",
                worktree_dir.display()
            ));
            drop(run_git(
                project_root,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &worktree_dir.display().to_string(),
                ],
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
        output::run_msg(
            "  worktree is persistent - run `brokkr clean --worktrees` to remove",
        );

        let worktree_str = worktree_dir.display().to_string();
        run_git(
            project_root,
            &["worktree", "add", "--detach", &worktree_str, commit_ref],
        )?;

        Ok(Self {
            path: worktree_dir,
            commit: short,
            subject,
        })
    }
}

/// Collect every persistent brokkr worktree sibling for the given project
/// (matching `<parent>/.brokkr-worktree-<project>-*`) without touching them.
pub fn list(project_root: &Path) -> Result<Vec<PathBuf>, DevError> {
    let parent = project_root
        .parent()
        .ok_or_else(|| DevError::Config("project root has no parent directory".into()))?;
    let project_name = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let prefix = format!(".brokkr-worktree-{project_name}-");

    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(&prefix) {
            found.push(path);
        }
    }
    Ok(found)
}

/// Remove every persistent brokkr worktree sibling for the given project
/// (matching `<parent>/.brokkr-worktree-<project>-*`) and prune git
/// bookkeeping. Returns the number of worktrees removed.
pub fn purge_all(project_root: &Path) -> Result<usize, DevError> {
    let paths = list(project_root)?;
    let mut removed = 0usize;
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(unnamed)");
        output::run_msg(&format!("removing worktree {name}"));
        drop(run_git(
            project_root,
            &["worktree", "remove", "--force", &path.display().to_string()],
        ));
        if path.exists() {
            std::fs::remove_dir_all(&path).map_err(|e| {
                DevError::Config(format!(
                    "cannot remove worktree at {}: {e}",
                    path.display()
                ))
            })?;
        }
        removed += 1;
    }

    if removed > 0 {
        drop(run_git(project_root, &["worktree", "prune"]));
    }
    Ok(removed)
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
