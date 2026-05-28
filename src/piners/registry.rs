//! The piners-owned corpus registry: a canonical pin file plus
//! keyword grouping files.
//!
//! Layout under `[piners] registry_dir` (default `corpus-registry`):
//!
//! - `pins.toml` - the canonical, verified universe. One entry per probe
//!   id, each pinning `strategy.pine` (input) and `tv_trades.csv` (oracle)
//!   by path + xxh128. This is the single source of truth; `--probe`,
//!   `--all`, `--verify-only`, and the future reseed helper all operate on
//!   it alone.
//! - `<keyword>.toml` (any other `*.toml`) - a pure selection grouping:
//!   `probes = ["id", ...]`. The keyword is the file stem. Ids reference
//!   `pins.toml`; a keyword cannot introduce a probe, only group pinned
//!   ones.
//!
//! Pins carry the hash, not the keyword files, because the hash is the
//! most volatile field (it changes on every upstream re-pin); duplicating
//! it across keyword files would invite a self-contradicting registry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::DevError;
use crate::preflight;

/// File name of the canonical pin file inside the registry directory.
const PINS_FILE: &str = "pins.toml";

/// The canonical per-probe disposition labels. A probe's actual disposition
/// (and its pinned `expected`) is one of these: the four parity acceptance
/// tiers, then the four non-`parity` outcomes. This is the single unit the
/// gate compares - `count_tier` (exact/near/drift) stays diagnostic.
pub const DISPOSITION_LABELS: [&str; 8] = [
    "byte_exact",
    "accepted",
    "actionable_drift",
    "count_divergent",
    "compile_fail",
    "runtime_fail",
    "no_tv_data",
    "no_overlap",
];

/// True if `label` is one of [`DISPOSITION_LABELS`].
pub fn is_disposition(label: &str) -> bool {
    DISPOSITION_LABELS.contains(&label)
}

/// One pinned file: a path relative to the corpus root plus its xxh128.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FilePin {
    /// Path relative to `[piners] corpus_root`.
    pub path: PathBuf,
    /// Expected xxh128 hex digest (brokkr's standard file hash).
    pub xxh128: String,
}

/// A pinned probe: its input script, its oracle trade list, and the
/// disposition the gate holds it to.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// The blessed disposition label (one of [`DISPOSITION_LABELS`]). `None`
    /// means never blessed: the gate treats that as a hard "must bless"
    /// failure rather than passing silently. Stamped by `--bless`, preserved
    /// across `--reseed` (which touches `pine`/`csv` only).
    #[serde(default)]
    pub expected: Option<String>,
    pub pine: FilePin,
    pub csv: FilePin,
}

/// Raw `pins.toml` shape: `[probes.<id>]` tables.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinsFile {
    #[serde(default)]
    probes: BTreeMap<String, Pin>,
}

/// Raw keyword-file shape: `probes = [ids]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeywordFile {
    #[serde(default)]
    probes: Vec<String>,
}

/// The loaded registry: the pinned universe plus the keyword index.
#[derive(Debug, Default)]
pub struct Registry {
    /// Canonical pins, keyed by probe id.
    pub pins: BTreeMap<String, Pin>,
    /// keyword -> probe ids, built from the `<keyword>.toml` files.
    pub keywords: BTreeMap<String, Vec<String>>,
}

/// Parse just `pins.toml` into the id -> [`Pin`] map. Shared by
/// [`Registry::load`] and `brokkr corpus --reseed` (which reads the
/// existing pins to compute its added/changed/removed diff and to merge a
/// single `--probe` upsert).
pub fn load_pins(pins_path: &Path) -> Result<BTreeMap<String, Pin>, DevError> {
    let text = std::fs::read_to_string(pins_path).map_err(|e| {
        DevError::Config(format!("piners: failed to read {}: {e}", pins_path.display()))
    })?;
    let parsed: PinsFile = toml::from_str(&text)
        .map_err(|e| DevError::Config(format!("piners: {}: {e}", pins_path.display())))?;
    Ok(parsed.probes)
}

