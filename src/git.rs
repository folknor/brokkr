use std::path::Path;
use std::process::Command;

use crate::error::DevError;

/// Width, in hex digits, of every abbreviated commit hash brokkr stores or
/// names something by: results rows, `--commit` worktree names, bench
/// baselines, tilegen archives. Matches the crate's own `build.rs` stamp.
///
/// Fixed rather than `git rev-parse --short`'s default, which grows with the
/// repo (7, then 8, ...): the same commit got a new worktree/baseline/archive
/// name once the repo crossed a threshold, orphaning the old one, and a hash
/// copied at one width failed to find artefacts named at another. Truncation
/// of the full hash, not `--short=9`, because `--short=N` still lengthens an
/// ambiguous abbreviation - the name must be a pure function of the commit.
pub const SHORT_HASH_LEN: usize = 9;

/// `full`'s [`SHORT_HASH_LEN`]-digit abbreviation - the one way brokkr
/// shortens a hash. A string shorter than that (already abbreviated, or not a
/// hash at all) comes back unchanged.
pub fn short_of(full: &str) -> String {
    full.get(..SHORT_HASH_LEN).unwrap_or(full).to_owned()
}

/// A commit resolved from a user-supplied revision (hash of any width, branch,
/// tag, `HEAD~N`).
pub struct CommitId {
    /// The full 40-digit hash.
    pub full: String,
    /// [`short_of`] the full hash.
    pub short: String,
}

/// Resolve `rev` to the commit it names, peeling tags. The single entry point
/// for turning a user-typed revision into brokkr's commit identity, so a hash
/// pasted at any width names the same worktree, baseline and archive.
pub fn resolve_commit(repo: &Path, rev: &str) -> Result<CommitId, DevError> {
    let spec = format!("{rev}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &spec])
        .current_dir(repo)
        .output()
        .map_err(|error| DevError::Spawn { program: "git".to_owned(), error })?;
    if !output.status.success() {
        return Err(DevError::Config(format!("{rev:?} does not name a commit in {}", repo.display())));
    }
    let full = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let short = short_of(&full);
    Ok(CommitId { full, short })
}

/// Structured git state for the benchmark harness.
pub struct GitInfo {
    /// `HEAD`'s hash, abbreviated by [`short_of`].
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
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace_root)
        .output()
        .map_err(|error| DevError::Spawn { program: "git".to_owned(), error })?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".to_owned(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(short_of(String::from_utf8_lossy(&output.stdout).trim()))
}

fn read_commit_subject(workspace_root: &Path) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(workspace_root)
        .output()
        .map_err(|error| DevError::Spawn { program: "git".to_owned(), error })?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".to_owned(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Pathspecs excluding whatever toolchain file brokkr *itself* has currently
/// moved aside, and the sidecar it moved it to.
///
/// The lock activates the toolchain-disable (see [`crate::toolchain`]), which
/// renames `rust-toolchain.toml` to a `.brokkr-disabled` sidecar. That is a
/// tracked-file deletion plus an untracked file - a dirty tree by any ordinary
/// reading. Since the harness collects git state *after* taking the lock, a
/// `disable_toolchain` project would refuse every measured run against a
/// spotless checkout, and `--force` would silently decline to store the row.
///
/// The exclusion is conditional on the sidecar actually being present, which is
/// what keeps it honest: brokkr only hides a file it can see it moved. A user's
/// own edit to `rust-toolchain.toml` leaves no sidecar, still marks the tree
/// dirty, and still blocks the run - correctly, because changing the toolchain
/// absolutely does change what the built binary does. That is the difference
/// between this and the unconditional exclusions in [`check_clean`], all of
/// which are for files judged unable to affect what a measured run measures.
fn toolchain_exclusions(workspace_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for name in crate::toolchain::FILES {
        if workspace_root
            .join(format!("{name}{}", crate::toolchain::SUFFIX))
            .exists()
        {
            out.push(format!(":(exclude){name}"));
            out.push(format!(":(exclude){name}{}", crate::toolchain::SUFFIX));
        }
    }
    out
}

