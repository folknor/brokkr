//! `brokkr corpus --reseed`: stamp `pins.toml` from the corpus filesystem.
//!
//! This is the bootstrap and the after-re-pin re-stamp - the only
//! sanctioned way `pins.toml` comes into existence or gets refreshed.
//! `--verify-only` can only compare against pins that already exist; there
//! is no `xxhsum` on PATH. Reseed adopting the current submodule content
//! is the deliberate human act of re-validating the oracle, so `git diff
//! pins.toml` is the review surface and no drift override exists.
//!
//! Unlike every other mode, reseed's selection universe is the corpus
//! **filesystem**, not `pins.toml`: it must be able to pin probes that are
//! not pinned yet, so it resolves ids against
//! `corpus_root/validation/<id>/`.
//!
//! - `--reseed --all` - walk `corpus_root/validation/*`, stamp every
//!   probe. Authoritative full regen: a probe whose dir vanished upstream
//!   drops out of the file.
//! - `--reseed --probe <id>` (repeatable) - upsert the named probe(s),
//!   leaving the rest intact.
//!
//! Output is deterministic (entries sorted by id, inline `pine`/`csv = {
//! path, xxh128 }`) for clean diffs, and idempotent (re-stamping
//! overwrites hashes - the re-pin case).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::PinersConfig;
use crate::error::DevError;
use crate::output;
use crate::piners::cmd::CorpusArgs;
use crate::piners::registry::{self, FilePin, Pin};
use crate::preflight;

/// Probe directory under `corpus_root`, and the two pinned files.
const VALIDATION_SUBDIR: &str = "validation";
const PINE_FILE: &str = "strategy.pine";
const CSV_FILE: &str = "tv_trades.csv";
const PINS_FILE: &str = "pins.toml";

/// Entry point for `brokkr corpus --reseed`.
pub fn run(
    project_root: &Path,
    cfg: &PinersConfig,
    args: &CorpusArgs,
) -> Result<(), DevError> {
    if !args.keywords.is_empty() {
        return Err(DevError::Config(
            "corpus --reseed: --keyword is not supported (keywords reference \
             already-pinned ids). Use --all or --probe."
                .into(),
        ));
    }
    if args.all && !args.probe.is_empty() {
        return Err(DevError::Config(
            "corpus --reseed: --all and --probe are mutually exclusive.".into(),
        ));
    }

    let corpus_root = project_root.join(cfg.corpus_root());
    let validation = corpus_root.join(VALIDATION_SUBDIR);
    let registry_dir = project_root.join(cfg.registry_dir());
    let pins_path = registry_dir.join(PINS_FILE);

    let existing = if pins_path.exists() {
        registry::load_pins(&pins_path)?
    } else {
        BTreeMap::new()
    };

    let mut new_pins = if args.all {
        stamp_all(&validation, &corpus_root)?
    } else if !args.probe.is_empty() {
        let mut merged = existing.clone();
        for id in &args.probe {
            merged.insert(id.clone(), stamp_one(id, &corpus_root)?);
        }
        merged
    } else {
        return Err(DevError::Config(
            "corpus --reseed requires --all (full regen) or --probe <id> \
             (repeatable upsert)."
                .into(),
        ));
    };

    // Reseed touches the pinned content (pine/csv hashes) only. The blessed
    // `expected` disposition is an independent contract owned by `--bless`,
    // so carry it forward for every probe that survives the re-stamp.
    carry_expected(&mut new_pins, &existing);

    let diff = Diff::compute(&existing, &new_pins);

    std::fs::create_dir_all(&registry_dir).map_err(DevError::Io)?;
    std::fs::write(&pins_path, registry::serialize_pins(&new_pins)).map_err(DevError::Io)?;

    output::corpus_msg(&format!(
        "reseed: {} probe(s) -> {} (added={} changed={} removed={})",
        new_pins.len(),
        pins_path.display(),
        diff.added,
        diff.changed,
        diff.removed,
    ));
    Ok(())
}

/// Stamp a single probe by id, resolving `corpus_root/validation/<id>/`.
fn stamp_one(id: &str, corpus_root: &Path) -> Result<Pin, DevError> {
    let pine_rel = PathBuf::from(VALIDATION_SUBDIR).join(id).join(PINE_FILE);
    let csv_rel = PathBuf::from(VALIDATION_SUBDIR).join(id).join(CSV_FILE);
    let pine_abs = corpus_root.join(&pine_rel);
    let csv_abs = corpus_root.join(&csv_rel);

    for (label, path) in [(PINE_FILE, &pine_abs), (CSV_FILE, &csv_abs)] {
        if !path.exists() {
            return Err(DevError::Config(format!(
                "corpus --reseed: probe '{id}' is missing {label}: {}",
                path.display()
            )));
        }
    }

    Ok(Pin {
        // Content-only stamp; the caller carries `expected` forward.
        expected: None,
        pine: FilePin {
            path: pine_rel,
            xxh128: preflight::compute_xxh128(&pine_abs)?,
        },
        csv: FilePin {
            path: csv_rel,
            xxh128: preflight::compute_xxh128(&csv_abs)?,
        },
    })
}

