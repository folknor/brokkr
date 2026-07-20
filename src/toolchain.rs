//! Temporarily disabling a project's pinned Rust toolchain.
//!
//! A foreign checkout may pin a `rust-toolchain.toml` (or the legacy bare
//! `rust-toolchain`) that we don't have installed, or don't want rustup to
//! honour when brokkr drives cargo against it. When `disable_toolchain` is
//! set in `brokkr.toml`, [`DisabledToolchain::activate`] moves any such file
//! aside while brokkr holds the global lock (see *Serialisation* below) and
//! restores it on drop. brokkr picks *no* replacement - with the file gone,
//! rustup just does its normal fallback (a directory override, then the
//! default toolchain).
//!
//! ## Robustness
//!
//! The guard restores on `Drop`, which covers normal completion, an error
//! return, and the cooperative-interrupt path (measured runs unwind through
//! `DevError::Interrupted`). It does **not** cover a hard kill during a
//! non-tracked window (`brokkr check`, a bare `cargo build`), where SIGTERM
//! terminates brokkr with no unwinding - that mirrors how the rest of brokkr
//! treats a hard kill (scratch is left for `brokkr clean`). We self-heal that
//! case instead: [`activate`](DisabledToolchain::activate) adopts a leftover
//! `*.brokkr-disabled` sidecar from a prior aborted run and restores it on the
//! next drop, so the foreign repo returns to normal the next time brokkr runs
//! there.
//!
//! A `--commit` run builds in a persistent worktree rather than the live build
//! root, so [`with_worktree`](crate::context::with_worktree) re-[`arm`]s the
//! disable dir to the worktree path for the build closure when
//! `disable_toolchain` is set - otherwise the commit's own committed pin would
//! be honoured there.
//!
//! ## Serialisation
//!
//! Activation is driven by the global command lock, not the top of `run`. The
//! build root to disable is *armed* once ([`arm`], from `main` and
//! `with_worktree`); [`crate::lockfile::acquire`] then activates it - moving the
//! file aside - immediately after taking the flock, and the returned
//! `LockGuard`'s drop restores it just before releasing the flock. The
//! moved-aside window is thus exactly the locked window, so concurrent brokkr
//! invocations (which the lock already serialises) can never observe or race
//! the half-moved state. Commands that never take the lock never touch the file.
//!
//! The one exception is `brokkr fmt`: it runs `cargo fmt` (rustfmt is
//! toolchain-pinned) but takes no lock, so it constructs a [`DisabledToolchain`]
//! guard directly around the format run instead of riding a lock's activation.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::error::DevError;
use crate::output;

/// The build root whose pinned toolchain should be disabled when the global
/// lock is taken, or `None`. Armed once from `main` (the live build root) and
/// temporarily re-pointed at a worktree by `with_worktree`. Read by
/// [`activate_for_lock`], which the lockfile calls under the flock.
static DISABLE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Set the build root to disable at lock time, returning the previous value.
///
/// Called once at startup with the live build root (or `None` when
/// `disable_toolchain` is off / no project), and by `with_worktree` to scope
/// the dir to a `--commit` worktree for the duration of its build closure.
pub fn arm(dir: Option<PathBuf>) -> Option<PathBuf> {
    let mut slot = DISABLE_DIR.lock().unwrap_or_else(PoisonError::into_inner);
    std::mem::replace(&mut *slot, dir)
}

/// Activate the armed toolchain-disable, if any. Called by
/// [`crate::lockfile::acquire`] immediately after the flock is held, so the
/// moved-aside window coincides with the locked window. The returned guard is
/// stored in the `LockGuard` and restores the file just before the flock is
/// released.
pub fn activate_for_lock() -> Result<Option<DisabledToolchain>, DevError> {
    let dir = DISABLE_DIR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match dir {
        Some(dir) => Ok(Some(DisabledToolchain::activate(&dir)?)),
        None => Ok(None),
    }
}

/// The toolchain files rustup recognises, in the order we disable them.
const FILES: [&str; 2] = ["rust-toolchain.toml", "rust-toolchain"];

/// Suffix appended to a disabled toolchain file while it is moved aside.
const SUFFIX: &str = ".brokkr-disabled";

/// Guard that has moved a project's toolchain file(s) aside and restores them
/// when dropped. An empty guard (no files found) is a harmless no-op.
pub struct DisabledToolchain {
    /// `(moved-aside path, original path)` pairs to rename back on drop.
    moved: Vec<(PathBuf, PathBuf)>,
}

impl DisabledToolchain {
    /// Move any toolchain file in `dir` aside, returning a guard that restores
    /// it on drop. `dir` is the code tree where cargo runs (the build root).
    ///
    /// A leftover `*.brokkr-disabled` sidecar from a previously aborted run is
    /// adopted (tracked for restore) even when the original is already gone, so
    /// the next clean run heals it.
    pub fn activate(dir: &Path) -> Result<Self, DevError> {
        let mut moved = Vec::new();
        for name in FILES {
            let orig = dir.join(name);
            let aside = dir.join(format!("{name}{SUFFIX}"));
            if orig.exists() {
                // Both-exist case: a stale sidecar from a prior hard-killed run
                // sits next to a current original (rustup/the user recreated the
                // real file since). The stale sidecar no longer matches anything;
                // its original was superseded. Remove it (best-effort) before we
                // move the current `orig` into its place, so the CURRENT file
                // becomes the restore target rather than being silently clobbered.
                if aside.exists() {
                    std::fs::remove_file(&aside).ok();
                }
                std::fs::rename(&orig, &aside).map_err(|e| {
                    DevError::Config(format!(
                        "disable_toolchain: cannot move {} aside: {e}",
                        orig.display()
                    ))
                })?;
                output::build_msg(&format!("toolchain disabled: {name} moved aside"));
                moved.push((aside, orig));
            } else if aside.exists() {
                // Leftover from a hard-killed prior run: adopt so drop restores
                // it. The file stays disabled for this run (its point), then
                // returns to normal afterwards.
                moved.push((aside, orig));
            }
        }
        Ok(Self { moved })
    }
}

