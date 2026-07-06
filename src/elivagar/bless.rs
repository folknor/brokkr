//! `brokkr bless` - promote a tilegen output to the dataset's regress
//! reference.
//!
//! Blessing is manual and deliberate: it is run only after a landing's full
//! gate battery (including human QA) passes. It copies the current archive
//! from the durable output store into `data/blessed/<dataset>-<commit>.pmtiles`
//! (gitignored) and writes a singular `[<host>.datasets.<D>.blessed]` entry
//! (`file`, `commit`, `xxhash`) into brokkr.toml via `toml_edit`, preserving
//! every hand-written comment. It refuses a dirty working tree (results.db and
//! *.md excluded, matching bench discipline) - a hash recorded from
//! uncommitted state does not reproduce.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config;
use crate::error::DevError;
use crate::output;
use crate::preflight;
use crate::resolve::resolve_pmtiles_by_commit;

pub fn run(
    project_root: &Path,
    paths: &config::ResolvedPaths,
    dataset: &str,
    commit: Option<&str>,
    file: Option<&str>,
) -> Result<(), DevError> {
    // Refuse to bless from a dirty tree: the recorded commit would not
    // reproduce the archive. results.db and *.md are excluded (bench discipline).
    let git = crate::git::collect(project_root)?;
    if !git.is_clean {
        return Err(DevError::Config(
            "refusing to bless from a dirty working tree (results.db and *.md \
             excluded); commit or stash your changes first"
                .to_owned(),
        ));
    }

    // The commit to record: the explicit --commit, else current HEAD.
    let source_commit = commit.map(str::to_owned).unwrap_or(git.commit);

    // Resolve the archive to bless exactly as `regress` would pick it.
    let source = resolve_pmtiles_by_commit(dataset, commit, file, paths, project_root)?;

    // Copy into the durable, non-wiped `data/blessed/` store.
    let rel_file = format!("blessed/{dataset}-{source_commit}.pmtiles");
    let dest = paths.data_dir.join(&rel_file);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&source, &dest)?;

    // Record the hash of the blessed copy for regress-time verification.
    let xxhash = preflight::compute_xxh128(&dest)?;

    // Register in brokkr.toml, comment-preserving.
    let toml_path = project_root.join("brokkr.toml");
    let existing = std::fs::read_to_string(&toml_path)?;
    let updated = write_blessed_entry(
        &existing,
        &paths.hostname,
        dataset,
        &rel_file,
        &source_commit,
        &xxhash,
    )?;
    std::fs::write(&toml_path, updated)?;

    output::run_msg(&format!("blessed {} -> {}", source.display(), dest.display()));
    output::run_msg(&format!(
        "registered [{}.datasets.{dataset}.blessed] (commit {source_commit}, xxhash {xxhash})",
        paths.hostname
    ));
    Ok(())
}

/// Insert or update the singular `[<host>.datasets.<dataset>.blessed]` table in
/// the brokkr.toml text, preserving comments and formatting of everything that
/// survives. Fields on an existing entry are replaced in place (keeping decor);
/// a fresh entry is inserted as a table with the three string fields.
fn write_blessed_entry(
    existing: &str,
    host: &str,
    dataset: &str,
    file: &str,
    commit: &str,
    xxhash: &str,
) -> Result<String, DevError> {
    let mut doc: DocumentMut = existing
        .parse()
        .map_err(|e| DevError::Config(format!("brokkr.toml: {e}")))?;

    let host_tbl = ensure_table(doc.as_table_mut(), host)?;
    let ds_parent = ensure_table(host_tbl, "datasets")?;
    let ds_tbl = ensure_table(ds_parent, dataset)?;

    let fresh = !ds_tbl.contains_key("blessed");
    let blessed = ensure_table(ds_tbl, "blessed")?;
    if fresh {
        // A blank line before a newly-created `[...blessed]` header.
        blessed.decor_mut().set_prefix("\n");
    }
    set_str(blessed, "file", file);
    set_str(blessed, "commit", commit);
    set_str(blessed, "xxhash", xxhash);

    Ok(doc.to_string())
}

/// Return a mutable reference to `parent[key]` as a table, creating an implicit
/// table if the key is absent. Errors if the key exists but is not a table.
fn ensure_table<'a>(parent: &'a mut Table, key: &str) -> Result<&'a mut Table, DevError> {
    if !parent.contains_key(key) {
        let mut fresh = Table::new();
        fresh.set_implicit(true);
        parent.insert(key, Item::Table(fresh));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| DevError::Config(format!("brokkr.toml: [{key}] is not a table")))
}

/// Set `key = "value"`, preserving the existing value's decor (spacing around
/// `=` and any trailing `# comment`) when the key already exists.
fn set_str(table: &mut Table, key: &str, value: &str) {
    let new = Value::from(value);
    if let Some(Item::Value(existing)) = table.get_mut(key) {
        let mut new = new;
        *new.decor_mut() = existing.decor().clone();
        *existing = new;
    } else {
        table.insert(key, Item::Value(new));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const BASE: &str = "\
[plantasjen]
scratch = \"data/tilegen_tmp\"

[plantasjen.datasets.denmark]
origin = \"Geofabrik\" # hand-maintained
bbox = \"12.4,55.6,12.7,55.8\"

[plantasjen.datasets.denmark.pbf.raw]
file = \"denmark.osm.pbf\"
";

    #[test]
    fn inserts_blessed_and_preserves_comments() {
        let out = write_blessed_entry(BASE, "plantasjen", "denmark", "blessed/denmark-abc123.pmtiles", "abc123", "deadbeef").unwrap();
        // Comment survives.
        assert!(out.contains("origin = \"Geofabrik\" # hand-maintained"));
        // New header + fields present.
        assert!(out.contains("[plantasjen.datasets.denmark.blessed]"));
        assert!(out.contains("file = \"blessed/denmark-abc123.pmtiles\""));
        assert!(out.contains("commit = \"abc123\""));
        assert!(out.contains("xxhash = \"deadbeef\""));
        // The written entry deserializes into the real config type.
        let tbl: toml::Table = toml::from_str(&out).unwrap();
        let entry = tbl["plantasjen"]["datasets"]["denmark"]["blessed"].clone();
        let b: crate::config::BlessedEntry = entry.try_into().unwrap();
        assert_eq!(b.commit, "abc123");
        assert_eq!(b.xxhash.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn rebless_updates_in_place() {
        let first = write_blessed_entry(BASE, "plantasjen", "denmark", "blessed/denmark-abc123.pmtiles", "abc123", "aaaa").unwrap();
        let second = write_blessed_entry(&first, "plantasjen", "denmark", "blessed/denmark-def456.pmtiles", "def456", "bbbb").unwrap();
        assert!(second.contains("commit = \"def456\""));
        assert!(second.contains("xxhash = \"bbbb\""));
        assert!(!second.contains("abc123"));
        assert!(!second.contains("aaaa"));
        // Still exactly one blessed table.
        assert_eq!(second.matches("[plantasjen.datasets.denmark.blessed]").count(), 1);
    }
}
