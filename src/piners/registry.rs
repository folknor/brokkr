//! The piners-owned corpus registry: a canonical pin file plus
//! keyword grouping files.
//!
//! Layout under `[piners] registry_dir` (default `corpus-registry`):
//!
//! - `pins.toml` - the canonical, verified universe. One entry per probe
//!   id, each pinning `strategy.pine` (input) and its TradingView oracle -
//!   a `tv_trades.csv` export, a `tv_record.json` tvr capture, or both - by
//!   path + xxh128, plus three top-level tables: `[feeds.<name>]`
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

/// A probe's input script.
pub const PINE_FILE: &str = "strategy.pine";
/// The TradingView "List of Trades" CSV export oracle.
pub const CSV_FILE: &str = "tv_trades.csv";
/// The tvr capture oracle: TradingView's in-memory strategy report at full
/// precision. When a probe dir carries one, the harness judges against it and
/// never reads a `tv_trades.csv` beside it.
pub const RECORD_FILE: &str = "tv_record.json";
/// A probe's declared inputs (input-panel overrides, window, timezone,
/// candle, syminfo block). Optional, but it moves the verdict whenever
/// present, so it is pinned like the oracle.
pub const INPUTS_FILE: &str = "inputs.json";

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

/// One hash-pinned OHLCV feed group. A probe's TV oracle was taken against
/// one specific feed, so the feed files are pinned like the probe's own
/// files - the same script and oracle against the wrong feed gates as a fake
/// regression.
///
/// Two mutually exclusive forms, selected per group:
///
/// - [`FeedGroup::Roles`] (legacy role form): a chart-timeframe `primary`
///   feed plus optional `warmup`/`lower` companions, consumed by the harness
///   as-is (`primary` is already at chart TF).
/// - [`FeedGroup::Base`] (single-base form): the only committed input is a
///   lower-timeframe `base` feed the harness aggregates locally to the chart
///   timeframe (and uses directly as the magnifier/lower source). No separate
///   `lower` is meaningful - the base *is* the 1m feed.
///
/// The form travels into the manifest verbatim (role names as keys), and the
/// bumped manifest version lets the harness tell a base group it must
/// aggregate from role feeds it consumes directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedGroup {
    /// The legacy role form: `primary` (chart TF) plus optional companions.
    Roles {
        primary: FilePin,
        warmup: Option<FilePin>,
        lower: Option<FilePin>,
    },
    /// The single-base form: one lower-TF `base` feed, aggregated by consumers.
    Base { base: FilePin },
}

/// Raw `[feeds.<name>]` shape before form validation: all four keys optional,
/// unknown keys rejected. [`FeedGroup`]'s `Deserialize` reduces this to one of
/// the two legal forms, so a typo or an illegal mix errors at parse time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedGroupRaw {
    #[serde(default)]
    primary: Option<FilePin>,
    #[serde(default)]
    warmup: Option<FilePin>,
    #[serde(default)]
    lower: Option<FilePin>,
    #[serde(default)]
    base: Option<FilePin>,
}

