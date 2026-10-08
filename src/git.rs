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

/// Whether `repo` has any uncommitted change - staged, unstaged or untracked
/// (gitignored files excluded) - outside `.brokkr/`. Unlike the measured-run
/// clean check, nothing else is exempt: markdown and `brokkr.toml` count. A
/// run record that says "clean" must mean `HEAD` describes the whole
/// invocation. Callers ask under the lock, which may have moved a toolchain
/// file aside (see [`toolchain_exclusions`]): git sees that move as dirt, so
/// it is excluded from the status and the moved bytes are compared with
/// `HEAD`'s instead - a user's own edit to the file still counts, whoever
/// moved it. `None` when git cannot answer (not a repo, git failed).
pub fn has_uncommitted(repo: &Path) -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--", ".", ":(exclude).brokkr"])
        .args(toolchain_exclusions(repo))
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    if !output.stdout.is_empty() {
        return Some(true);
    }
    for name in crate::toolchain::FILES {
        let sidecar = repo.join(format!("{name}{}", crate::toolchain::SUFFIX));
        if !sidecar.exists() {
            continue;
        }
        // Unreadable moved bytes are unknown, never clean.
        let moved = std::fs::read(&sidecar).ok()?;
        // The status above excluded the path, so a staged change to it is
        // checked here: the index against HEAD.
        let staged = Command::new("git")
            .args(["diff", "--cached", "--quiet", "HEAD", "--", name])
            .current_dir(repo)
            .status()
            .ok()?;
        match staged.code() {
            Some(0) => {}
            Some(1) => return Some(true),
            _ => return None,
        }
        // HEAD's entry: `<mode> blob <sha>\t<name>`, or nothing when the file
        // is not in HEAD (it was untracked - uncommitted either way). A git
        // failure is unknown.
        let tree = Command::new("git")
            .args(["ls-tree", "HEAD", "--", name])
            .current_dir(repo)
            .output()
            .ok()?;
        if !tree.status.success() {
            return None;
        }
        let entry = String::from_utf8_lossy(&tree.stdout);
        let Some(mode) = entry.split_whitespace().next() else {
            return Some(true);
        };
        // The mode git would record for the moved file: a mode-only change
        // is a change.
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&sidecar).ok()?.permissions().mode() & 0o111 != 0
        };
        if (mode == "100755") != executable {
            return Some(true);
        }
        let at_head = Command::new("git")
            .args(["show", &format!("HEAD:./{name}")])
            .current_dir(repo)
            .output()
            .ok()?;
        if !at_head.status.success() {
            return None;
        }
        if at_head.stdout != moved {
            return Some(true);
        }
    }
    Some(false)
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

/// Pathspecs excluding the files the governing `brokkr.toml` `include`s, for
/// the same decision that excludes `brokkr.toml` itself (see [`check_clean`]):
/// an included file is part of that config, and brokkr writes dataset
/// registrations into whichever file holds the dataset.
///
/// Only files inside `workspace_root` are named - git refuses a pathspec
/// outside the repository, and a file outside the tree cannot dirty it. A
/// config that does not compose adds nothing: the command that loaded it has
/// already failed, or is not a brokkr project at all.
fn include_exclusions(workspace_root: &Path) -> Vec<String> {
    let Some(dir) = crate::project::find_config_dir(workspace_root) else {
        return Vec::new();
    };
    let Ok((_, sources)) = crate::config::compose(&dir.join("brokkr.toml")) else {
        return Vec::new();
    };
    let Ok(root) = std::fs::canonicalize(workspace_root) else {
        return Vec::new();
    };
    sources
        .included()
        .iter()
        .filter_map(|file| file.strip_prefix(&root).ok())
        .map(|rel| format!(":(exclude,literal){}", rel.display()))
        .collect()
}

fn check_clean(workspace_root: &Path) -> bool {
    // Exclude `.brokkr/` (brokkr's own measurement stores - results.db,
    // sidecar.db, ratatoskr's gate.db, piners' runs.db), *.md (docs),
    // brokkr.toml and sluggrs' approved.png baselines, so they don't mark a
    // measured run dirty.
    //
    // *.md is a judgement, not a certainty: a crate that `include_str!`s its
    // docs (brokkr's own `man`, any `#![doc = include_str!(..)]` crate) does
    // compile the markdown into the binary, so a docs edit changes its bytes.
    // It cannot change what a measured run measures, though - the doc text is
    // inert data on every path a bench exercises - and what the clean-tree
    // demand protects is that the pinned commit describes the measured
    // behaviour. Markdown is never dirt - here or in `check`'s prose-only
    // shortcut (`scope::dirt`) - and that is deliberate, not a bug.
    //
    // brokkr.toml is excluded by decision, not because it is inert: it carries
    // host build features, `env` and `capture_env`. But brokkr itself writes
    // registrations into it (`--as-snapshot`, downloads), and counting it
    // would block measured runs after every such write until committed. The
    // run's features and captured env are recorded on the row itself, so the
    // row stays interpretable without the pin covering brokkr.toml. The files
    // it `include`s are excluded with it (`include_exclusions`).
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
    const EXCLUDES: [&str; 4] = [
        ":(exclude).brokkr/",
        ":(exclude)*.md",
        ":(exclude)brokkr.toml",
        ":(exclude)snapshots/*/approved.png",
    ];
    let mut excludes: Vec<String> = EXCLUDES.iter().map(|s| (*s).to_owned()).collect();
    excludes.extend(toolchain_exclusions(workspace_root));
    excludes.extend(include_exclusions(workspace_root));

    let run = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.args(args);
        cmd.arg("--");
        cmd.args(&excludes);
        cmd.current_dir(workspace_root);
        cmd.output()
    };

    let unstaged = run(&["diff", "--quiet", "HEAD"]);
    let staged = run(&["diff", "--quiet", "--cached", "HEAD"]);
    let untracked = run(&["ls-files", "--others", "--exclude-standard"]);

    let unstaged_ok = unstaged.as_ref().ok().is_some_and(|o| o.status.success());

    let staged_ok = staged.as_ref().ok().is_some_and(|o| o.status.success());

    let no_untracked = untracked
        .as_ref()
        .ok()
        .is_some_and(|o| o.status.success() && o.stdout.is_empty());

    unstaged_ok && staged_ok && no_untracked
}
