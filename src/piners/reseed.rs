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
//! - `--reseed --probe <id>` - upsert one probe, leaving the rest intact.
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
    if args.all && args.probe.is_some() {
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

    let new_pins = if args.all {
        stamp_all(&validation, &corpus_root)?
    } else if let Some(id) = &args.probe {
        let mut merged = existing.clone();
        merged.insert(id.clone(), stamp_one(id, &corpus_root)?);
        merged
    } else {
        return Err(DevError::Config(
            "corpus --reseed requires --all (full regen) or --probe <id> \
             (single upsert)."
                .into(),
        ));
    };

    let diff = Diff::compute(&existing, &new_pins);

    std::fs::create_dir_all(&registry_dir).map_err(DevError::Io)?;
    std::fs::write(&pins_path, format_pins(&new_pins)).map_err(DevError::Io)?;

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

/// Walk `validation/` and stamp every probe directory. A subdir with
/// neither pinned file is not a probe and is skipped; a subdir with
/// exactly one is malformed and reported.
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
    let mut malformed: Vec<String> = Vec::new();
    for id in ids {
        let dir = validation.join(&id);
        let has_pine = dir.join(PINE_FILE).exists();
        let has_csv = dir.join(CSV_FILE).exists();
        if !has_pine && !has_csv {
            continue; // not a probe dir
        }
        match stamp_one(&id, corpus_root) {
            Ok(pin) => {
                pins.insert(id, pin);
            }
            Err(_) => malformed.push(id),
        }
    }

    if !malformed.is_empty() {
        return Err(DevError::Config(format!(
            "corpus --reseed --all: {} probe dir(s) missing {PINE_FILE} or {CSV_FILE}:\n  {}",
            malformed.len(),
            malformed.join("\n  ")
        )));
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

/// Render the pin set as deterministic TOML: entries sorted by id (the
/// `BTreeMap` iteration order), inline `pine`/`csv` tables with exactly
/// `path` + `xxh128`, blank line between entries, single trailing newline.
fn format_pins(pins: &BTreeMap<String, Pin>) -> String {
    let entries: Vec<String> = pins
        .iter()
        .map(|(id, pin)| {
            format!(
                "[probes.{}]\npine = {{ path = {}, xxh128 = {} }}\ncsv = {{ path = {}, xxh128 = {} }}\n",
                toml_key(id),
                toml_str(&pin.pine.path.to_string_lossy()),
                toml_str(&pin.pine.xxh128),
                toml_str(&pin.csv.path.to_string_lossy()),
                toml_str(&pin.csv.xxh128),
            )
        })
        .collect();
    // Each entry already ends in a newline; joining with one more puts a
    // blank line between entries and leaves a single trailing newline.
    entries.join("\n")
}

/// A probe id as a TOML key: bare when it is all `[A-Za-z0-9_-]`, else
/// quoted.
fn toml_key(id: &str) -> String {
    let bare = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if bare {
        id.to_owned()
    } else {
        toml_str(id)
    }
}

/// A TOML basic string with `"` and `\` escaped.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn pin(p: &str, h: &str) -> Pin {
        Pin {
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
    fn format_is_sorted_and_inline() {
        let mut pins = BTreeMap::new();
        pins.insert("zeta-01".to_owned(), pin("zeta-01", "ff"));
        pins.insert("alpha-01".to_owned(), pin("alpha-01", "aa"));
        let out = format_pins(&pins);
        // alpha sorts before zeta
        assert!(out.find("alpha-01").unwrap() < out.find("zeta-01").unwrap());
        assert!(out.contains("pine = { path = \"validation/alpha-01/strategy.pine\", xxh128 = \"aa\" }"));
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn format_round_trips_through_loader() {
        let mut pins = BTreeMap::new();
        pins.insert("p-01".to_owned(), pin("p-01", "abc123"));
        let text = format_pins(&pins);
        let parsed: BTreeMap<String, Pin> = toml::from_str::<super::ReparseProbes>(&text)
            .unwrap()
            .probes;
        assert_eq!(parsed, pins);
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
    fn toml_key_quotes_only_when_needed() {
        assert_eq!(toml_key("magnifier-tick-01"), "magnifier-tick-01");
        assert_eq!(toml_key("weird.id"), "\"weird.id\"");
    }
}

#[cfg(test)]
#[derive(serde::Deserialize)]
struct ReparseProbes {
    #[serde(default)]
    probes: BTreeMap<String, Pin>,
}