impl FeedGroupRaw {
    /// Reduce the raw table to a validated [`FeedGroup`], or a message naming
    /// what is wrong (fed through `serde::de::Error::custom`).
    fn into_group(self) -> Result<FeedGroup, String> {
        match (self.base, self.primary) {
            (Some(base), None) => {
                if self.warmup.is_some() || self.lower.is_some() {
                    return Err(
                        "a base feed group must not also set `warmup`/`lower`: its only \
                         input is the `base` feed, which consumers aggregate and also use \
                         directly as the lower/magnifier source"
                            .to_owned(),
                    );
                }
                Ok(FeedGroup::Base { base })
            }
            (None, Some(primary)) => Ok(FeedGroup::Roles {
                primary,
                warmup: self.warmup,
                lower: self.lower,
            }),
            (Some(_), Some(_)) => Err(
                "a feed group sets both `base` and `primary`; pick one form - `base` (a \
                 single lower-TF feed the consumer aggregates) or `primary`/`warmup`/`lower` \
                 (chart-TF role feeds consumed as-is)"
                    .to_owned(),
            ),
            (None, None) => Err(
                "a feed group must set either `base` (single-base form) or `primary` (role \
                 form)"
                    .to_owned(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for FeedGroup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        FeedGroupRaw::deserialize(deserializer)?
            .into_group()
            .map_err(serde::de::Error::custom)
    }
}

impl FeedGroup {
    /// The present (role, pin) pairs. For a role group: `primary` first, then
    /// any `warmup`/`lower`. For a base group: the single `base` role. This is
    /// the shared iteration surface for verification, re-stamping, and the
    /// manifest - all of which treat a feed group as its set of named roles.
    pub fn roles(&self) -> Vec<(&'static str, &FilePin)> {
        match self {
            FeedGroup::Roles {
                primary,
                warmup,
                lower,
            } => {
                let mut roles = vec![("primary", primary)];
                if let Some(w) = warmup {
                    roles.push(("warmup", w));
                }
                if let Some(l) = lower {
                    roles.push(("lower", l));
                }
                roles
            }
            FeedGroup::Base { base } => vec![("base", base)],
        }
    }

    /// [`FeedGroup::roles`] with mutable pins, for re-stamping hashes in place.
    pub fn roles_mut(&mut self) -> Vec<(&'static str, &mut FilePin)> {
        match self {
            FeedGroup::Roles {
                primary,
                warmup,
                lower,
            } => {
                let mut roles: Vec<(&'static str, &mut FilePin)> = vec![("primary", primary)];
                if let Some(w) = warmup {
                    roles.push(("warmup", w));
                }
                if let Some(l) = lower {
                    roles.push(("lower", l));
                }
                roles
            }
            FeedGroup::Base { base } => vec![("base", base)],
        }
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
///
/// The oracle is `csv`, `record`, or both; [`parse_pins`] refuses a pin with
/// neither. Both are pinned when both are on disk, even though the harness
/// reads only the record then: pins mirror the probe dir, and a pinned CSV
/// stays verified for the day the record is removed.
///
/// Every pinned file sits in one probe dir under its fixed name, because
/// the harness reads `<probe_dir>/<name>` and nothing else: a pin naming any
/// other path would verify one file while the harness read another.
/// [`parse_pins`] enforces that too.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// The blessed disposition label (one of [`DISPOSITION_LABELS`]). `None`
    /// means never blessed: the gate treats that as a hard "must bless"
    /// failure rather than passing silently. Stamped by `--bless`, preserved
    /// across `--reseed` (which touches the pinned files only).
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
    /// as `ohlcv_start_ms`. Hand-edited, reseed-preserved - except that a pin
    /// with a `record` may not carry it: the harness then never reads the
    /// CSV, so the override would be dead.
    #[serde(default)]
    pub tv_trades_csv_tz: Option<String>,
    pub pine: FilePin,
    /// The probe's `inputs.json`, when the probe dir carries one.
    #[serde(default)]
    pub inputs: Option<FilePin>,
    /// The `tv_trades.csv` export oracle, when the probe dir carries one.
    #[serde(default)]
    pub csv: Option<FilePin>,
    /// The `tv_record.json` tvr capture oracle, when the probe dir carries
    /// one. Outranks `csv` harness-side.
    #[serde(default)]
    pub record: Option<FilePin>,
}

/// A probe's pinned files, as reseed stamps them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFiles {
    pub pine: FilePin,
    pub inputs: Option<FilePin>,
    pub csv: Option<FilePin>,
    pub record: Option<FilePin>,
}

impl Pin {
    /// A content-only pin: the pinned files, no `expected`, no feed, no
    /// overrides. [`parse_pins`] - which every writer runs on its output -
    /// is what refuses a pin with no oracle.
    pub fn content(files: ProbeFiles) -> Self {
        Self {
            expected: None,
            feed: None,
            bar_budget: None,
            ohlcv_start_ms: None,
            tv_trades_csv_tz: None,
            pine: files.pine,
            inputs: files.inputs,
            csv: files.csv,
            record: files.record,
        }
    }

    /// A content-only CSV-oracle pin, the common test shape.
    #[cfg(test)]
    pub fn new(pine: FilePin, csv: FilePin) -> Self {
        Self::content(ProbeFiles {
            pine,
            inputs: None,
            csv: Some(csv),
            record: None,
        })
    }

    /// The probe dir, relative to `corpus_root`: where the harness reads
    /// every probe file from.
    pub fn probe_dir(&self) -> &Path {
        self.pine.path.parent().unwrap_or_else(|| Path::new(""))
    }

    /// The optional pinned files, each with the fixed name the harness
    /// reads it by.
    pub fn optional_files(&self) -> [(&'static str, Option<&FilePin>); 3] {
        [
            (INPUTS_FILE, self.inputs.as_ref()),
            (CSV_FILE, self.csv.as_ref()),
            (RECORD_FILE, self.record.as_ref()),
        ]
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
    parse_pins(&text, pins_path)
}

/// Parse `pins.toml` text already in hand, and hold every pin to the
/// structural rules in [`check_pins`]. The writers run this on their own
/// output before replacing the file, so neither can write one the next load
/// refuses.
pub fn parse_pins(text: &str, origin: &Path) -> Result<PinsData, DevError> {
    let data = parse_pins_unchecked(text, origin)?;
    check_pins(&data.probes)
        .map_err(|e| DevError::Config(format!("piners: {}: {e}", origin.display())))?;
    Ok(data)
}

/// [`parse_pins`] without the structural rules: the TOML shape only. For
/// reseed's read of the file it is about to regenerate, so a hand edit that
/// broke a rule is repaired by the regen instead of blocking it. The
/// regenerated text still goes through [`parse_pins`] before it is written.
pub fn parse_pins_unchecked(text: &str, origin: &Path) -> Result<PinsData, DevError> {
    toml::from_str(text)
        .map_err(|e| DevError::Config(format!("piners: {}: {e}", origin.display())))
}

/// The structural rules every pin obeys: at least one oracle, every pinned
/// file in the probe dir under its fixed name, and no `tv_trades_csv_tz`
/// beside a `record` (the harness never reads the CSV then, so the override
/// would be dead). Returns every violation, one per line.
fn check_pins(probes: &BTreeMap<String, Pin>) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    for (id, pin) in probes {
        if pin.csv.is_none() && pin.record.is_none() {
            problems.push(format!(
                "{id}: pins no oracle - needs `csv` ({CSV_FILE}), `record` ({RECORD_FILE}), \
                 or both"
            ));
        }
        let dir = pin.probe_dir();
        let pine_want = dir.join(PINE_FILE);
        if pin.pine.path != pine_want {
            problems.push(format!(
                "{id}: `pine` must be named {PINE_FILE}, got {}",
                pin.pine.path.display()
            ));
        }
        for (name, file) in pin.optional_files() {
            let Some(file) = file else { continue };
            let want = dir.join(name);
            if file.path != want {
                problems.push(format!(
                    "{id}: pins {} but the harness reads {} (every probe file sits \
                     beside {PINE_FILE} under its fixed name)",
                    file.path.display(),
                    want.display()
                ));
            }
        }
        if pin.record.is_some() && pin.tv_trades_csv_tz.is_some() {
            problems.push(format!(
                "{id}: `tv_trades_csv_tz` is dead beside a `record` (the harness judges \
                 against {RECORD_FILE} and never reads {CSV_FILE}); remove it"
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("invalid probe pin(s):\n  {}", problems.join("\n  ")))
    }
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
    pub pine: FilePin,
    pub inputs: Option<FilePin>,
    pub csv: Option<FilePin>,
    pub record: Option<FilePin>,
}

/// Resolve and hard-verify a single pinned probe against `corpus_root`:
/// the script and every other file the pin declares.
///
/// A missing file or a hash mismatch is a hard error: either the registry
/// is lying or the corpus drifted under us. Reuses
/// [`preflight::verify_file_hash`] (xxh128, mtime-cached) so the digest
/// matches the rest of brokkr.
///
/// Two more refusals, both about what the harness will actually do:
///
/// - A `tv_record.json` or `inputs.json` in the probe dir that the pin does
///   not declare. The harness reads both by presence alone - the record
///   outranks the CSV, the inputs move the verdict - so an unpinned one
///   would steer the run unverified while verification vouched for the rest.
///   (An unpinned `tv_trades.csv` is harmless: it is read only as the
///   oracle, and a pin with no `csv` has a `record`, which outranks it.)
/// - A pin with no `feed`. The harness requires one per probe and would
///   abort the whole run over it, not just this probe.
pub fn verify_probe(
    id: &str,
    pin: &Pin,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<VerifiedProbe, DevError> {
    let subject = format!("probe '{id}'");
    if pin.feed.is_none() {
        return Err(DevError::Preflight(vec![format!(
            "piners: {subject} pins no `feed`; the harness needs the [feeds] group its \
             oracle was taken against. Set `feed` on the pin, or add a [roots] prefix \
             covering {} and re-run `brokkr corpus --reseed --probe {id}`",
            pin.probe_dir().display()
        )]));
    }
    verify_one(&subject, PINE_FILE, &pin.pine, corpus_root, project_root)?;
    let dir = corpus_root.join(pin.probe_dir());
    for (name, file) in pin.optional_files() {
        match file {
            Some(file) => verify_one(&subject, name, file, corpus_root, project_root)?,
            None if name != CSV_FILE && dir.join(name).exists() => {
                return Err(DevError::Preflight(vec![format!(
                    "piners: {subject} has a {name} its pin does not declare:\n  {}\n  \
                     (the harness would read it unverified; pin it with \
                     `brokkr corpus --reseed --probe {id}`)",
                    dir.join(name).display()
                )]));
            }
            None => {}
        }
    }
    Ok(VerifiedProbe {
        id: id.to_owned(),
        pine: pin.pine.clone(),
        inputs: pin.inputs.clone(),
        csv: pin.csv.clone(),
        record: pin.record.clone(),
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
    // Refuse an unmaterialized Git-LFS pointer before hashing: hashing the
    // pointer bytes would compare a 134-byte stub against the real feed's
    // digest (a spurious mismatch), or - worse, on reseed - stamp the pointer
    // hash into the pin. Cheap sniff, no-op for plaintext files.
    crate::piners::lfs::ensure_materialized(&abs)?;
    let origin = format!("{subject} ({label})");
    preflight::verify_file_hash(&abs, &file.xxh128, project_root, Some(&origin))?;
    Ok(())
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
        FeedGroup::Roles {
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
        let dir = crate::test_scratch::scratch("piners_registry", "load");
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

        assert_eq!(r.pins.len(), 2);
        assert_eq!(r.pins["alpha-01"].pine.xxh128, "aaa");
        assert_eq!(r.pins["alpha-01"].feed.as_deref(), Some("eth-15m"));
        assert_eq!(r.pins["alpha-01"].bar_budget, Some(38000));
        assert_eq!(r.pins["beta-02"].ohlcv_start_ms, Some(1_700_000_000_000));
        assert_eq!(
            r.pins["beta-02"].tv_trades_csv_tz.as_deref(),
            Some("America/New_York")
        );
        let roles = r.feeds["eth-15m"].roles();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].0, "primary");
        assert_eq!(roles[0].1.xxh128, "f0");
        assert_eq!(roles[1].0, "warmup");
        assert_eq!(roles[1].1.xxh128, "f1");
        assert_eq!(r.roots["vendor/engine"].feed, "eth-15m");
        assert_eq!(r.keywords["ema"], vec!["alpha-01".to_owned()]);
        assert!(r.lint().is_ok());
    }

    #[test]
    fn parses_single_base_feed_group() {
        let data: PinsData = toml::from_str(
            r#"
[feeds.eth-15m-2025]
base = { path = "vendor/engine/data/ohlcv_1m.csv", xxh128 = "b0" }
"#,
        )
        .unwrap();
        match &data.feeds["eth-15m-2025"] {
            FeedGroup::Base { base } => assert_eq!(base.xxh128, "b0"),
            other => panic!("expected a base group, got {other:?}"),
        }
        // A base group exposes exactly the `base` role for verify/manifest.
        let roles = data.feeds["eth-15m-2025"].roles();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].0, "base");
    }

    #[test]
    fn base_and_primary_together_is_rejected() {
        let err = toml::from_str::<PinsData>(
            r#"
[feeds.bad]
base = { path = "a.csv", xxh128 = "b0" }
primary = { path = "b.csv", xxh128 = "f0" }
"#,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("both `base` and `primary`"));
    }

    #[test]
    fn base_with_lower_is_rejected() {
        let err = toml::from_str::<PinsData>(
            r#"
[feeds.bad]
base = { path = "a.csv", xxh128 = "b0" }
lower = { path = "l.csv", xxh128 = "f0" }
"#,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("must not also set"));
    }

    #[test]
    fn empty_feed_group_is_rejected() {
        let err = toml::from_str::<PinsData>("[feeds.bad]\n").unwrap_err();
        assert!(format!("{err}").contains("either `base`"));
    }

    #[test]
    fn parses_record_only_and_dual_oracle_pins() {
        let data = parse_pins(
            r#"
[probes.rec-only]
pine = { path = "p/rec-only/strategy.pine", xxh128 = "aa" }
record = { path = "p/rec-only/tv_record.json", xxh128 = "bb" }

[probes.both]
pine = { path = "p/both/strategy.pine", xxh128 = "aa" }
csv = { path = "p/both/tv_trades.csv", xxh128 = "cc" }
record = { path = "p/both/tv_record.json", xxh128 = "dd" }
"#,
            Path::new("pins.toml"),
        )
        .unwrap();
        let rec_only = &data.probes["rec-only"];
        assert!(rec_only.csv.is_none());
        assert_eq!(rec_only.record.as_ref().unwrap().xxh128, "bb");
        let both = &data.probes["both"];
        assert_eq!(both.csv.as_ref().unwrap().xxh128, "cc");
        assert_eq!(both.record.as_ref().unwrap().xxh128, "dd");
    }

    #[test]
    fn pin_without_any_oracle_is_rejected() {
        let err = parse_pins(
            r#"
[probes.bare]
pine = { path = "p/bare/strategy.pine", xxh128 = "aa" }
"#,
            Path::new("pins.toml"),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("bare: pins no oracle"));
        assert!(msg.contains("bare"));
    }

    /// Write `root/rel_dir/name` and return its pin (path relative to `root`).
    fn write_file(root: &Path, rel_dir: &Path, name: &str, bytes: &[u8]) -> FilePin {
        let rel = rel_dir.join(name);
        let abs = root.join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, bytes).unwrap();
        FilePin {
            path: rel,
            xxh128: preflight::compute_xxh128(&abs).unwrap(),
        }
    }

    /// A feed-bearing pin over `files`, the shape `verify_probe` accepts.
    fn fed(files: ProbeFiles) -> Pin {
        let mut pin = Pin::content(files);
        pin.feed = Some("f".to_owned());
        pin
    }

    #[test]
    fn verify_checks_the_record_and_refuses_an_unpinned_one() {
        let root = crate::test_scratch::scratch("piners_registry", "verify_record");
        let rel = Path::new("p/probe");
        let pine = write_file(&root, rel, PINE_FILE, b"//@version=6\n");
        let csv = write_file(&root, rel, CSV_FILE, b"a,b\n");
        let pin = fed(ProbeFiles {
            pine,
            inputs: None,
            csv: Some(csv),
            record: None,
        });

        // A CSV-only pin verifies while no record sits beside it.
        assert!(verify_probe("probe", &pin, &root, &root).is_ok());

        // A record appears on disk: the CSV-only pin is now refused.
        let record = write_file(&root, rel, RECORD_FILE, b"{}\n");
        let err = verify_probe("probe", &pin, &root, &root).unwrap_err();
        assert!(format!("{err:?}").contains("tv_record.json its pin does not declare"));

        // Pinned, it verifies and travels in the result.
        let mut with_record = pin.clone();
        with_record.record = Some(record.clone());
        let v = verify_probe("probe", &with_record, &root, &root).unwrap();
        assert_eq!(v.record.as_ref().unwrap().xxh128, record.xxh128);

        // Record drift is caught like any other pinned file.
        let mut drifted = with_record;
        drifted.record = Some(FilePin {
            path: record.path,
            xxh128: "0".repeat(32),
        });
        let err = verify_probe("probe", &drifted, &root, &root).unwrap_err();
        assert!(format!("{err:?}").contains("hash mismatch"));
    }

    #[test]
    fn verify_accepts_a_record_only_probe() {
        let root = crate::test_scratch::scratch("piners_registry", "verify_record_only");
        let rel = Path::new("p/rec");
        let pin = fed(ProbeFiles {
            pine: write_file(&root, rel, PINE_FILE, b"//@version=6\n"),
            inputs: None,
            csv: None,
            record: Some(write_file(&root, rel, RECORD_FILE, b"{}\n")),
        });
        let v = verify_probe("rec", &pin, &root, &root).unwrap();
        assert!(v.csv.is_none());
        assert!(v.record.is_some());

        // A CSV turning up beside a record-only pin is harmless (the record
        // outranks it), so it is not refused.
        write_file(&root, rel, CSV_FILE, b"a,b\n");
        assert!(verify_probe("rec", &pin, &root, &root).is_ok());
    }

    #[test]
    fn verify_pins_inputs_and_refuses_an_unpinned_one() {
        let root = crate::test_scratch::scratch("piners_registry", "verify_inputs");
        let rel = Path::new("p/inp");
        let mut pin = fed(ProbeFiles {
            pine: write_file(&root, rel, PINE_FILE, b"//@version=6\n"),
            inputs: None,
            csv: Some(write_file(&root, rel, CSV_FILE, b"a,b\n")),
            record: None,
        });
        let inputs = write_file(&root, rel, INPUTS_FILE, b"{\"Source\":\"high\"}\n");
        let err = verify_probe("inp", &pin, &root, &root).unwrap_err();
        assert!(format!("{err:?}").contains("inputs.json its pin does not declare"));

        pin.inputs = Some(inputs.clone());
        let v = verify_probe("inp", &pin, &root, &root).unwrap();
        assert_eq!(v.inputs.unwrap().xxh128, inputs.xxh128);

        write_file(&root, rel, INPUTS_FILE, b"{\"Source\":\"low\"}\n");
        let err = verify_probe("inp", &pin, &root, &root).unwrap_err();
        assert!(format!("{err:?}").contains("hash mismatch"));
    }

    #[test]
    fn verify_refuses_a_feedless_pin() {
        let root = crate::test_scratch::scratch("piners_registry", "verify_feedless");
        let rel = Path::new("p/nofeed");
        let mut pin = fed(ProbeFiles {
            pine: write_file(&root, rel, PINE_FILE, b"//@version=6\n"),
            inputs: None,
            csv: Some(write_file(&root, rel, CSV_FILE, b"a,b\n")),
            record: None,
        });
        pin.feed = None;
        let err = verify_probe("nofeed", &pin, &root, &root).unwrap_err();
        assert!(format!("{err:?}").contains("pins no `feed`"));
    }

    #[test]
    fn a_pinned_file_off_its_fixed_path_is_rejected() {
        let err = parse_pins(
            r#"
[probes.moved]
pine = { path = "p/moved/strategy.pine", xxh128 = "aa" }
record = { path = "elsewhere/tv_record.json", xxh128 = "bb" }

[probes.renamed]
pine = { path = "p/renamed/main.pine", xxh128 = "aa" }
csv = { path = "p/renamed/tv_trades.csv", xxh128 = "cc" }
"#,
            Path::new("pins.toml"),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("moved: pins elsewhere/tv_record.json"));
        assert!(msg.contains("renamed: `pine` must be named strategy.pine"));
    }

    #[test]
    fn csv_timezone_beside_a_record_is_rejected() {
        let err = parse_pins(
            r#"
[probes.dead-tz]
tv_trades_csv_tz = "utc"
pine = { path = "p/dead-tz/strategy.pine", xxh128 = "aa" }
record = { path = "p/dead-tz/tv_record.json", xxh128 = "bb" }
"#,
            Path::new("pins.toml"),
        )
        .unwrap_err();
        assert!(format!("{err:?}").contains("`tv_trades_csv_tz` is dead"));
    }

    #[test]
    fn lint_fails_on_unknown_expected_label() {
        let mut r = registry_with(&[], &["a"]);
        r.pins.get_mut("a").unwrap().expected = Some("totally-bogus".to_owned());
        let err = r.lint().unwrap_err();
        assert!(format!("{err:?}").contains("totally-bogus"));
    }

}
