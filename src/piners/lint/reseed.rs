//! `brokkr lint-corpus --reseed`: stamp `lints.toml` from the snippet tree.
//!
//! The bootstrap and after-edit re-stamp - the only sanctioned way
//! `lints.toml` comes into existence or refreshes its `xxh128` hashes
//! (`--verify-only` and runs can only check pins that already exist; there is
//! no `xxhsum` on PATH). The lint analogue of `corpus --reseed`, but far
//! simpler: a lint probe is a single `.pine` snippet (no `tv_trades.csv`
//! oracle, no feeds, no `[roots]`), so discovery is just every `.pine` under
//! the snippet dir, keyed by file stem.
//!
//! - `--reseed --all` - walk the snippet dir, stamp every snippet; a snippet
//!   that vanished drops out of the file.
//! - `--reseed --probe <id>` (repeatable) - upsert the named snippet(s),
//!   leaving the rest intact.
//!
//! Touches the pinned *content* (the snippet path + hash) only; each
//! surviving probe's `expected` disposition and TV anchor (`tv` /
//! `tv_anchored_at`) are carried forward. Comment-preserving via
//! [`crate::piners::lint::lints_write`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{LintConfig, PinersConfig};
use crate::error::DevError;
use crate::output;
use crate::piners::lint::cmd::LintArgs;
use crate::piners::lint::lints_write;
use crate::piners::lint::registry::{self, LintPin};
use crate::piners::registry::FilePin;
use crate::preflight;

const LINTS_FILE: &str = "lints.toml";

/// Entry point for `brokkr lint-corpus --reseed`.
pub fn run(
    project_root: &Path,
    piners_cfg: &PinersConfig,
    lint_cfg: &LintConfig,
    args: &LintArgs,
) -> Result<(), DevError> {
    if !args.keywords.is_empty() {
        return Err(DevError::Config(
            "lint-corpus --reseed: --keyword is not supported (keywords reference \
             already-pinned ids). Use --all or --probe."
                .into(),
        ));
    }
    if args.all && !args.probe.is_empty() {
        return Err(DevError::Config(
            "lint-corpus --reseed: --all and --probe are mutually exclusive.".into(),
        ));
    }

    let corpus_root = project_root.join(piners_cfg.corpus_root());
    let snippets_dir = project_root.join(lint_cfg.snippets_dir());
    let registry_dir = project_root.join(lint_cfg.registry_dir());
    let lints_path = registry_dir.join(LINTS_FILE);

    let existing_text = if lints_path.exists() {
        Some(std::fs::read_to_string(&lints_path).map_err(DevError::Io)?)
    } else {
        None
    };
    let existing = match existing_text.as_deref() {
        Some(text) => registry::parse_lints(text, &lints_path)?.probes,
        None => BTreeMap::new(),
    };

    let discovered = discover(&snippets_dir)?;

    let mut new_pins = if args.all {
        let mut pins = BTreeMap::new();
        for (id, abs) in &discovered {
            pins.insert(id.clone(), stamp_one(id, abs, &corpus_root)?);
        }
        pins
    } else if !args.probe.is_empty() {
        let mut merged = existing.clone();
        for id in &args.probe {
            let abs = discovered.get(id).ok_or_else(|| {
                DevError::Config(format!(
                    "lint-corpus --reseed: snippet '{id}' not found under {} \
                     (no '{id}.pine')",
                    snippets_dir.display()
                ))
            })?;
            merged.insert(id.clone(), stamp_one(id, abs, &corpus_root)?);
        }
        merged
    } else {
        return Err(DevError::Config(
            "lint-corpus --reseed requires --all (full regen) or --probe <id> \
             (repeatable upsert)."
                .into(),
        ));
    };

    carry_preserved(&mut new_pins, &existing);
    let (added, changed, removed) = diff(&existing, &new_pins);

    std::fs::create_dir_all(&registry_dir).map_err(DevError::Io)?;
    std::fs::write(
        &lints_path,
        lints_write::render_lints(existing_text.as_deref(), &new_pins)?,
    )
    .map_err(DevError::Io)?;

    output::lint_msg(&format!(
        "reseed: {} snippet(s) -> {} (added={added} changed={changed} removed={removed})",
        new_pins.len(),
        lints_path.display(),
    ));
    Ok(())
}

/// Walk `snippets_dir` recursively for `*.pine` files. The id is the file
/// stem; a stem collision across subdirs is a hard error (ids key
/// `lints.toml`). Dot-dirs are skipped.
fn discover(snippets_dir: &Path) -> Result<BTreeMap<String, PathBuf>, DevError> {
    if !snippets_dir.is_dir() {
        return Err(DevError::Config(format!(
            "lint-corpus --reseed: snippet dir not found: {}",
            snippets_dir.display()
        )));
    }
    let mut found = BTreeMap::new();
    walk(snippets_dir, &mut found)?;
    Ok(found)
}

