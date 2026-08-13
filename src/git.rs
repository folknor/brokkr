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
/// between this and the unconditional exclusions above, all of which are for
/// files that cannot affect the binary at all.
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
    // sidecar.db, ratatoskr's gate.db, piners' runs.db), *.md (docs),
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
