//! The piners-owned corpus registry: a canonical pin file plus
//! keyword grouping files.
//!
//! Layout under `[piners] registry_dir` (default `corpus-registry`):
//!
//! - `pins.toml` - the canonical, verified universe. One entry per probe
//!   id, each pinning `strategy.pine` (input) and `tv_trades.csv` (oracle)
//!   by path + xxh128, plus three top-level tables: `[feeds.<name>]`
//!   (hash-pinned OHLCV feed groups - the feed is part of a probe's oracle
//!   identity now that universes with different feeds coexist), `[roots]`
//!   (root-prefix -> feed assignments consumed by reseed), and
//!   `[probes.<id>]`. This is the single source of truth; `--probe`,
//!   `--all`, `--verify-only`, and reseed all operate on it alone.
//! - `<keyword>.toml` (any other `*.toml`) - a pure selection grouping:
//!   `probes = ["id", ...]`. The keyword is the file stem. Ids reference
//!   `pins.toml`; a keyword cannot introduce a probe, only group pinned
//!   ones.
//!
//! Pins carry the hash, not the keyword files, because the hash is the
//! most volatile field (it changes on every upstream re-pin); duplicating
//! it across keyword files would invite a self-contradicting registry.
//! Feeds live in `pins.toml` for the same reason - keyword files stay
//! trivially shaped and the volatile fields stay in one place.

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

/// One hash-pinned OHLCV feed group: the primary feed plus optional
/// warmup/lower companions. A probe's TV export was taken against one
/// specific feed, so the feed files are pinned oracles like `pine`/`csv` -
/// the same pine + csv against the wrong feed gates as a fake regression.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeedGroup {
    pub primary: FilePin,
    #[serde(default)]
    pub warmup: Option<FilePin>,
    #[serde(default)]
    pub lower: Option<FilePin>,
}

impl FeedGroup {
    /// The present (role, pin) pairs, primary first.
    pub fn roles(&self) -> Vec<(&'static str, &FilePin)> {
        let mut roles = vec![("primary", &self.primary)];
        if let Some(w) = &self.warmup {
            roles.push(("warmup", w));
        }
        if let Some(l) = &self.lower {
            roles.push(("lower", l));
        }
        roles
    }
}

/// One `[roots]` entry: the feed group reseed assigns to newly discovered
/// probes under this corpus-root-relative prefix (longest prefix wins; an
/// existing explicit `feed` on a pin is preserved).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootEntry {
    pub feed: String,
}

/// A pinned probe: its input script, its oracle trade list, the
/// disposition the gate holds it to, and the optional per-probe overrides
/// that flow into the manifest.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// The blessed disposition label (one of [`DISPOSITION_LABELS`]). `None`
    /// means never blessed: the gate treats that as a hard "must bless"
    /// failure rather than passing silently. Stamped by `--bless`, preserved
    /// across `--reseed` (which touches `pine`/`csv` only).
    #[serde(default)]
    pub expected: Option<String>,
    /// Name of the `[feeds.<name>]` group this probe's TV export was taken
    /// against. Assigned by reseed via `[roots]` (explicit value preserved).
    #[serde(default)]
    pub feed: Option<String>,
    /// Override for the harness's scan bar cap (harness-side default stays
    /// 10,000). Lives next to `expected` deliberately: changing a budget
    /// changes the disposition contract and warrants a re-bless, reviewed
    /// in the same `git diff pins.toml`. Hand-edited, reseed-preserved.
    #[serde(default)]
    pub bar_budget: Option<u64>,
    /// Piners-side OHLCV start override (epoch ms) for vendor probes whose
    /// in-submodule `inputs.json` cannot carry it. Probe-local `inputs.json`
    /// keeps precedence. Hand-edited, reseed-preserved.
    #[serde(default)]
    pub ohlcv_start_ms: Option<i64>,
    /// Piners-side `tv_trades.csv` timezone override; same carve-out rules
    /// as `ohlcv_start_ms`. Hand-edited, reseed-preserved.
    #[serde(default)]
    pub tv_trades_csv_tz: Option<String>,
    pub pine: FilePin,
    pub csv: FilePin,
}

impl Pin {
    /// A content-only pin: both files, no `expected`, no feed, no overrides.
    pub fn new(pine: FilePin, csv: FilePin) -> Self {
        Self {
            expected: None,
            feed: None,
            bar_budget: None,
            ohlcv_start_ms: None,
            tv_trades_csv_tz: None,
            pine,
            csv,
        }
    }
}