fn walk(dir: &Path, found: &mut BTreeMap<String, PathBuf>) -> Result<(), DevError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(DevError::Io)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if name.as_deref().is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        if path.is_dir() {
            walk(&path, found)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("pine") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(prev) = found.get(stem) {
            return Err(DevError::Config(format!(
                "lint-corpus --reseed: snippet id '{stem}' is ambiguous - two files \
                 share the stem:\n  {}\n  {}\n  (ids key lints.toml; rename one)",
                prev.display(),
                path.display()
            )));
        }
        found.insert(stem.to_owned(), path);
    }
    Ok(())
}

/// Stamp a single snippet: its path relative to `corpus_root` plus its hash.
fn stamp_one(id: &str, abs: &Path, corpus_root: &Path) -> Result<LintPin, DevError> {
    let rel = abs
        .strip_prefix(corpus_root)
        .map_err(|_| {
            DevError::Config(format!(
                "lint-corpus --reseed: snippet '{id}' escapes corpus root:\n  {}\n  \
                 (snippets must live under [piners] corpus_root)",
                abs.display()
            ))
        })?
        .to_path_buf();
    Ok(LintPin {
        expected: None,
        tv_anchored_at: None,
        tv: Vec::new(),
        pine: FilePin {
            path: rel,
            xxh128: preflight::compute_xxh128(abs)?,
        },
    })
}

/// Carry each surviving probe's `expected` disposition and TV anchor forward
/// onto the freshly content-stamped pin. A snippet new to the corpus stays
/// unblessed (the gate's "must bless").
fn carry_preserved(new: &mut BTreeMap<String, LintPin>, old: &BTreeMap<String, LintPin>) {
    for (id, pin) in new.iter_mut() {
        if let Some(prev) = old.get(id) {
            pin.expected = prev.expected.clone();
            pin.tv_anchored_at = prev.tv_anchored_at.clone();
            pin.tv = prev.tv.clone();
        }
    }
}

/// Added/changed/removed counts between the old and new pin sets.
fn diff(old: &BTreeMap<String, LintPin>, new: &BTreeMap<String, LintPin>) -> (usize, usize, usize) {
    let mut added = 0;
    let mut changed = 0;
    for (id, pin) in new {
        match old.get(id) {
            None => added += 1,
            Some(prev) if prev != pin => changed += 1,
            Some(_) => {}
        }
    }
    let removed = old.keys().filter(|k| !new.contains_key(*k)).count();
    (added, changed, removed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn discover_finds_pine_by_stem_and_errors_on_collision() {
        let root = std::env::temp_dir().join(format!("brokkr_lint_reseed_{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/clean-01.pine"), b"//@version=6\n").unwrap();
        std::fs::write(root.join("nope.txt"), b"x").unwrap();

        let found = discover(&root).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found.contains_key("clean-01"));

        // A second file with the same stem in a sibling dir collides.
        std::fs::write(root.join("b/clean-01.pine"), b"//@version=6\n").unwrap();
        let err = discover(&root).unwrap_err();
        std::fs::remove_dir_all(&root).ok();
        assert!(format!("{err:?}").contains("ambiguous"));
    }

    #[test]
    fn carry_preserved_keeps_expected_and_anchor() {
        let mut old = BTreeMap::new();
        let mut blessed = LintPin {
            expected: Some("agree_flagged".to_owned()),
            tv_anchored_at: Some("2026-06-22T00:00:00Z".to_owned()),
            tv: vec![registry::TvDiag {
                line: 1,
                col: Some(1),
                severity: "error".to_owned(),
            }],
            pine: FilePin {
                path: PathBuf::from("lint/keep.pine"),
                xxh128: "old".into(),
            },
        };
        blessed.pine.xxh128 = "old".into();
        old.insert("keep".to_owned(), blessed);

        let mut new = BTreeMap::new();
        new.insert(
            "keep".to_owned(),
            LintPin {
                expected: None,
                tv_anchored_at: None,
                tv: Vec::new(),
                pine: FilePin {
                    path: PathBuf::from("lint/keep.pine"),
                    xxh128: "new".into(),
                },
            },
        );

        carry_preserved(&mut new, &old);
        let kept = &new["keep"];
        assert_eq!(kept.expected.as_deref(), Some("agree_flagged"));
        assert_eq!(kept.tv.len(), 1);
        assert_eq!(kept.pine.xxh128, "new"); // content still refreshed
    }
}
