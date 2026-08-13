//! Last-used bookkeeping for persistent `--commit` worktrees, and the
//! retention rule that keeps them from filling the disk.
//!
//! ## Why a record rather than mtimes
//!
//! Eviction needs to know which worktree was used least recently.
//! `target/`'s mtime is a decent proxy - the only operation that needs a
//! worktree is a measuring run, which is exactly what rebuilds into it - but
//! mtimes are clobbered by rsync, backups, editors and a stray `touch`, and the
//! consequence of a wrong answer here is deleting the wrong 1.3G. brokkr writes
//! the timestamp itself instead.
//!
//! ## Why it lives at the project root
//!
//! In `.brokkr/worktrees.toml`, beside every other brokkr-owned store.
//!
//! Not in a command's own directory: [`crate::worktree::Worktree::create`] is
//! shared machinery, and `--commit` exists on `dellingr`, `sluggrs hotpath`,
//! pbfhogg's benches and `ratatoskr sync` as well as `bench`. A record kept
//! under one command's store would date only that command's worktrees and leave
//! the rest invisible to eviction, so the bound would silently fail to apply to
//! most of its subjects.
//!
//! Not *inside* the worktree either, which is the tempting place to put it. A
//! worktree is a git checkout, an untracked file in it makes `git status
//! --porcelain` non-empty, and `git::collect` runs against the effective build
//! root - which for a `--commit` run is the worktree. A marker file there would
//! trip the dirty-tree refusal, the same way brokkr's own toolchain sidecar
//! did.
//!
//! ## What the bound is and isn't
//!
//! A per-project count. It is a **growth damper, not a bound**: each project
//! keeps its own N, so the disk-wide total scales with how many projects you
//! have touched. Only a global byte budget could promise "never fills the
//! disk", and count is a proxy for the real constraint, which is bytes. The
//! cached `size_bytes` in each record exists so that a size-based rule can be
//! added later without walking a hundred thousand files at eviction time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::DevError;
use crate::output;

/// Default number of worktrees kept per project.
///
/// Chosen for the *heaviest* dependency graph rather than the lightest. A cold
/// worktree build is ~36s on nautilus but minutes on elivagar, and a silent
/// multi-minute stall is a worse failure than some gigabytes that were not
/// strictly needed. Six also clears the shape of a real study: a baseline, the
/// commit under test, the head, a rework, and room to iterate the rework twice
/// without evicting the baseline you are still comparing against. Raise it per
/// host with `worktree_keep` in `brokkr.toml`.
pub const DEFAULT_KEEP: usize = 6;

/// One worktree's bookkeeping.
#[derive(Debug, Clone, Default)]
pub struct Record {
    /// Unix seconds when a run last created or reused this worktree.
    pub last_used: u64,
    /// Measured size, cached so a future size-based rule need not walk the
    /// tree. `None` when never measured.
    pub size_bytes: Option<u64>,
}

/// The whole file: worktree directory name -> record.
#[derive(Debug, Default)]
pub struct Store {
    entries: BTreeMap<String, Record>,
}

fn store_path(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("worktrees.toml")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    /// Read the store. A missing or unparseable file is an empty store, not an
    /// error: this is advisory bookkeeping, and losing it costs a suboptimal
    /// eviction order, never data.
    pub fn load(project_root: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(store_path(project_root)) else {
            return Self::default();
        };
        let Ok(value) = text.parse::<toml::Table>() else {
            return Self::default();
        };
        let mut entries = BTreeMap::new();
        for (name, item) in &value {
            let Some(table) = item.as_table() else { continue };
            let last_used = table
                .get("last_used")
                .and_then(toml::Value::as_integer)
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(0);
            let size_bytes = table
                .get("size_bytes")
                .and_then(toml::Value::as_integer)
                .and_then(|v| u64::try_from(v).ok());
            entries.insert(
                name.clone(),
                Record {
                    last_used,
                    size_bytes,
                },
            );
        }
        Self { entries }
    }

    fn save(&self, project_root: &Path) -> Result<(), DevError> {
        let path = store_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::from(
            "# brokkr worktree bookkeeping. Written by `--commit` runs; safe to delete.\n",
        );
        for (name, rec) in &self.entries {
            out.push_str(&format!("[\"{name}\"]\n"));
            out.push_str(&format!("last_used = {}\n", rec.last_used));
            if let Some(size) = rec.size_bytes {
                out.push_str(&format!("size_bytes = {size}\n"));
            }
            out.push('\n');
        }
        std::fs::write(&path, out)?;
        Ok(())
    }

    /// Mark a worktree as used now, and persist.
    pub fn touch(project_root: &Path, name: &str) -> Result<(), DevError> {
        let mut store = Self::load(project_root);
        let entry = store.entries.entry(name.to_owned()).or_default();
        entry.last_used = now_secs();
        store.save(project_root)
    }

    /// Drop records for worktrees that no longer exist on disk, so a stale
    /// entry can't be chosen as an eviction victim or inflate the count.
    fn prune_missing(&mut self, existing: &[String]) {
        self.entries.retain(|name, _| existing.contains(name));
    }

    /// Worktree names ordered least-recently-used first.
    ///
    /// An existing worktree with no record sorts oldest: it predates the
    /// bookkeeping, so it is the best guess at "least recently wanted".
    fn lru_order(&self, existing: &[String]) -> Vec<String> {
        let mut names: Vec<String> = existing.to_vec();
        names.sort_by_key(|n| self.entries.get(n).map_or(0, |r| r.last_used));
        names
    }
}