impl Drop for DisabledToolchain {
    fn drop(&mut self) {
        for (aside, orig) in &self.moved {
            if let Err(e) = std::fs::rename(aside, orig) {
                // Best-effort: warn rather than panic in a destructor. The next
                // `activate` will re-adopt the sidecar and try again.
                output::error(&format!(
                    "disable_toolchain: failed to restore {}: {e}",
                    orig.display()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{DisabledToolchain, SUFFIX};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A fresh, empty scratch dir under the crate's gitignored `target/`
    /// (project rules forbid `/tmp`).
    fn tmpdir(test_name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/toolchain")
            .join(test_name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn moves_aside_and_restores_on_drop() {
        let dir = tmpdir("roundtrip");
        let toml = dir.join("rust-toolchain.toml");
        fs::write(&toml, "[toolchain]\nchannel = \"nightly\"\n").unwrap();

        {
            let _guard = DisabledToolchain::activate(&dir).unwrap();
            // Disabled: original gone, sidecar present.
            assert!(!toml.exists());
            assert!(dir.join(format!("rust-toolchain.toml{SUFFIX}")).exists());
        }
        // Restored on drop, content intact.
        assert!(toml.exists());
        assert!(!dir.join(format!("rust-toolchain.toml{SUFFIX}")).exists());
        assert_eq!(
            fs::read_to_string(&toml).unwrap(),
            "[toolchain]\nchannel = \"nightly\"\n"
        );
    }

    #[test]
    fn no_file_is_a_noop() {
        let dir = tmpdir("noop");
        let guard = DisabledToolchain::activate(&dir).unwrap();
        assert!(guard.moved.is_empty());
        drop(guard);
        // Nothing created.
        assert!(!dir.join("rust-toolchain.toml").exists());
    }

    #[test]
    fn handles_both_file_names() {
        let dir = tmpdir("both_names");
        fs::write(dir.join("rust-toolchain.toml"), "a").unwrap();
        fs::write(dir.join("rust-toolchain"), "b").unwrap();
        {
            let _guard = DisabledToolchain::activate(&dir).unwrap();
            assert!(!dir.join("rust-toolchain.toml").exists());
            assert!(!dir.join("rust-toolchain").exists());
        }
        assert_eq!(fs::read_to_string(dir.join("rust-toolchain.toml")).unwrap(), "a");
        assert_eq!(fs::read_to_string(dir.join("rust-toolchain")).unwrap(), "b");
    }

    #[test]
    fn stale_sidecar_beside_current_original_is_not_clobbered() {
        let dir = tmpdir("both_present");
        // A current original plus a stale sidecar from a prior aborted run.
        let toml = dir.join("rust-toolchain.toml");
        let aside = dir.join(format!("rust-toolchain.toml{SUFFIX}"));
        fs::write(&toml, "current").unwrap();
        fs::write(&aside, "stale").unwrap();

        {
            let _guard = DisabledToolchain::activate(&dir).unwrap();
            // Disabled during the run: original moved aside.
            assert!(!toml.exists());
            assert!(aside.exists());
        }
        // Restored to the CURRENT file, no sidecar remnant.
        assert_eq!(fs::read_to_string(&toml).unwrap(), "current");
        assert!(!aside.exists());
    }

    #[test]
    fn adopts_leftover_sidecar_from_aborted_run() {
        let dir = tmpdir("leftover");
        // Simulate a prior hard-killed run: only the sidecar exists.
        let aside = dir.join(format!("rust-toolchain.toml{SUFFIX}"));
        fs::write(&aside, "pinned").unwrap();

        {
            let _guard = DisabledToolchain::activate(&dir).unwrap();
            // Still disabled during this run.
            assert!(!dir.join("rust-toolchain.toml").exists());
            assert!(aside.exists());
        }
        // Healed on drop: the real file is back, sidecar gone.
        assert!(dir.join("rust-toolchain.toml").exists());
        assert!(!aside.exists());
    }

    #[test]
    fn arm_drives_activation_and_restores_previous() {
        let dir = tmpdir("armed");
        let toml = dir.join("rust-toolchain.toml");
        fs::write(&toml, "pinned").unwrap();

        // Armed: activate_for_lock (what the lockfile calls under the flock)
        // moves the file aside; dropping the returned guard restores it.
        let saved = super::arm(Some(dir.clone()));
        {
            let guard = super::activate_for_lock().unwrap();
            assert!(guard.is_some());
            assert!(!toml.exists());
            assert!(dir.join(format!("rust-toolchain.toml{SUFFIX}")).exists());
        }
        assert!(toml.exists());

        // Not armed: activate_for_lock is a no-op.
        super::arm(None);
        assert!(super::activate_for_lock().unwrap().is_none());

        // Restore whatever the process global held before this test.
        super::arm(saved);
    }
}