/// The full `pins.toml` shape: `[feeds.<name>]` + `[roots]` +
/// `[probes.<id>]`. Public because reseed loads and rewrites the whole file
/// (feed hashes re-stamped, `[roots]` preserved verbatim).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinsData {
    #[serde(default)]
    pub feeds: BTreeMap<String, FeedGroup>,
    #[serde(default)]
    pub roots: BTreeMap<String, RootEntry>,
    #[serde(default)]
    pub probes: BTreeMap<String, Pin>,
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
    /// Hash-pinned feed groups, keyed by group name.
    pub feeds: BTreeMap<String, FeedGroup>,
    /// Root-prefix -> feed assignments (reseed's input; kept loaded so
    /// lint can validate them).
    pub roots: BTreeMap<String, RootEntry>,
    /// keyword -> probe ids, built from the `<keyword>.toml` files.
    pub keywords: BTreeMap<String, Vec<String>>,
}

/// Parse `pins.toml` into its full [`PinsData`]. Shared by
/// [`Registry::load`] and `brokkr corpus --reseed` (which reads the
/// existing file to compute its added/changed/removed diff, merge a single
/// `--probe` upsert, and round-trip `[feeds]`/`[roots]`).
pub fn load_pins(pins_path: &Path) -> Result<PinsData, DevError> {
    let text = std::fs::read_to_string(pins_path).map_err(|e| {
        DevError::Config(format!("piners: failed to read {}: {e}", pins_path.display()))
    })?;
    toml::from_str(&text)
        .map_err(|e| DevError::Config(format!("piners: {}: {e}", pins_path.display())))
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

        let data = load_pins(&registry_dir.join(PINS_FILE))?;

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

        Ok(Self {
            pins: data.probes,
            feeds: data.feeds,
            roots: data.roots,
            keywords,
        })
    }

    /// Structural lint: every id referenced by a keyword file must exist
    /// in `pins.toml`, every pinned `expected` must be a known disposition
    /// label, and every `feed` reference (on a pin or a `[roots]` entry)
    /// must name a `[feeds]` group. A keyword pointing at an unknown id
    /// means the registry is lying about what is selectable; an unknown
    /// `expected` means the gate could never be satisfied; an unknown feed
    /// means verification could never cover the probe's oracle feed.
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
        let mut bad_feed: Vec<String> = Vec::new();
        for (id, pin) in &self.pins {
            if let Some(feed) = &pin.feed
                && !self.feeds.contains_key(feed)
            {
                bad_feed.push(format!("{id} -> feed = \"{feed}\""));
            }
        }
        for (prefix, root) in &self.roots {
            if !self.feeds.contains_key(&root.feed) {
                bad_feed.push(format!("[roots] {prefix} -> feed = \"{}\"", root.feed));
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
        if !bad_feed.is_empty() {
            errs.push(format!(
                "feed reference(s) name a group absent from [feeds]:\n  {}",
                bad_feed.join("\n  ")
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
/// is lying or the corpus drifted under us. Reuses
/// [`preflight::verify_file_hash`] (xxh128, mtime-cached) so the digest
/// matches the rest of brokkr.
pub fn verify_probe(
    id: &str,
    pin: &Pin,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<VerifiedProbe, DevError> {
    let subject = format!("probe '{id}'");
    verify_one(&subject, "strategy.pine", &pin.pine, corpus_root, project_root)?;
    verify_one(&subject, "tv_trades.csv", &pin.csv, corpus_root, project_root)?;
    Ok(VerifiedProbe {
        id: id.to_owned(),
        pine_rel: pin.pine.path.clone(),
        pine_xxh128: pin.pine.xxh128.clone(),
        csv_rel: pin.csv.path.clone(),
        csv_xxh128: pin.csv.xxh128.clone(),
    })
}

/// Hard-verify a feed group's files against `corpus_root`, same
/// no-bypass policy as [`verify_probe`]: the feed is part of the oracle
/// identity of every probe that references the group.
pub fn verify_feed_group(
    name: &str,
    group: &FeedGroup,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let subject = format!("feed group '{name}'");
    for (role, pin) in group.roles() {
        verify_one(&subject, role, pin, corpus_root, project_root)?;
    }
    Ok(())
}

fn verify_one(
    subject: &str,
    label: &str,
    file: &FilePin,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let abs = corpus_root.join(&file.path);
    if !abs.exists() {
        return Err(DevError::Preflight(vec![format!(
            "piners: {subject} pins a {label} path that is missing from the corpus:\n  {}\n  (registry is lying or the corpus drifted)",
            abs.display()
        )]));
    }
    let origin = format!("{subject} ({label})");
    preflight::verify_file_hash(&abs, &file.xxh128, project_root, Some(&origin))?;
    Ok(())
}

/// Render the full pin file as deterministic `pins.toml`: `[feeds.<name>]`
/// tables first, then the `[roots]` table, then `[probes.<id>]` entries -
/// each section sorted by key (the `BTreeMap` order). Probe entries put the
/// hand-maintained contract fields (`expected`, `feed`, the overrides)
/// before the volatile `pine`/`csv` hashes. Blank line between blocks,
/// single trailing newline. Shared by `--reseed` and `--bless`, the two
/// writers.
pub fn serialize_pins(
    feeds: &BTreeMap<String, FeedGroup>,
    roots: &BTreeMap<String, RootEntry>,
    probes: &BTreeMap<String, Pin>,
) -> String {
    let mut blocks: Vec<String> = Vec::new();

    for (name, group) in feeds {
        let mut s = format!("[feeds.{}]\n", toml_key(name));
        for (role, pin) in group.roles() {
            s.push_str(&format!("{role} = {}\n", file_pin_inline(pin)));
        }
        blocks.push(s);
    }

    if !roots.is_empty() {
        let mut s = String::from("[roots]\n");
        for (prefix, root) in roots {
            s.push_str(&format!(
                "{} = {{ feed = {} }}\n",
                toml_key(prefix),
                toml_str(&root.feed)
            ));
        }
        blocks.push(s);
    }

    for (id, pin) in probes {
        let mut s = format!("[probes.{}]\n", toml_key(id));
        if let Some(exp) = &pin.expected {
            s.push_str(&format!("expected = {}\n", toml_str(exp)));
        }
        if let Some(feed) = &pin.feed {
            s.push_str(&format!("feed = {}\n", toml_str(feed)));
        }
        if let Some(budget) = pin.bar_budget {
            s.push_str(&format!("bar_budget = {budget}\n"));
        }
        if let Some(start) = pin.ohlcv_start_ms {
            s.push_str(&format!("ohlcv_start_ms = {start}\n"));
        }
        if let Some(tz) = &pin.tv_trades_csv_tz {
            s.push_str(&format!("tv_trades_csv_tz = {}\n", toml_str(tz)));
        }
        s.push_str(&format!(
            "pine = {}\ncsv = {}\n",
            file_pin_inline(&pin.pine),
            file_pin_inline(&pin.csv),
        ));
        blocks.push(s);
    }

    // Each block ends in a newline; joining with one more puts a blank line
    // between blocks and leaves a single trailing newline.
    blocks.join("\n")
}

/// A [`FilePin`] as an inline TOML table.
fn file_pin_inline(pin: &FilePin) -> String {
    format!(
        "{{ path = {}, xxh128 = {} }}",
        toml_str(&pin.path.to_string_lossy()),
        toml_str(&pin.xxh128),
    )
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
                Pin::new(
                    FilePin {
                        path: PathBuf::from(format!("validation/{id}/strategy.pine")),
                        xxh128: "00".into(),
                    },
                    FilePin {
                        path: PathBuf::from(format!("validation/{id}/tv_trades.csv")),
                        xxh128: "11".into(),
                    },
                ),
            );
        }
        let mut keywords = BTreeMap::new();
        for (k, ids) in keyword_ids {
            keywords.insert(
                (*k).to_owned(),
                ids.iter().map(|s| (*s).to_owned()).collect(),
            );
        }
        Registry {
            pins,
            feeds: BTreeMap::new(),
            roots: BTreeMap::new(),
            keywords,
        }
    }

    fn feed_group(primary: &str) -> FeedGroup {
        FeedGroup {
            primary: FilePin {
                path: PathBuf::from(primary),
                xxh128: "ff".into(),
            },
            warmup: None,
            lower: None,
        }
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
    fn lint_fails_on_unknown_pin_feed() {
        let mut r = registry_with(&[], &["a"]);
        r.pins.get_mut("a").unwrap().feed = Some("nope".to_owned());
        let err = r.lint().unwrap_err();
        assert!(format!("{err:?}").contains("feed = \\\"nope\\\""));
    }

    #[test]
    fn lint_fails_on_unknown_root_feed() {
        let mut r = registry_with(&[], &["a"]);
        r.roots.insert(
            "vendor/x".to_owned(),
            RootEntry {
                feed: "ghost-feed".to_owned(),
            },
        );
        let err = r.lint().unwrap_err();
        assert!(format!("{err:?}").contains("ghost-feed"));
    }

    #[test]
    fn lint_passes_when_feed_references_resolve() {
        let mut r = registry_with(&[], &["a"]);
        r.feeds.insert("f1".to_owned(), feed_group("data/p.csv"));
        r.pins.get_mut("a").unwrap().feed = Some("f1".to_owned());
        r.roots
            .insert("vendor/x".to_owned(), RootEntry { feed: "f1".to_owned() });
        assert!(r.lint().is_ok());
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
[feeds.eth-15m]
primary = { path = "vendor/engine/data/15m.csv", xxh128 = "f0" }
warmup  = { path = "vendor/engine/data/15m_warmup.csv", xxh128 = "f1" }

[roots]
"vendor/engine" = { feed = "eth-15m" }

[probes.alpha-01]
feed = "eth-15m"
bar_budget = 38000
pine = { path = "vendor/engine/validation/alpha-01/strategy.pine", xxh128 = "aaa" }
csv  = { path = "vendor/engine/validation/alpha-01/tv_trades.csv", xxh128 = "bbb" }

[probes.beta-02]
ohlcv_start_ms = 1700000000000
tv_trades_csv_tz = "America/New_York"
pine = { path = "piners/beta-02/strategy.pine", xxh128 = "ccc" }
csv  = { path = "piners/beta-02/tv_trades.csv", xxh128 = "ddd" }
"#,
        )
        .unwrap();
        std::fs::write(dir.join("ema.toml"), "probes = [\"alpha-01\"]\n").unwrap();

        let r = Registry::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(r.pins.len(), 2);
        assert_eq!(r.pins["alpha-01"].pine.xxh128, "aaa");
        assert_eq!(r.pins["alpha-01"].feed.as_deref(), Some("eth-15m"));
        assert_eq!(r.pins["alpha-01"].bar_budget, Some(38000));
        assert_eq!(r.pins["beta-02"].ohlcv_start_ms, Some(1_700_000_000_000));
        assert_eq!(
            r.pins["beta-02"].tv_trades_csv_tz.as_deref(),
            Some("America/New_York")
        );
        assert_eq!(r.feeds["eth-15m"].primary.xxh128, "f0");
        assert_eq!(r.feeds["eth-15m"].warmup.as_ref().unwrap().xxh128, "f1");
        assert_eq!(r.roots["vendor/engine"].feed, "eth-15m");
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
        let text = serialize_pins(&r.feeds, &r.roots, &r.pins);
        assert!(text.contains("expected = \"accepted\""));
        // expected precedes pine within the entry.
        assert!(text.find("expected").unwrap() < text.find("pine").unwrap());
        let reparsed = load_pins_str(&text);
        assert_eq!(reparsed.probes["alpha-01"].expected.as_deref(), Some("accepted"));
        assert_eq!(reparsed.probes["alpha-01"].pine.xxh128, "00");
    }

    #[test]
    fn serialize_pins_round_trips_feeds_roots_and_overrides() {
        let mut r = registry_with(&[], &["alpha-01"]);
        let mut group = feed_group("vendor/engine/data/15m.csv");
        group.lower = Some(FilePin {
            path: PathBuf::from("vendor/engine/data/1m.csv"),
            xxh128: "f2".into(),
        });
        r.feeds.insert("eth-15m".to_owned(), group);
        r.roots.insert(
            "vendor/engine".to_owned(),
            RootEntry {
                feed: "eth-15m".to_owned(),
            },
        );
        let pin = r.pins.get_mut("alpha-01").unwrap();
        pin.feed = Some("eth-15m".to_owned());
        pin.bar_budget = Some(38000);
        pin.ohlcv_start_ms = Some(1_700_000_000_000);
        pin.tv_trades_csv_tz = Some("America/New_York".to_owned());

        let text = serialize_pins(&r.feeds, &r.roots, &r.pins);
        // sections in order: feeds, roots, probes.
        assert!(text.find("[feeds.eth-15m]").unwrap() < text.find("[roots]").unwrap());
        assert!(text.find("[roots]").unwrap() < text.find("[probes.alpha-01]").unwrap());

        let reparsed = load_pins_str(&text);
        assert_eq!(reparsed.feeds["eth-15m"].lower.as_ref().unwrap().xxh128, "f2");
        assert_eq!(reparsed.roots["vendor/engine"].feed, "eth-15m");
        let p = &reparsed.probes["alpha-01"];
        assert_eq!(p.feed.as_deref(), Some("eth-15m"));
        assert_eq!(p.bar_budget, Some(38000));
        assert_eq!(p.ohlcv_start_ms, Some(1_700_000_000_000));
        assert_eq!(p.tv_trades_csv_tz.as_deref(), Some("America/New_York"));
    }

    fn load_pins_str(text: &str) -> PinsData {
        toml::from_str::<PinsData>(text).unwrap()
    }

    #[test]
    fn toml_key_quotes_only_when_needed() {
        assert_eq!(toml_key("magnifier-tick-01"), "magnifier-tick-01");
        assert_eq!(toml_key("weird.id"), "\"weird.id\"");
        assert_eq!(toml_key("vendor/engine"), "\"vendor/engine\"");
    }
}