fn check_clean(workspace_root: &Path) -> bool {
    // Exclude `.brokkr/` (brokkr's own measurement stores - results.db,
    // sidecar.db, ratatoskr's gate.db, piners' runs.db), *.md (docs), and
    // sluggrs' approved.png baselines, so they don't mark a measured run dirty.
    //
    // *.md is a judgement, not a certainty: a crate that `include_str!`s its
    // docs (brokkr's own `man`, any `#![doc = include_str!(..)]` crate) does
    // compile the markdown into the binary, so a docs edit changes its bytes.
    // It cannot change what a measured run measures, though - the doc text is
    // inert data on every path a bench exercises - and what the clean-tree
    // demand protects is that the pinned commit describes the measured
    // behaviour. `check`'s prose-only shortcut is a different question (can
    // this edit break the build or a test?) and does count included markdown;
    // see `scope::dirt`.
    //
    // brokkr.toml is NOT excluded from the tracked-file diffs: it carries host
    // build features, `env` and `capture_env`, all of which change what a
    // measured run builds or sees, so an uncommitted edit leaves the pinned
    // commit describing a different run. The price is that a registration
    // brokkr writes into a tracked brokkr.toml (`--as-snapshot`, a download)
    // marks the tree dirty until committed. An *untracked* brokkr.toml stays
    // excluded from the untracked listing: a project that keeps it out of git
    // has chosen not to pin it, and counting it would make every run dirty.
    //
    // approved.png is here because `brokkr approve` was otherwise
    // self-blocking: it demands a clean tree, then writes into the tree, so the
    // first approval succeeded and every later one failed until you committed.
    // Approving N snapshots took N commits. The clean-tree demand exists so the
    // commit an approval is pinned to actually describes what rendered the
    // image; approved.png is that operation's *output*, so it cannot invalidate
    // the pin.
    //
    // `.brokkr/` is excluded as a directory rather than as `results.db` alone
    // for the same reason, and it took the same bug to find out: every gated
    // `sync --bench` writes a row to a tracked gate.db, so once one gated run had
    // happened, the next `--as-baseline` refused - and recording a baseline is
    // precisely the operation you cannot work around with `--force`, since a
    // dirty baseline is the thing the gate warns about forever after. Every
    // store under `.brokkr/` is an output of the run being measured, so none of
    // them can invalidate the commit that run is pinned to. This matches the
    // untracked check below, which has always excluded the whole directory.
    const EXCLUDES: [&str; 3] = [
        ":(exclude).brokkr/",
        ":(exclude)*.md",
        ":(exclude)snapshots/*/approved.png",
    ];
    let mut excludes: Vec<String> = EXCLUDES.iter().map(|s| (*s).to_owned()).collect();
    excludes.extend(toolchain_exclusions(workspace_root));
    let mut untracked_excludes = excludes.clone();
    untracked_excludes.push(":(exclude)brokkr.toml".to_owned());

    let run = |args: &[&str], excludes: &[String]| {
        let mut cmd = Command::new("git");
        cmd.args(args);
        cmd.arg("--");
        cmd.args(excludes);
        cmd.current_dir(workspace_root);
        cmd.output()
    };

    let unstaged = run(&["diff", "--quiet", "HEAD"], &excludes);
    let staged = run(&["diff", "--quiet", "--cached", "HEAD"], &excludes);
    let untracked = run(&["ls-files", "--others", "--exclude-standard"], &untracked_excludes);

    let unstaged_ok = unstaged.as_ref().ok().is_some_and(|o| o.status.success());

    let staged_ok = staged.as_ref().ok().is_some_and(|o| o.status.success());

    let no_untracked = untracked
        .as_ref()
        .ok()
        .is_some_and(|o| o.status.success() && o.stdout.is_empty());

    unstaged_ok && staged_ok && no_untracked
}