/// Copy each surviving probe's blessed `expected` from the old pin set into
/// the freshly stamped one. A probe new to the corpus stays `expected: None`
/// (unblessed), which the gate treats as a hard "must bless".
fn carry_expected(new: &mut BTreeMap<String, Pin>, old: &BTreeMap<String, Pin>) {
    for (id, pin) in new.iter_mut() {
        if let Some(prev) = old.get(id) {
            pin.expected = prev.expected.clone();
        }
    }
}

/// Walk `validation/` and stamp every single-oracle parity probe.
///
/// The upstream corpus contains dirs that are not parity probes: a
/// multi-mode self-test (`strategy.pine` + `trades-*.csv`, no
/// `tv_trades.csv`), and a container of nested per-symbol probes (no files
/// at its top level). Both lack `tv_trades.csv` at the top level, so the
/// presence of the oracle is the discriminator: a top-level dir without
/// `tv_trades.csv` is skipped (counted), not an error. `--probe <id>` -
/// where the user named the probe explicitly - still hard-errors on a
/// missing file.
fn stamp_all(validation: &Path, corpus_root: &Path) -> Result<BTreeMap<String, Pin>, DevError> {
    if !validation.is_dir() {
        return Err(DevError::Config(format!(
            "corpus --reseed --all: corpus probe directory not found: {}",
            validation.display()
        )));
    }

    let mut ids: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(validation).map_err(DevError::Io)? {
        let entry = entry.map_err(DevError::Io)?;
        if !entry.file_type().map_err(DevError::Io)?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            ids.push(name.to_owned());
        }
    }
    ids.sort();

    let mut pins = BTreeMap::new();
    let mut skipped = 0usize;
    for id in ids {
        if !validation.join(&id).join(CSV_FILE).exists() {
            skipped += 1; // non-parity dir: self-test, symbol container, etc.
            continue;
        }
        pins.insert(id.clone(), stamp_one(&id, corpus_root)?);
    }

    if skipped > 0 {
        output::corpus_msg(&format!(
            "skipped {skipped} non-parity dir(s) (no {CSV_FILE})"
        ));
    }

    Ok(pins)
}

/// Added/changed/removed counts between the old and new pin sets.
struct Diff {
    added: usize,
    changed: usize,
    removed: usize,
}

impl Diff {
    fn compute(old: &BTreeMap<String, Pin>, new: &BTreeMap<String, Pin>) -> Self {
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
        Self {
            added,
            changed,
            removed,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn pin(p: &str, h: &str) -> Pin {
        Pin {
            expected: None,
            pine: FilePin {
                path: PathBuf::from(format!("validation/{p}/strategy.pine")),
                xxh128: h.to_owned(),
            },
            csv: FilePin {
                path: PathBuf::from(format!("validation/{p}/tv_trades.csv")),
                xxh128: h.to_owned(),
            },
        }
    }

    #[test]
    fn carry_expected_preserves_blessed_label_across_restamp() {
        // old probe was blessed; the re-stamp produced a fresh (expected:
        // None) pin with a new hash. carry_expected must restore the label.
        let mut old = BTreeMap::new();
        let mut blessed = pin("keep", "old-hash");
        blessed.expected = Some("accepted".to_owned());
        old.insert("keep".to_owned(), blessed);
        old.insert("vanished".to_owned(), pin("vanished", "x"));

        let mut new = BTreeMap::new();
        new.insert("keep".to_owned(), pin("keep", "new-hash")); // re-stamped
        new.insert("fresh".to_owned(), pin("fresh", "y")); // brand new

        carry_expected(&mut new, &old);

        assert_eq!(new["keep"].expected.as_deref(), Some("accepted"));
        assert_eq!(new["keep"].pine.xxh128, "new-hash"); // content still updated
        assert_eq!(new["fresh"].expected, None); // unblessed newcomer
    }

    #[test]
    fn diff_counts_added_changed_removed() {
        let mut old = BTreeMap::new();
        old.insert("keep".to_owned(), pin("keep", "11"));
        old.insert("change".to_owned(), pin("change", "11"));
        old.insert("gone".to_owned(), pin("gone", "11"));
        let mut new = BTreeMap::new();
        new.insert("keep".to_owned(), pin("keep", "11"));
        new.insert("change".to_owned(), pin("change", "22"));
        new.insert("fresh".to_owned(), pin("fresh", "33"));
        let d = Diff::compute(&old, &new);
        assert_eq!((d.added, d.changed, d.removed), (1, 1, 1));
    }

    #[test]
    fn stamp_all_skips_dirs_without_the_oracle() {
        let root =
            std::env::temp_dir().join(format!("brokkr_piners_reseed_{}", std::process::id()));
        let validation = root.join("validation");
        // A real parity probe: both files.
        let probe = validation.join("alpha-01");
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join("strategy.pine"), b"//@version=6\n").unwrap();
        std::fs::write(probe.join("tv_trades.csv"), b"a,b\n1,2\n").unwrap();
        // A multi-mode self-test: strategy.pine but no tv_trades.csv.
        let selftest = validation.join("selftest-01");
        std::fs::create_dir_all(&selftest).unwrap();
        std::fs::write(selftest.join("strategy.pine"), b"//@version=6\n").unwrap();
        std::fs::write(selftest.join("trades-mode-a.csv"), b"x\n").unwrap();
        // A container dir with nothing at its top level.
        std::fs::create_dir_all(validation.join("container/nested")).unwrap();

        let pins = stamp_all(&validation, &root).unwrap();
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(pins.len(), 1);
        assert!(pins.contains_key("alpha-01"));
    }
}