/// Evict least-recently-used worktrees until at most `keep` remain.
///
/// Called from [`crate::worktree::Worktree::create`] *before* a new worktree is
/// cut, so the cost lands next to a build you are already paying for rather
/// than as an unexplained pause, and so a measuring run is never turned into a
/// destructive operation by the mere act of running it. A project that stops
/// growing therefore never shrinks on its own; `brokkr clean --worktrees` is
/// the explicit hammer for that.
///
/// Never evicts a worktree with uncommitted work. That is a correctness rule,
/// not a courtesy: a dirty worktree is the one place where removal destroys
/// something unrecoverable, so it is skipped regardless of whether git would
/// have succeeded. Any other failure is skipped too, and reported - the
/// benchmark you actually asked for is not worth failing over housekeeping.
///
/// Because skips can hold the count above `keep`, this reports the overage
/// **every** run it persists, not once when a removal fails. A damper that has
/// quietly stopped working is the original problem, and you should not learn
/// about it from the volume filling up.
pub fn enforce(project_root: &Path, git_root: &Path, keep: usize) -> Result<(), DevError> {
    let existing: Vec<String> = crate::worktree::list(git_root)?
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        .collect();

    let mut store = Store::load(project_root);
    store.prune_missing(&existing);

    if existing.len() <= keep {
        drop(store.save(project_root));
        return Ok(());
    }

    let mut over = existing.len() - keep;
    let mut skipped: Vec<String> = Vec::new();
    for name in store.lru_order(&existing) {
        if over == 0 {
            break;
        }
        let path = git_root
            .parent()
            .map(|p| p.join(&name))
            .unwrap_or_else(|| PathBuf::from(&name));

        if is_dirty(&path) {
            skipped.push(format!("{name} (uncommitted work)"));
            continue;
        }
        match crate::worktree::remove_one(git_root, &path) {
            Ok(()) => {
                output::run_msg(&format!("evicted least-recently-used worktree {name}"));
                store.entries.remove(&name);
                over -= 1;
            }
            Err(e) => skipped.push(format!("{name} ({e})")),
        }
    }

    if over > 0 {
        output::warn(&format!(
            "worktree retention exceeded by {over} (keep = {keep}); skipped: {}",
            skipped.join(", ")
        ));
    }
    drop(store.save(project_root));
    Ok(())
}

/// True when the worktree has uncommitted or untracked content.
///
/// Deliberately does not exclude anything. `git::check_clean`'s exclusions
/// exist so brokkr's own outputs can't block a *measurement*; here the question
/// is whether deleting this directory would destroy work, and for that, an
/// untracked file counts.
fn is_dirty(path: &Path) -> bool {
    let Ok(out) = std::process::Command::new("git")
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

#[cfg(test)]
mod tests {
    use super::{Record, Store};
    use std::collections::BTreeMap;

    fn store_with(pairs: &[(&str, u64)]) -> Store {
        let mut entries = BTreeMap::new();
        for (name, last_used) in pairs {
            entries.insert(
                (*name).to_owned(),
                Record {
                    last_used: *last_used,
                    size_bytes: None,
                },
            );
        }
        Store { entries }
    }

    #[test]
    fn lru_order_is_oldest_first() {
        let store = store_with(&[("a", 300), ("b", 100), ("c", 200)]);
        let existing = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(store.lru_order(&existing), vec!["b", "c", "a"]);
    }

    #[test]
    fn a_worktree_with_no_record_sorts_oldest() {
        // Predates the bookkeeping, so it is the best available guess at
        // "least recently wanted" - and must not be treated as freshest.
        let store = store_with(&[("known", 500)]);
        let existing = vec!["known".to_owned(), "unrecorded".to_owned()];
        assert_eq!(store.lru_order(&existing), vec!["unrecorded", "known"]);
    }

    #[test]
    fn prune_missing_drops_records_for_vanished_worktrees() {
        let mut store = store_with(&[("gone", 100), ("here", 200)]);
        store.prune_missing(&["here".to_owned()]);
        assert!(store.entries.contains_key("here"));
        assert!(!store.entries.contains_key("gone"));
    }

    #[test]
    fn prune_missing_keeps_a_stale_record_from_inflating_the_count() {
        // A record for a worktree removed behind brokkr's back would otherwise
        // make the count look over the bound and evict a live one.
        let mut store = store_with(&[("a", 1), ("b", 2), ("c", 3)]);
        store.prune_missing(&["a".to_owned()]);
        assert_eq!(store.entries.len(), 1);
    }
}
