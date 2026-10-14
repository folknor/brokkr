//! Crash-safe writes for the piners registries (`pins.toml`, `lints.toml`).
//!
//! Every registry writer (`corpus --reseed`/`--bless`, `lint-corpus
//! --reseed`/`--bless`/`--reanchor`) rewrites a hand-curated, git-tracked file
//! whose comments are part of the review surface, so every write goes through
//! [`write_atomic`] (see `crate::atomic_write`): a reader sees either the old
//! bytes or the new ones, never a truncated prefix.
//!
//! [`lock`] is the other half: every writer takes the global brokkr lock
//! before it reads the file it is about to rewrite, so two writers can never
//! interleave a read-modify-write (a reseed landing between a bless's read and
//! its write would otherwise be silently reverted).

use std::path::Path;
use std::{collections::BTreeMap, fs};

use serde::Deserialize;

use crate::error::DevError;
use crate::lockfile::LockGuard;

/// The shared part of a registry snapshot: its canonical file and keyword
/// grouping files. The two registries parse the canonical text into different
/// schemas after this filesystem work is complete.
pub(crate) struct RegistryFiles {
    pub canonical_text: String,
    pub keywords: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeywordFile {
    #[serde(default)]
    probes: Vec<String>,
}

/// Read a registry's canonical file and sorted sibling keyword files.
pub(crate) fn load_files(
    registry_dir: &Path,
    canonical_file: &str,
    label: &str,
) -> Result<RegistryFiles, DevError> {
    if !registry_dir.is_dir() {
        return Err(DevError::Config(format!(
            "{label}: registry directory not found: {}",
            registry_dir.display()
        )));
    }

    let canonical_path = registry_dir.join(canonical_file);
    let canonical_text = fs::read_to_string(&canonical_path).map_err(|e| {
        DevError::Config(format!(
            "{label}: failed to read {}: {e}",
            canonical_path.display()
        ))
    })?;

    let mut keywords = BTreeMap::new();
    let mut entries: Vec<_> = fs::read_dir(registry_dir)
        .map_err(DevError::Io)?
        .map(|entry| entry.map(|entry| entry.path()).map_err(DevError::Io))
        .collect::<Result<_, _>>()?;
    entries.sort();
    for path in entries {
        if path.file_name().and_then(|n| n.to_str()) == Some(canonical_file)
            || path.extension().and_then(|e| e.to_str()) != Some("toml")
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let text = fs::read_to_string(&path).map_err(|e| {
            DevError::Config(format!(
                "{label}: failed to read keyword file {}: {e}",
                path.display()
            ))
        })?;
        let keyword: KeywordFile = toml::from_str(&text)
            .map_err(|e| DevError::Config(format!("{label}: {}: {e}", path.display())))?;
        keywords.insert(stem.to_owned(), keyword.probes);
    }

    Ok(RegistryFiles {
        canonical_text,
        keywords,
    })
}

/// Return references in keyword files that are absent from the accepted pin
/// universe. Callers supply their own pending-pin policy.
pub(crate) fn dangling_keywords(
    keywords: &BTreeMap<String, Vec<String>>,
    mut is_known: impl FnMut(&str) -> bool,
) -> Vec<String> {
    let mut dangling = Vec::new();
    for (keyword, ids) in keywords {
        for id in ids {
            if !is_known(id) {
                dangling.push(format!("{keyword}.toml -> {id}"));
            }
        }
    }
    dangling
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffCounts {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
}

/// Count added, changed and removed ids between two registry snapshots.
pub(crate) fn diff_counts<T: PartialEq>(
    old: &BTreeMap<String, T>,
    new: &BTreeMap<String, T>,
) -> DiffCounts {
    let mut counts = DiffCounts::default();
    for (id, value) in new {
        match old.get(id) {
            None => counts.added += 1,
            Some(previous) if previous != value => counts.changed += 1,
            Some(_) => {}
        }
    }
    counts.removed = old.keys().filter(|id| !new.contains_key(*id)).count();
    counts
}

/// Apply one registry-specific carry-forward policy to entries that survive
/// the reseed by id.
pub(crate) fn carry_by_id<New, Old>(
    new: &mut BTreeMap<String, New>,
    old: &BTreeMap<String, Old>,
    mut carry: impl FnMut(&mut New, &Old),
) {
    for (id, value) in new {
        if let Some(previous) = old.get(id) {
            carry(value, previous);
        }
    }
}

/// Atomically replace a registry file with `contents`.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), DevError> {
    crate::atomic_write::replace(path, contents.as_bytes()).map_err(DevError::Io)
}

/// Take the global brokkr lock for a registry writer that does not otherwise
/// hold it (the two `--reseed` paths; the run-then-write paths already hold it
/// for the run).
///
/// The lock records the parsed invocation (`corpus` / `lint-corpus`); the
/// dispatcher normally holds it already, and this re-enters that hold.
pub fn lock() -> Result<LockGuard, DevError> {
    crate::context::reenter_command_lock()
}
