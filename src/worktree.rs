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

/// Filename prefix for the retroactive-benchmark worktrees brokkr creates as
/// siblings to the project root (`<parent>/.brokkr-worktree-<project>-<short>`).
/// Single source of truth for the naming convention - `create`/`list` build on
/// it and [`is_brokkr_worktree`] recognises it.
pub const WORKTREE_PREFIX: &str = ".brokkr-worktree-";

/// True when `path`'s final component names a brokkr-created worktree.
///
/// Used by [`crate::build::cargo_build_observed`] to decide whether to pin
/// `CARGO_TARGET_DIR` to a worktree-local path: only builds inside one of our
/// own retro-bench worktrees need that isolation, so ordinary main-tree builds
/// (which may legitimately share a target dir via host config) are untouched.
pub fn is_brokkr_worktree(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(WORKTREE_PREFIX))
}

/// A persistent git worktree checked out at a specific commit.
///
/// Created on demand by `Worktree::create` and reused on subsequent runs at
/// the same commit. Use `brokkr clean --worktrees` to garbage collect.
pub struct Worktree {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Short commit hash ([`crate::git::short_of`]).
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
    ///
    /// `before_cut` runs only when a worktree is about to be *cut* - never on
    /// reuse - and is handed the directory about to be (re)created. It is the
    /// retention hook ([`crate::worktree_record::enforce`]): eviction belongs to
    /// growth, and a reuse does not grow anything. Passing the cut directory
    /// lets eviction leave it out of the count and out of the victim list, so
    /// the worktree about to be replaced is never evicted as a bystander.
    ///
    /// The caller must hold the global lock: this removes and creates
    /// directories another brokkr may be building in.
    pub fn create(
        project_root: &Path,
        commit_ref: &str,
        before_cut: impl FnOnce(&Path),
    ) -> Result<Self, DevError> {
        // Validate the commit exists and resolve to a full hash for comparison.
        // The directory name is the fixed-width abbreviation, so the same
        // commit names the same worktree whatever width (or ref) it was asked
        // for by, and however large the repo has grown since.
        let crate::git::CommitId { full: full_hash, short } =
            crate::git::resolve_commit(project_root, commit_ref)?;
        let subject = run_git(project_root, &["log", "-1", "--format=%s", &full_hash])?;

        // Place worktree as a sibling so relative path deps still work.
        let (parent, prefix) = sibling_prefix(project_root)?;
        let worktree_dir = parent.join(format!("{prefix}{short}"));

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

        // Stale (different commit, git lost track of the dir, or HEAD could
        // not be read at all). A failed `rev-parse` is not proof of staleness -
        // it can be transient - so replacement goes through the same dirty
        // rule eviction obeys: `is_dirty` reads "git could not answer" as
        // dirty, and a dirty tree is refused rather than force-removed. The
        // cost of a wrong refusal is one rerun; the cost of a wrong removal is
        // somebody's uncommitted work.
        let stale = worktree_dir.exists();
        if stale && is_dirty(&worktree_dir) {
            return Err(DevError::Config(format!(
                "worktree at {} is not checked out at {short} and has uncommitted work \
                 (or git could not read it); refusing to replace it. Commit or move the \
                 work, remove the directory by hand, then rerun",
                worktree_dir.display()
            )));
        }

        before_cut(&worktree_dir);

        if stale {
            output::run_msg(&format!(
                "removing stale worktree at {}",
                worktree_dir.display()
            ));
            remove_one(project_root, &worktree_dir)?;
        }

        output::run_msg(&format!("creating worktree for {short} ({subject})"));
        output::run_msg(
            "  worktree is persistent - run `brokkr clean --worktrees` to remove",
        );

        let worktree_str = worktree_dir.display().to_string();
        run_git(
            project_root,
            &["worktree", "add", "--detach", &worktree_str, &full_hash],
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
/// `root` must be the **build** root - the same root [`Worktree::create`] was
/// given. Both the directory searched and the name prefix are derived from it,
/// so passing the project root when the two differ (config one level up) looks
/// in the wrong parent for the wrong prefix and silently finds nothing.
pub fn list(project_root: &Path) -> Result<Vec<PathBuf>, DevError> {
    let (parent, prefix) = sibling_prefix(project_root)?;

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
        if is_worktree_name(&prefix, name) {
            found.push(path);
        }
    }
    Ok(found)
}

/// The directory brokkr's worktrees for `root` live in, and the name prefix
/// they carry (`.brokkr-worktree-<root name>-`). One derivation, so `create`
/// (which names a worktree) and `list` (which finds them) cannot disagree.
fn sibling_prefix(root: &Path) -> Result<(&Path, String), DevError> {
    let parent = root
        .parent()
        .ok_or_else(|| DevError::Config("project root has no parent directory".into()))?;
    Ok((parent, name_prefix(root)))
}

/// The worktree-name prefix for `root`: `.brokkr-worktree-<root name>-`.
pub fn name_prefix(root: &Path) -> String {
    let project_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    format!("{WORKTREE_PREFIX}{project_name}-")
}

/// True when `name` is a worktree brokkr cut under `prefix` (from
/// [`name_prefix`]): the prefix followed by a bare short hash.
///
/// Matched by construction, not by prefix alone. A prefix test lets checkout
/// `foo` claim checkout `foo-bar`'s worktrees (`.brokkr-worktree-foo-bar-1a2b`
/// starts with `.brokkr-worktree-foo-`), and both live in the same parent under
/// the config-one-level-up layout - so eviction or a purge for one would
/// delete the other's. A short hash is hex with no hyphen, which the other
/// checkout's remainder (`bar-1a2b`) can never be.
pub fn is_worktree_name(prefix: &str, name: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Remove every persistent brokkr worktree sibling for the given project
/// (`<parent>/.brokkr-worktree-<project>-<short hash>`). Returns the number
/// of worktrees removed.
///
/// Unlike retention eviction this does **not** skip dirty worktrees: it is the
/// explicit hammer (`brokkr clean --worktrees`), asked for by name, and a purge
/// that quietly kept some would leave the user believing the disk was clear.
pub fn purge_all(project_root: &Path) -> Result<usize, DevError> {
    let paths = list(project_root)?;
    let mut removed = 0usize;
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(unnamed)");
        output::run_msg(&format!("removing worktree {name}"));
        remove_one(project_root, &path)?;
        removed += 1;
    }
    Ok(removed)
}

/// Remove one worktree: ask git, then fall back to a directory removal if git
/// left it behind, then prune git's bookkeeping.
///
/// The one removal sequence: shared by `purge_all`, by retention eviction
/// ([`crate::worktree_record::enforce`]) and by `create`'s stale-replacement
/// path. Callers decide *whether* to remove (eviction and `create` apply the
/// dirty rule first; the purge does not); this decides *how*.
///
/// The git failure is not an error on its own - "not a working tree" is the
/// expected answer for a directory git has lost track of, which is exactly
/// what the filesystem fallback is for - but it is carried into the error when
/// the fallback fails too, since it is usually the more telling of the two.
pub fn remove_one(git_root: &Path, path: &Path) -> Result<(), DevError> {
    let git = run_git(
        git_root,
        &["worktree", "remove", "--force", &path.display().to_string()],
    );
    if path.exists()
        && let Err(e) = std::fs::remove_dir_all(path)
    {
        let git_note = match &git {
            Err(g) => format!(" (git worktree remove also failed: {g})"),
            Ok(_) => String::new(),
        };
        return Err(DevError::Config(format!(
            "cannot remove worktree at {}: {e}{git_note}",
            path.display()
        )));
    }
    drop(run_git(git_root, &["worktree", "prune"]));
    Ok(())
}

/// True when the worktree has uncommitted or untracked content.
///
/// Deliberately does not exclude anything. `git::check_clean`'s exclusions
/// exist so brokkr's own outputs can't block a *measurement*; here the question
/// is whether deleting this directory would destroy work, and for that, an
/// untracked file counts.
pub fn is_dirty(path: &Path) -> bool {
    let Ok(out) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
    else {
        // Cannot tell: assume dirty. The failure mode of guessing wrong in the
        // other direction is deleting someone's work.
        return true;
    };
    !out.status.success() || !out.stdout.is_empty()
}

/// Run a git command in the given directory and return trimmed stdout.
fn run_git(cwd: &Path, args: &[&str]) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| DevError::Spawn { program: "git".into(), error })?;

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

#[cfg(test)]
mod tests {
    use super::{is_worktree_name, name_prefix};
    use std::path::Path;

    #[test]
    fn worktree_name_is_prefix_plus_short_hash() {
        let prefix = name_prefix(Path::new("/src/foo"));
        assert_eq!(prefix, ".brokkr-worktree-foo-");
        assert!(is_worktree_name(&prefix, ".brokkr-worktree-foo-1a2b3c4"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-foo-"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-bar-1a2b3c4"));
    }

    #[test]
    fn a_checkout_does_not_claim_a_hyphen_extended_siblings_worktrees() {
        // `foo` and `foo-bar` share a parent under the config-one-level-up
        // layout; a bare prefix test would let `foo` evict or purge these.
        let prefix = name_prefix(Path::new("/src/foo"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-foo-bar-1a2b3c4"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-foo-beef-1a2b3c4"));
    }
}
