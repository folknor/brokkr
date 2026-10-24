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
//! return, the cooperative-interrupt path (runs unwind through
//! `DevError::Interrupted`) and a `check`/`test`/`clippy` watchdog kill, which
//! unwinds the same way - and whose exit backstop calls
//! [`crate::tree_policy::restore_active_hold`]. It does **not** cover a hard kill
//! (`brokkr kill --hard`, SIGKILL) or a signal during an unguarded window (a
//! bare `cargo build`), which terminates brokkr with no unwinding - that
//! mirrors how the rest of brokkr
//! treats a hard kill (scratch is left for `brokkr clean`). We self-heal that
//! case instead: [`activate`](DisabledToolchain::activate) adopts a leftover
//! `*.brokkr-disabled` sidecar from a prior aborted run and restores it on the
//! next drop, so the foreign repo returns to normal the next time brokkr runs
//! there.
//!
//! A `--commit` run builds in a persistent worktree rather than the live build
//! root, so [`with_worktree`](crate::context::with_worktree), which holds the
//! lock across the whole `--commit` run, activates the tree policy on the
//! worktree path itself for the build closure - otherwise the commit's own
//! committed pin would be honoured there. (The worktree path is attached to
//! the active hold for its own lifetime.)
//!
//! ## Serialisation
//!
//! Activation is driven by the global command lock, through
//! [`crate::tree_policy`], which also owns the cargo config strip sharing
//! this lifecycle. The lock context carries the configured build-root path
//! and policy into acquisition. After
//! taking the flock, checking the disk gate and draining compiler leases,
//! acquisition moves the file aside before publishing the hold capability.
//! The returned `LockGuard` restores it just before releasing the flock, so
//! concurrent brokkr invocations cannot observe or race the half-moved state.
//!
//! Every command that runs cargo/rustup against a pinned tree therefore takes
//! the lock so this activation fires - including the ones that aren't builds in
//! the timing sense: `deps` (`cargo metadata` + `ccu`), `compare-tiles`
//! (`cargo metadata`), and `fmt` (rustfmt is toolchain-pinned). `fmt` used to
//! construct a [`DisabledToolchain`] guard directly, but a bare guard races a
//! concurrent locked build that already moved the same file aside - it would
//! adopt that build's sidecar and restore it mid-command - so it now rides the
//! lock's activation like the rest. `fmt` only takes the lock when
//! `disable_toolchain` is set; with nothing to move it stays lock-free.

use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::output;

/// Put back toolchain pins held aside in `dirs`, for a process about to exit
/// without unwinding - the `check` watchdog's backstop, through
/// [`crate::tree_policy::restore_active_hold`], whose main thread (and so the
/// guard whose drop normally restores) is stuck. Only a sidecar whose original
/// is absent is renamed back, as the guard's drop would do.
pub fn restore_dirs(dirs: &[PathBuf]) {
    for dir in dirs {
        restore_dir(dir);
    }
}

fn restore_dir(dir: &Path) {
    for (name, sidecar) in moved_paths() {
        let orig = dir.join(name);
        let aside = dir.join(sidecar);
        let should_restore = match path_exists(&aside).and_then(|exists| {
            if exists {
                path_exists(&orig).map(|original_exists| !original_exists)
            } else {
                Ok(false)
            }
        }) {
            Ok(should_restore) => should_restore,
            Err(error) => {
                output::error_forced(&format!("disable_toolchain: {error}"));
                false
            }
        };
        if should_restore && let Err(e) = std::fs::rename(&aside, &orig) {
            output::error_forced(&format!(
                "disable_toolchain: failed to restore {}: {e}",
                orig.display()
            ));
        }
    }
}

fn path_exists(path: &Path) -> Result<bool, DevError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DevError::Io(std::io::Error::new(
            error.kind(),
            format!("cannot inspect {}: {error}", path.display()),
        ))),
    }
}

/// The toolchain files rustup recognises, in the order we disable them.
pub const FILES: [&str; 2] = ["rust-toolchain.toml", "rust-toolchain"];

/// Suffix appended to a disabled toolchain file while it is moved aside.
pub const SUFFIX: &str = ".brokkr-disabled";

