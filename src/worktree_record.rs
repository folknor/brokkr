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
//! One project root can govern several checkouts (the config-one-level-up
//! layout), each a separate git root with its own worktrees. They share this
//! one file, so everything that walks it is scoped to one checkout's names
//! ([`crate::worktree::is_worktree_name`]): pruning drops only *this*
//! checkout's vanished records, and another checkout's records - which look
//! missing from here only because they live under a different prefix - are
//! left alone rather than deleted, which would make that checkout's worktrees
//! sort oldest and go first at its next eviction.
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// Unix seconds when a run last created or reused this worktree.
    #[serde(default)]
    pub last_used: u64,
    /// Measured size, cached so a future size-based rule need not walk the
    /// tree. `None` when never measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

const STORE_HEADER: &str =
    "# brokkr worktree bookkeeping. Written by `--commit` runs; safe to delete.\n";

impl Store {
    /// Read the store. A missing file is an empty store. An unreadable or
    /// unparseable one is an **error**, not an empty store: reading it as
    /// empty would make every worktree look unrecorded, and the next save
    /// would overwrite the file with that loss. The caller reports it and
    /// skips the bookkeeping, and the file stays put until someone looks at
    /// it - it is safe to delete, which is the fix the message names.
    pub fn load(project_root: &Path) -> Result<Self, DevError> {
        let path = store_path(project_root);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(DevError::Config(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        Self::parse(&text).map_err(|e| {
            DevError::Config(format!(
                "{} does not parse ({e}); it is safe to delete",
                path.display()
            ))
        })
    }

    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let entries: BTreeMap<String, Record> = toml::from_str(text)?;
        Ok(Self { entries })
    }

    /// Serialize through the `toml` crate, so a name carrying a quote, a
    /// backslash or a newline is escaped instead of corrupting the file.
    fn render(&self) -> Result<String, DevError> {
        let body = toml::to_string(&self.entries)
            .map_err(|e| DevError::Config(format!("cannot serialize worktree bookkeeping: {e}")))?;
        Ok(format!("{STORE_HEADER}{body}"))
    }

    fn save(&self, project_root: &Path) -> Result<(), DevError> {
        let path = store_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic: a torn file now makes `load` refuse, rather than read empty.
        crate::atomic_write::replace(&path, self.render()?.as_bytes())?;
        Ok(())
    }

    /// Mark a worktree as used now, and persist.
    pub fn touch(project_root: &Path, name: &str) -> Result<(), DevError> {
        let mut store = Self::load(project_root)?;
        let entry = store.entries.entry(name.to_owned()).or_default();
        entry.last_used = now_secs();
        store.save(project_root)
    }

    /// Drop this checkout's records for worktrees that no longer exist on
    /// disk, so a stale entry can't be chosen as an eviction victim or inflate
    /// the count. Records outside `prefix` belong to another checkout sharing
    /// this project root and are kept (see the module doc).
    fn prune_missing(&mut self, prefix: &str, existing: &[String]) {
        self.entries.retain(|name, _| {
            !crate::worktree::is_worktree_name(prefix, name) || existing.contains(name)
        });
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

/// Evict least-recently-used worktrees so that, once `cutting` is created, at
/// most `keep` remain.
///
/// Run from [`crate::worktree::Worktree::create`]'s `before_cut` hook, which
/// fires only when a worktree is about to be cut - never on reuse - so the cost
/// lands next to a build you are already paying for rather than as an
/// unexplained pause, and so a measuring run is never turned into a
/// destructive operation by the mere act of running it. A project that stops
/// growing therefore never shrinks on its own; `brokkr clean --worktrees` is
/// the explicit hammer for that.
///
/// `cutting` is the directory about to be (re)created. It is excluded from the
/// victims and from the count, and room is made for it: the other worktrees are
/// brought down to `keep - 1`, so the steady state is `keep`, not `keep + 1`.
///
/// The caller must hold the global lock: the removals and the
/// read-modify-write of `worktrees.toml` both race a concurrent brokkr
/// otherwise.
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
pub fn enforce(
    project_root: &Path,
    git_root: &Path,
    keep: usize,
    cutting: &Path,
) -> Result<(), DevError> {
    let listed: Vec<String> = crate::worktree::list(git_root)?
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        .collect();
    let prefix = crate::worktree::name_prefix(git_root);

    let mut store = Store::load(project_root)?;
    store.prune_missing(&prefix, &listed);

    let cutting_name = cutting.file_name().and_then(|n| n.to_str());
    let existing: Vec<String> = listed
        .into_iter()
        .filter(|n| Some(n.as_str()) != cutting_name)
        .collect();

    // Room for the one about to be cut. `keep` is at least 1 in practice
    // (config maps 0 to the default), and saturating keeps 0 meaning "evict
    // every other one" rather than wrapping.
    let allowed = keep.saturating_sub(1);
    if existing.len() <= allowed {
        return store.save(project_root);
    }

    let mut over = existing.len() - allowed;
    let mut skipped: Vec<String> = Vec::new();
    for name in store.lru_order(&existing) {
        if over == 0 {
            break;
        }
        let path = git_root
            .parent()
            .map(|p| p.join(&name))
            .unwrap_or_else(|| PathBuf::from(&name));

        if crate::worktree::is_dirty(&path) {
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
    store.save(project_root)
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

    const P: &str = ".brokkr-worktree-foo-";

    #[test]
    fn prune_missing_drops_records_for_vanished_worktrees() {
        let gone = format!("{P}0001");
        let here = format!("{P}0002");
        let mut store = store_with(&[(gone.as_str(), 100), (here.as_str(), 200)]);
        store.prune_missing(P, std::slice::from_ref(&here));
        assert!(store.entries.contains_key(&here));
        assert!(!store.entries.contains_key(&gone));
    }

    #[test]
    fn prune_missing_keeps_a_stale_record_from_inflating_the_count() {
        // A record for a worktree removed behind brokkr's back would otherwise
        // make the count look over the bound and evict a live one.
        let (a, b, c) = (format!("{P}a"), format!("{P}b"), format!("{P}c"));
        let mut store = store_with(&[(a.as_str(), 1), (b.as_str(), 2), (c.as_str(), 3)]);
        store.prune_missing(P, std::slice::from_ref(&a));
        assert_eq!(store.entries.len(), 1);
    }

    #[test]
    fn prune_missing_leaves_another_checkouts_records_alone() {
        // One project root governing checkouts `foo` and `foo-bar`: `foo`'s
        // listing cannot see `foo-bar`'s worktrees, and must not read that as
        // their having vanished.
        let mine = format!("{P}0001");
        let theirs = ".brokkr-worktree-foo-bar-0001";
        let mut store = store_with(&[(mine.as_str(), 1), (theirs, 2)]);
        store.prune_missing(P, &[]);
        assert!(!store.entries.contains_key(&mine));
        assert!(store.entries.contains_key(theirs));
    }

    #[test]
    fn a_name_needing_escapes_round_trips() {
        let mut store = store_with(&[("quote\"back\\slash\nnewline", 7), ("plain", 9)]);
        if let Some(r) = store.entries.get_mut("plain") {
            r.size_bytes = Some(1234);
        }
        let text = store.render().expect("render");
        let back = Store::parse(&text).expect("parse");
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries["quote\"back\\slash\nnewline"].last_used, 7);
        assert_eq!(back.entries["plain"].size_bytes, Some(1234));
        assert_eq!(back.entries["quote\"back\\slash\nnewline"].size_bytes, None);
    }

    #[test]
    fn an_unparseable_store_is_an_error_not_an_empty_store() {
        let dir = crate::test_scratch::scratch("worktree_record", "unparseable");
        let brokkr = dir.join(".brokkr");
        std::fs::create_dir_all(&brokkr).expect("mkdir");
        std::fs::write(brokkr.join("worktrees.toml"), "[\"unterminated\n").expect("write");
        assert!(Store::load(&dir).is_err());
        // And touch must not paper over it by rewriting the file.
        assert!(Store::touch(&dir, "x").is_err());
        let after = std::fs::read_to_string(brokkr.join("worktrees.toml")).expect("read");
        assert_eq!(after, "[\"unterminated\n");
    }

    #[test]
    fn a_missing_store_is_empty() {
        let dir = crate::test_scratch::scratch("worktree_record", "missing");
        let store = Store::load(&dir).expect("missing file is not an error");
        assert!(store.entries.is_empty());
    }
}