impl Registry {
    /// Load `pins.toml` and every sibling `<keyword>.toml` from
    /// `registry_dir`. Does not touch the corpus; call
    /// [`Registry::lint`] (and per-probe verification) for that.
    pub fn load(registry_dir: &Path) -> Result<Self, DevError> {
        if !registry_dir.is_dir() {
            return Err(DevError::Config(format!(
                "piners: registry directory not found: {}",
                registry_dir.display()
            )));
        }

        let pins = load_pins(&registry_dir.join(PINS_FILE))?;

        let mut keywords = BTreeMap::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(registry_dir)
            .map_err(DevError::Io)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for path in entries {
            if path.file_name().and_then(|n| n.to_str()) == Some(PINS_FILE) {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let text = std::fs::read_to_string(&path).map_err(|e| {
                DevError::Config(format!(
                    "piners: failed to read keyword file {}: {e}",
                    path.display()
                ))
            })?;
            let kw: KeywordFile = toml::from_str(&text).map_err(|e| {
                DevError::Config(format!("piners: {}: {e}", path.display()))
            })?;
            keywords.insert(stem.to_owned(), kw.probes);
        }

        Ok(Self { pins, keywords })
    }

    /// Structural lint: every id referenced by a keyword file must exist
    /// in `pins.toml`, and every pinned `expected` must be a known
    /// disposition label. A keyword pointing at an unknown id means the
    /// registry is lying about what is selectable; an unknown `expected`
    /// means the gate could never be satisfied.
    pub fn lint(&self) -> Result<(), DevError> {
        let mut dangling: Vec<String> = Vec::new();
        for (keyword, ids) in &self.keywords {
            for id in ids {
                if !self.pins.contains_key(id) {
                    dangling.push(format!("{keyword}.toml -> {id}"));
                }
            }
        }
        let mut bad_expected: Vec<String> = Vec::new();
        for (id, pin) in &self.pins {
            if let Some(exp) = &pin.expected
                && !is_disposition(exp)
            {
                bad_expected.push(format!("{id} -> expected = \"{exp}\""));
            }
        }
        let mut errs: Vec<String> = Vec::new();
        if !dangling.is_empty() {
            errs.push(format!(
                "keyword file(s) reference ids absent from {PINS_FILE}:\n  {}",
                dangling.join("\n  ")
            ));
        }
        if !bad_expected.is_empty() {
            errs.push(format!(
                "pin(s) carry an unknown `expected` label (must be one of {}):\n  {}",
                DISPOSITION_LABELS.join(", "),
                bad_expected.join("\n  ")
            ));
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(DevError::Config(format!("piners: {}", errs.join("\n"))))
        }
    }

    /// All keywords that contain `id`, sorted. Used for manifest
    /// provenance regardless of how the probe was selected.
    pub fn keywords_for(&self, id: &str) -> Vec<String> {
        self.keywords
            .iter()
            .filter(|(_, ids)| ids.iter().any(|p| p == id))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Sorted list of keyword names for error/help messages.
    pub fn keyword_names(&self) -> Vec<&str> {
        self.keywords.keys().map(String::as_str).collect()
    }
}

/// A pinned probe whose files are confirmed present and hash-matched.
///
/// Paths are kept **relative to `corpus_root`** (exactly as pinned in
/// `pins.toml`) so the manifest can be expressed against a single
/// top-level `corpus_root` the harness already knows how to resolve.
#[derive(Debug, Clone)]
pub struct VerifiedProbe {
    pub id: String,
    pub pine_rel: PathBuf,
    pub pine_xxh128: String,
    pub csv_rel: PathBuf,
    pub csv_xxh128: String,
}

/// Resolve and hard-verify a single pinned probe against `corpus_root`.
///
/// A missing file or a hash mismatch is a hard error: either the registry
/// is lying or the submodule drifted under us. Reuses
/// [`preflight::verify_file_hash`] (xxh128, mtime-cached) so the digest
/// matches the rest of brokkr.
pub fn verify_probe(
    id: &str,
    pin: &Pin,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<VerifiedProbe, DevError> {
    verify_one(id, "strategy.pine", &pin.pine, corpus_root, project_root)?;
    verify_one(id, "tv_trades.csv", &pin.csv, corpus_root, project_root)?;
    Ok(VerifiedProbe {
        id: id.to_owned(),
        pine_rel: pin.pine.path.clone(),
        pine_xxh128: pin.pine.xxh128.clone(),
        csv_rel: pin.csv.path.clone(),
        csv_xxh128: pin.csv.xxh128.clone(),
    })
}

fn verify_one(
    id: &str,
    label: &str,
    file: &FilePin,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let abs = corpus_root.join(&file.path);
    if !abs.exists() {
        return Err(DevError::Preflight(vec![format!(
            "piners: probe '{id}' pins a {label} path that is missing from the corpus:\n  {}\n  (registry is lying or the submodule drifted)",
            abs.display()
        )]));
    }
    let origin = format!("probe {id} ({label})");
    preflight::verify_file_hash(&abs, &file.xxh128, project_root, Some(&origin))?;
    Ok(())
}

/// Render a pin set as deterministic `pins.toml`: entries sorted by id (the
/// `BTreeMap` order), an `expected` line first when blessed, then inline
/// `pine`/`csv = { path, xxh128 }` tables. Blank line between entries, single
/// trailing newline. Shared by `--reseed` and `--bless`, the two writers.
pub fn serialize_pins(pins: &BTreeMap<String, Pin>) -> String {
    let entries: Vec<String> = pins
        .iter()
        .map(|(id, pin)| {
            let mut s = format!("[probes.{}]\n", toml_key(id));
            if let Some(exp) = &pin.expected {
                s.push_str(&format!("expected = {}\n", toml_str(exp)));
            }
            s.push_str(&format!(
                "pine = {{ path = {}, xxh128 = {} }}\ncsv = {{ path = {}, xxh128 = {} }}\n",
                toml_str(&pin.pine.path.to_string_lossy()),
                toml_str(&pin.pine.xxh128),
                toml_str(&pin.csv.path.to_string_lossy()),
                toml_str(&pin.csv.xxh128),
            ));
            s
        })
        .collect();
    // Each entry ends in a newline; joining with one more puts a blank line
    // between entries and leaves a single trailing newline.
    entries.join("\n")
}

/// A probe id as a TOML key: bare when it is all `[A-Za-z0-9_-]`, else quoted.
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

    fn registry_with(keyword_ids: &[(&str, &[&str])], pin_ids: &[&str]) -> Registry {
        let mut pins = BTreeMap::new();
        for id in pin_ids {
            pins.insert(
                (*id).to_owned(),
                Pin {
                    expected: None,
                    pine: FilePin {
                        path: PathBuf::from(format!("validation/{id}/strategy.pine")),
                        xxh128: "00".into(),
                    },
                    csv: FilePin {
                        path: PathBuf::from(format!("validation/{id}/tv_trades.csv")),
                        xxh128: "11".into(),
                    },
                },
            );
        }
        let mut keywords = BTreeMap::new();
        for (k, ids) in keyword_ids {
            keywords.insert(
                (*k).to_owned(),
                ids.iter().map(|s| (*s).to_owned()).collect(),
            );
        }
        Registry { pins, keywords }
    }

    #[test]
    fn lint_passes_when_keyword_ids_are_pinned() {
        let r = registry_with(&[("k", &["a", "b"])], &["a", "b", "c"]);
        assert!(r.lint().is_ok());
    }

    #[test]
    fn lint_fails_on_dangling_keyword_id() {
        let r = registry_with(&[("k", &["a", "ghost"])], &["a"]);
        let err = r.lint().unwrap_err();
        assert!(format!("{err:?}").contains("ghost"));
    }

    #[test]
    fn keywords_for_returns_sorted_membership() {
        let r = registry_with(&[("y", &["a"]), ("x", &["a"]), ("z", &["b"])], &["a", "b"]);
        assert_eq!(r.keywords_for("a"), vec!["x".to_owned(), "y".to_owned()]);
    }

    #[test]
    fn load_parses_pins_and_keyword_files() {
        let dir = std::env::temp_dir().join(format!("brokkr_piners_reg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pins.toml"),
            r#"
[probes.alpha-01]
pine = { path = "validation/alpha-01/strategy.pine", xxh128 = "aaa" }
csv  = { path = "validation/alpha-01/tv_trades.csv", xxh128 = "bbb" }

[probes.beta-02]
pine = { path = "validation/beta-02/strategy.pine", xxh128 = "ccc" }
csv  = { path = "validation/beta-02/tv_trades.csv", xxh128 = "ddd" }
"#,
        )
        .unwrap();
        std::fs::write(dir.join("ema.toml"), "probes = [\"alpha-01\"]\n").unwrap();

        let r = Registry::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(r.pins.len(), 2);
        assert_eq!(r.pins["alpha-01"].pine.xxh128, "aaa");
        assert_eq!(r.keywords["ema"], vec!["alpha-01".to_owned()]);
        assert!(r.lint().is_ok());
    }

    #[test]
    fn lint_fails_on_unknown_expected_label() {
        let mut r = registry_with(&[], &["a"]);
        r.pins.get_mut("a").unwrap().expected = Some("totally-bogus".to_owned());
        let err = r.lint().unwrap_err();
        assert!(format!("{err:?}").contains("totally-bogus"));
    }

    #[test]
    fn serialize_pins_emits_expected_and_round_trips() {
        let mut r = registry_with(&[], &["alpha-01"]);
        r.pins.get_mut("alpha-01").unwrap().expected = Some("accepted".to_owned());
        let text = serialize_pins(&r.pins);
        assert!(text.contains("expected = \"accepted\""));
        // expected precedes pine within the entry.
        assert!(text.find("expected").unwrap() < text.find("pine").unwrap());
        let reparsed = load_pins_str(&text);
        assert_eq!(reparsed["alpha-01"].expected.as_deref(), Some("accepted"));
        assert_eq!(reparsed["alpha-01"].pine.xxh128, "00");
    }

    fn load_pins_str(text: &str) -> BTreeMap<String, Pin> {
        toml::from_str::<PinsFile>(text).unwrap().probes
    }

    #[test]
    fn toml_key_quotes_only_when_needed() {
        assert_eq!(toml_key("magnifier-tick-01"), "magnifier-tick-01");
        assert_eq!(toml_key("weird.id"), "\"weird.id\"");
    }
}
