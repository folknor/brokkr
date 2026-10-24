//! What brokkr changes in a code tree for the locked window, and the guard
//! that changes it back.
//!
//! Two config-derived edits share one lifecycle: `disable_toolchain` moves the
//! tree's toolchain pin aside ([`crate::toolchain`]), and
//! `strip_rustflags` puts a filtered copy of its cargo config
//! in place ([`crate::cargo_config_strip`]). Both are applied when a fresh
//! hold of the global lock is taken (or, under `--commit`, on the worktree the
//! hold builds in) and undone before the flock is released, so no other
//! brokkr ever observes the edited tree.
//!
//! Activation always recovers the cargo config first, whatever the current
//! policy says: a run that died mid-transaction leaves the tree in a state
//! only recovery can read, and that must be undone even after the key has
//! been removed from `brokkr.toml`. With no leftovers, recovery only inspects
//! the reserved names and changes nothing.

use std::path::{Path, PathBuf};

use crate::cargo_config_strip::{self, Installed};
use crate::error::DevError;
use crate::toolchain::DisabledToolchain;

/// The edits a project's config asks for.
#[derive(Debug, Clone, Default)]
pub struct TreePolicy {
    pub disable_toolchain: bool,
    pub strip_rustflags: Vec<cargo_config_strip::StripSpec>,
}

impl TreePolicy {
    pub fn of(config: &crate::config::DevConfig) -> Self {
        Self {
            disable_toolchain: config.disable_toolchain,
            strip_rustflags: config.strip_rustflags.clone(),
        }
    }

    /// Whether this policy edits anything.
    pub fn is_active(&self) -> bool {
        self.disable_toolchain || !self.strip_rustflags.is_empty()
    }
}

/// A directory and the policy to apply to it, carried into lock acquisition.
#[derive(Clone, Copy)]
pub struct TreeTarget<'a> {
    pub dir: &'a Path,
    pub policy: &'a TreePolicy,
}

/// The edits in force on one directory. Dropping it undoes them, the cargo
/// config before the toolchain pin (reverse of activation).
pub struct HeldTree {
    // Field order is drop order.
    cargo: Option<Installed>,
    toolchain: Option<DisabledToolchain>,
    dir: PathBuf,
}

impl HeldTree {
    /// Recover `dir`'s cargo config, then apply `policy`. An error part-way
    /// drops what was already applied.
    pub fn activate(dir: &Path, policy: &TreePolicy) -> Result<Self, DevError> {
        cargo_config_strip::recover(dir)?;
        let mut held = Self {
            cargo: None,
            toolchain: None,
            dir: dir.to_owned(),
        };
        if policy.disable_toolchain {
            held.toolchain = Some(DisabledToolchain::activate(dir)?);
            crate::build_provenance::forget_host_cache();
        }
        held.cargo = cargo_config_strip::activate(dir, &policy.strip_rustflags)?;
        Ok(held)
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }
}

impl Drop for HeldTree {
    fn drop(&mut self) {
        drop(self.cargo.take());
        if self.toolchain.take().is_some() {
            crate::build_provenance::forget_host_cache();
        }
    }
}

/// Activate the lock context's tree, if any. Lock acquisition calls this after
/// the compiler drain and before publishing the capability.
pub fn activate_for_lock(target: Option<TreeTarget<'_>>) -> Result<Option<HeldTree>, DevError> {
    target
        .map(|target| HeldTree::activate(target.dir, target.policy))
        .transpose()
}

/// Undo the live hold's tree edits for a process about to exit without
/// unwinding - the `check` watchdog's backstop, whose main thread (and so the
/// guards whose drops normally restore) is stuck. Runs on a helper thread and
/// waits for it a bounded time: the backstop must reach its exit whatever the
/// filesystem does, and every state left behind is recoverable next run.
pub fn restore_active_hold() {
    let Some(dirs) = crate::lockfile::current_hold_tree_dirs() else {
        return;
    };
    let (done, finished) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("tree-restore".into())
        .spawn(move || {
            crate::toolchain::restore_dirs(&dirs);
            cargo_config_strip::take_over_and_recover(&dirs);
            // A gone receiver means the backstop stopped waiting; fine.
            done.send(()).is_ok()
        });
    if spawned.is_ok() {
        // Finished or timed out, the backstop proceeds to its exit.
        match finished.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(()) | Err(_) => {}
        }
    }
}