/// Relative pin and sidecar paths, shared by moves, restoration, and git's
/// cleanliness policies. Consumers do not construct sidecar names themselves.
pub(crate) fn moved_paths() -> impl Iterator<Item = (&'static str, PathBuf)> {
    FILES
        .into_iter()
        .map(|name| (name, PathBuf::from(format!("{name}{SUFFIX}"))))
}

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
        // The guard exists before the first move, so an error on a later
        // file drops it and puts back what was already moved.
        let mut guard = Self { moved: Vec::new() };
        let moved = &mut guard.moved;
        for (name, sidecar) in moved_paths() {
            let orig = dir.join(name);
            let aside = dir.join(sidecar);
            if path_exists(&orig)? {
                // Both-exist case: a stale sidecar from a prior hard-killed run
                // sits next to a current original (rustup/the user recreated the
                // real file since). The stale sidecar no longer matches anything;
                // its original was superseded. Remove it before we move the
                // current file into its place, so that file becomes the restore
                // target rather than being silently clobbered.
                if path_exists(&aside)? {
                    match std::fs::remove_file(&aside) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(DevError::Io(std::io::Error::new(
                                error.kind(),
                                format!(
                                    "disable_toolchain: cannot remove stale {}: {error}",
                                    aside.display()
                                ),
                            )));
                        }
                    }
                }
                // This same-directory rename hides the intact toolchain pin
                // for the locked window and is reversed by Drop. Atomic file
                // replacement changes bytes at a stable path; this operation
                // must preserve the original and make that path absent.
                std::fs::rename(&orig, &aside).map_err(|e| {
                    DevError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "disable_toolchain: cannot move {} aside: {e}",
                            orig.display()
                        ),
                    ))
                })?;
                // Config-derived and identical on every run: the run log when
                // the command keeps one, else the console (most locked
                // commands keep none, and a log-only line would vanish).
                output::detail_or_console(&format!("toolchain disabled: {name} moved aside"));
                moved.push((aside, orig));
            } else if path_exists(&aside)? {
                // Leftover from a hard-killed prior run: adopt so drop restores
                // it. The file stays disabled for this run (its point), then
                // returns to normal afterwards. Worth saying: it means an
                // earlier run did not clean up after itself.
                output::warn(&format!(
                    "adopting stale {name}{SUFFIX} left by an interrupted run; it is restored when this command ends"
                ));
                moved.push((aside, orig));
            }
        }
        Ok(guard)
    }
}

impl Drop for DisabledToolchain {
    fn drop(&mut self) {
        for (aside, orig) in &self.moved {
            if let Err(e) = std::fs::rename(aside, orig) {
                // Best-effort: warn rather than panic in a destructor. The next
                // `activate` will re-adopt the sidecar and try again.
                output::warn(&format!(
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
    use std::path::PathBuf;

    /// A fresh, empty scratch dir for one test. `test_name` must be unique
    /// within this module - the allocator enforces that.
    fn tmpdir(test_name: &str) -> PathBuf {
        crate::test_scratch::scratch("toolchain", test_name)
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
        assert_eq!(
            fs::read_to_string(dir.join("rust-toolchain.toml")).unwrap(),
            "a"
        );
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
    fn explicit_path_drives_lock_activation() {
        let dir = tmpdir("armed");
        let toml = dir.join("rust-toolchain.toml");
        fs::write(&toml, "pinned").unwrap();

        let policy = crate::tree_policy::TreePolicy {
            disable_toolchain: true,
            strip_rustflags: Vec::new(),
        };
        {
            let target = crate::tree_policy::TreeTarget {
                dir: &dir,
                policy: &policy,
            };
            let guard = crate::tree_policy::activate_for_lock(Some(target)).unwrap();
            assert!(guard.is_some());
            assert!(!toml.exists());
            assert!(dir.join(format!("rust-toolchain.toml{SUFFIX}")).exists());
        }
        assert!(toml.exists());

        assert!(crate::tree_policy::activate_for_lock(None).unwrap().is_none());
    }

    #[test]
    fn a_failed_later_move_puts_back_the_earlier_one() {
        let dir = tmpdir("rollback");
        let toml = dir.join("rust-toolchain.toml");
        fs::write(&toml, "a").unwrap();
        // The legacy pin's sidecar name is a directory, so removing the
        // stale sidecar fails after the first pin has moved.
        fs::write(dir.join("rust-toolchain"), "b").unwrap();
        fs::create_dir(dir.join(format!("rust-toolchain{SUFFIX}"))).unwrap();
        assert!(DisabledToolchain::activate(&dir).is_err());
        assert_eq!(fs::read_to_string(&toml).unwrap(), "a");
    }
}
