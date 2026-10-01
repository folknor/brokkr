//! The free-space gate: a fresh hold on the brokkr lock is refused when a
//! filesystem the build will write to has less than [`MIN_FREE_PERCENT`]
//! of its size available - a fast `df`, not a size walk.
//!
//! Only the build's filesystem is checked, since that is where the bulk
//! lands. The project root's is not: `.brokkr/` and the results stores are
//! small, and a root on a full disk with `target` symlinked onto a roomy one
//! must not be refused for a build that writes nothing to the root's disk.
//! The target is probed as `<project_root>/target` with symlinks resolved,
//! which is what makes one rule cover every host: a plain per-project dir
//! (or a `target` not created yet, which resolves to its nearest existing
//! ancestor) reports the project's disk, a `target` symlinked into a shared
//! directory reports the shared one. A `CARGO_TARGET_DIR` in the environment
//! is probed too, deduplicated by mount. A target moved by `build.target-dir`
//! in a cargo config is not - finding it takes `cargo metadata`, too slow for
//! every lock acquisition.
//!
//! "Available" is `statvfs`' `f_bavail`, df's `Avail` column: the space an
//! unprivileged process can still write, so an ext4 root reserve does not
//! count as headroom. A filesystem whose size cannot be read fails open.
//!
//! Cutting a `--commit` worktree gates one more disk, the worktree container's
//! (`~/.brokkr/worktrees`), through [`check_paths`]: the checkout is written
//! there, which need not be the target's disk. Only at the cut - the per-hold
//! check would otherwise refuse every command on a full home disk, including
//! builds that write nothing to it.
//!
//! `clean` is exempt, since it is the remedy, and so is `env`, which reports
//! free space and is how you look: its storage rows show each disk's free
//! share and flag one under the floor by this module's rule.

use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::lockfile::LockContext;

/// Refuse below this share of a filesystem's size available.
pub(crate) const MIN_FREE_PERCENT: u64 = 5;

/// Commands the gate never refuses.
const EXEMPT: [&str; 2] = ["clean", "env"];

/// The host gate, run on every fresh lock hold.
pub(crate) fn check(ctx: &LockContext<'_>) -> Result<(), DevError> {
    if EXEMPT.contains(&ctx.command) {
        return Ok(());
    }
    let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR").filter(|d| !d.is_empty());
    check_paths(&candidates(Path::new(ctx.project_root), cargo_target_dir.as_deref()))
}

/// The gate over an explicit list of `(label, path)`s, deduplicated by mount.
/// Also called when a `--commit` worktree is cut, for the container the
/// checkout is written to ([`crate::worktree::Worktree::create`]) - a disk the
/// per-hold check does not cover, since it is not the target's.
pub(crate) fn check_paths(paths: &[(&str, PathBuf)]) -> Result<(), DevError> {
    let mut seen: Vec<String> = Vec::new();
    let mut low: Vec<String> = Vec::new();
    for (label, path) in paths {
        let Some(fs) = crate::env::probe_path(label, path) else {
            continue;
        };
        if seen.contains(&fs.mount_point) {
            continue;
        }
        seen.push(fs.mount_point.clone());
        if is_low(fs.free_bytes, fs.total_bytes) {
            low.push(format!(
                "{label} ({}) is on {}, which has {} available of {} ({:.1}%)",
                path.display(),
                fs.mount_point,
                crate::env::format_bytes(fs.free_bytes),
                crate::env::format_bytes(fs.total_bytes),
                percent(fs.free_bytes, fs.total_bytes),
            ));
        }
    }
    if low.is_empty() {
        return Ok(());
    }
    low.push(format!(
        "refusing to run with less than {MIN_FREE_PERCENT}% free. Free space first: \
         `brokkr clean` (scratch), `brokkr clean --cargo` (this package's build \
         artifacts), `brokkr clean --worktrees` (benchmark worktrees, each with its own \
         target/)"
    ));
    Err(DevError::Preflight(low))
}

/// The paths whose filesystems the gate probes, in report order.
fn candidates(root: &Path, cargo_target_dir: Option<&std::ffi::OsStr>) -> Vec<(&'static str, PathBuf)> {
    let mut out = vec![("target", root.join("target"))];
    if let Some(dir) = cargo_target_dir {
        out.push(("CARGO_TARGET_DIR", root.join(dir)));
    }
    out
}

/// True when `free` is below [`MIN_FREE_PERCENT`] of `total`. A zero total
/// means the size could not be read, which fails open. Shared with `brokkr
/// env`, which flags a storage row by the same rule.
pub(crate) fn is_low(free: u64, total: u64) -> bool {
    total > 0 && u128::from(free) * 100 < u128::from(total) * u128::from(MIN_FREE_PERCENT)
}

/// `free` as a percentage of a nonzero `total`, for display.
#[allow(clippy::cast_precision_loss)] // a display percentage
pub(crate) fn percent(free: u64, total: u64) -> f64 {
    free as f64 * 100.0 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_strictly_below_five_percent() {
        assert!(is_low(4, 100));
        assert!(!is_low(5, 100));
        assert!(!is_low(50, 100));
        assert!(is_low(0, u64::MAX)); // no overflow at the top of the range
        assert!(!is_low(0, 0)); // unreadable size fails open
    }

    #[test]
    fn candidates_cover_target_and_env_override_but_not_the_root() {
        let root = Path::new("/p");
        let plain = candidates(root, None);
        assert_eq!(plain, vec![("target", PathBuf::from("/p/target"))]);
        // A relative CARGO_TARGET_DIR resolves against the project; an
        // absolute one replaces it (Path::join semantics).
        let rel = candidates(root, Some(std::ffi::OsStr::new("t")));
        assert_eq!(rel[1], ("CARGO_TARGET_DIR", PathBuf::from("/p/t")));
        let abs = candidates(root, Some(std::ffi::OsStr::new("/shared/cargo")));
        assert_eq!(abs[1], ("CARGO_TARGET_DIR", PathBuf::from("/shared/cargo")));
    }

    #[test]
    fn exempt_commands_skip_the_probe() {
        for command in EXEMPT {
            let ctx = LockContext {
                project: "p",
                command,
                project_root: "/definitely/not/a/real/root",
            };
            assert!(check(&ctx).is_ok());
        }
    }
}
