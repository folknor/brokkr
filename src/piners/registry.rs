//! The piners-owned corpus registry: a canonical pin file plus
//! keyword grouping files.
//!
//! Layout under `[piners] registry_dir` (default `corpus-registry`):
//!
//! - `pins.toml` - the canonical, verified universe. One entry per probe
//!   id, each pinning `strategy.pine` (input) and its TradingView oracle -
//!   a `tv_trades.csv` export, a `tv_record.json` tvr capture, or both - by
//!   path + xxh128, plus four top-level tables: `[feeds.<name>]`
//!   (hash-pinned OHLCV feed groups - the feed is part of a probe's oracle
//!   identity now that universes with different feeds coexist),
//!   `[harness_files.<name>]` (any other file the harness reads that moves a
//!   verdict - see [`PinsData::harness_files`]),
//!   `[probe_config."<prefix>"]` (the hand-declared execution facts - feed,
//!   bar budget, start, CSV timezone - scoped by directory prefix), and
//!   `[probes.<id>]`. This is the single source of truth; `--probe`,
//!   `--all`, `--verify-only`, and reseed all operate on it alone.
//!
//! The split between `[probe_config]` and `[probes]` is by owner. A probe
//! entry is machine-stamped: reseed writes its hashes, bless its `expected`,
//! and reseed drops it when the probe's directory vanishes. The execution
//! facts used to live on that entry too, so losing it - a vanished-then-
//! returned probe, a hand-deleted entry - reset them: the next reseed
//! re-derived the feed from a directory default and forgot the budget and
//! start, and the probe then ran against the wrong feed with a plausible,
//! wrong verdict. Declared by prefix, outside any writer's reach, they
//! survive whatever happens to the entry. [`resolve`] is the one resolver.
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

/// One `[harness_files.<name>]` entry: a [`FilePin`] whose hash may be
/// absent. A hand-declared entry names only its `path`, and reseed stamps
/// the hash; loading refuses one still unstamped, so nothing ever runs
/// against an unverified file.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HarnessFile {
    /// Path relative to `[piners] corpus_root`.
    pub path: PathBuf,
    /// Expected xxh128 hex digest; `None` until reseed stamps it.
    #[serde(default)]
    pub xxh128: Option<String>,
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

/// A probe's execution facts: one `[probe_config."<prefix>"]` declaration,
/// or - out of [`resolve`] - the facts a probe actually runs under.
///
/// Every field is optional in a declaration and resolved independently: a
/// probe takes each field from the longest prefix covering its directory that
/// sets it. So a root declares the shared feed and budget, and one probe dir
/// beneath it declares only the start it differs by, without restating (and
/// later silently freezing) the root's values. Absent after resolution means
/// "the harness default", which is not the same as declaring that default's
/// value: an explicit declaration survives a harness-default change.
///
/// Precedence against the probe's own `inputs.json` is the harness's and is
/// field-specific: a resolved `ohlcv_start_ms` outranks an `inputs.json` start
/// (a registry start corrects an upstream window guess), while an
/// `inputs.json` timezone outranks a resolved `tv_trades_csv_tz`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    /// Name of the `[feeds.<name>]` group the probe's oracle was taken
    /// against. Every pinned probe must resolve one.
    #[serde(default)]
    pub feed: Option<String>,
    /// The harness's scan bar cap (harness default 10,000). Changing it
    /// changes the disposition contract, which the gate then enforces.
    #[serde(default)]
    pub bar_budget: Option<u64>,
    /// The execution start (epoch ms). Outranks an `inputs.json` start.
    #[serde(default)]
    pub ohlcv_start_ms: Option<i64>,
    /// The `tv_trades.csv` timezone. An `inputs.json` timezone outranks it,
    /// and a probe with a `record` may not resolve one: the harness then
    /// never reads the CSV, so the value would be dead.
    #[serde(default)]
    pub tv_trades_csv_tz: Option<String>,
}

/// The [`ProbeConfig`] field names, in declaration order. Field-generic
/// checks walk these through [`ProbeConfig::setting`].
const CONFIG_FIELDS: [&str; 4] = ["feed", "bar_budget", "ohlcv_start_ms", "tv_trades_csv_tz"];

/// One field's declared value, comparable across the field types.
#[derive(Debug, PartialEq, Eq)]
enum Setting<'a> {
    Text(&'a str),
    Int(i128),
}

impl ProbeConfig {
    /// The value of field `i` (an index into [`CONFIG_FIELDS`]).
    fn setting(&self, i: usize) -> Option<Setting<'_>> {
        match i {
            0 => self.feed.as_deref().map(Setting::Text),
            1 => self.bar_budget.map(|v| Setting::Int(i128::from(v))),
            2 => self.ohlcv_start_ms.map(|v| Setting::Int(i128::from(v))),
            3 => self.tv_trades_csv_tz.as_deref().map(Setting::Text),
            _ => None,
        }
    }

    fn is_empty(&self) -> bool {
        (0..CONFIG_FIELDS.len()).all(|i| self.setting(i).is_none())
    }
}

/// The declarations covering `dir`, most specific first. Coverage is by
/// whole path components (`piners/foo` does not cover `piners/foobar`), and
/// it is structural: nothing here looks at the filesystem, so a declaration
/// over an uninitialized submodule still covers the probes pinned there.
fn covering<'a>(
    config: &'a BTreeMap<String, ProbeConfig>,
    dir: &Path,
) -> Vec<(&'a str, &'a ProbeConfig)> {
    let mut chain: Vec<(&str, &ProbeConfig)> = config
        .iter()
        .filter(|(prefix, _)| dir.starts_with(Path::new(prefix.as_str())))
        .map(|(prefix, entry)| (prefix.as_str(), entry))
        .collect();
    chain.sort_by_key(|(prefix, _)| std::cmp::Reverse(Path::new(prefix).components().count()));
    chain
}

/// Resolve the execution facts of the probe in `dir` (relative to
/// `corpus_root`): each field from the longest covering prefix that sets it.
/// The one resolver - the manifest, verification and lint all ask it.
pub fn resolve(config: &BTreeMap<String, ProbeConfig>, dir: &Path) -> ProbeConfig {
    let chain = covering(config, dir);
    ProbeConfig {
        feed: chain.iter().find_map(|(_, c)| c.feed.clone()),
        bar_budget: chain.iter().find_map(|(_, c)| c.bar_budget),
        ohlcv_start_ms: chain.iter().find_map(|(_, c)| c.ohlcv_start_ms),
        tv_trades_csv_tz: chain.iter().find_map(|(_, c)| c.tv_trades_csv_tz.clone()),
    }
}

/// A pinned probe: its input script, its oracle trade list, and the
/// disposition the gate holds it to. Its execution facts are not here - they
/// are declared by prefix in `[probe_config]` (see the module header).
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
    /// A content-only pin: the pinned files, no `expected`. [`parse_pins`] -
    /// which every writer runs on its output - is what refuses a pin with no
    /// oracle.
    pub fn content(files: ProbeFiles) -> Self {
        Self {
            expected: None,
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

/// The full `pins.toml` shape: `[feeds.<name>]`, `[harness_files.<name>]`,
/// `[probe_config."<prefix>"]` and `[probes.<id>]`. Public because reseed
/// loads and rewrites the whole file (feed and harness-file hashes
/// re-stamped, `[probe_config]` preserved verbatim).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinsData {
    #[serde(default)]
    pub feeds: BTreeMap<String, FeedGroup>,
    /// Files the harness reads beyond the probe dirs and the feeds - a
    /// piners-owned facts table, say - keyed by a name of the project's
    /// choosing. brokkr knows nothing of their contents; it only holds them
    /// to their pinned hash before every run, whatever the selection, since
    /// the harness may consult one for any probe. Without this a file that
    /// moves verdicts sits outside the content gate, the same hole an
    /// unpinned `inputs.json` would be. The harness keeps finding the file
    /// by its own path; the pin names that path so the two agree.
    #[serde(default)]
    pub harness_files: BTreeMap<String, HarnessFile>,
    #[serde(default)]
    pub probe_config: BTreeMap<String, ProbeConfig>,
    /// Probes registered ahead of their pins: id -> probe dir relative to the
    /// corpus root, hand-written (`[pending]` / `live-02 = "piners/live-02"`).
    /// A pending probe is part of the structural universe - a `[probe_config]`
    /// declaration written for it is not stale, a keyword may list it - but it
    /// is never selected or run: it has no hashes, so nothing about it is
    /// verified. `brokkr corpus --reseed --probe <id>` moves it from here to
    /// `[probes]` in one write, so declaring several new probes and pinning
    /// them one at a time is a sequence of valid files. See [`check_pending`].
    #[serde(default)]
    pub pending: BTreeMap<String, PathBuf>,
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
    /// Hash-pinned harness files, keyed by name (see
    /// [`PinsData::harness_files`]).
    pub harness_files: BTreeMap<String, HarnessFile>,
    /// Prefix -> declared execution facts; read through [`Registry::config`].
    pub probe_config: BTreeMap<String, ProbeConfig>,
    /// Registered-but-unpinned probes (see [`PinsData::pending`]).
    pub pending: BTreeMap<String, PathBuf>,
    /// keyword -> probe ids, built from the `<keyword>.toml` files.
    pub keywords: BTreeMap<String, Vec<String>>,
    /// The `pins.toml` text this registry was parsed from. A writer that
    /// runs after the load (bless) edits this text and refuses to replace a
    /// file whose bytes have changed since: the load and the write are both
    /// under the lock, so a change means a hand edit the write would revert.
    pub pins_text: String,
}

/// Parse `pins.toml` text already in hand, and hold every pin to the
/// structural rules in [`check_pins`]. The writers run this on their own
/// output before replacing the file, so neither can write one the next load
/// refuses.
pub fn parse_pins(text: &str, origin: &Path) -> Result<PinsData, DevError> {
    let data = parse_pins_unchecked(text, origin)?;
    let mut problems = check_pins(&data.probes);
    problems.extend(check_harness_files(&data.harness_files));
    problems.extend(check_pending(&data.pending, &data.probes));
    problems.extend(check_probe_config(&data.probe_config, &data.probes, &data.pending));
    if !problems.is_empty() {
        return Err(DevError::Config(format!(
            "piners: {}: invalid registry:\n  {}",
            origin.display(),
            problems.join("\n  ")
        )));
    }
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

/// The structural rules every pin obeys: at least one oracle, a probe dir
/// below the corpus root, and every pinned file in that dir under its fixed
/// name. Returns every violation, one per line.
fn check_pins(probes: &BTreeMap<String, Pin>) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    for (id, pin) in probes {
        if pin.csv.is_none() && pin.record.is_none() {
            problems.push(format!(
                "{id}: pins no oracle - needs `csv` ({CSV_FILE}), `record` ({RECORD_FILE}), \
                 or both"
            ));
        }
        let dir = pin.probe_dir();
        // `components()` quietly drops an internal `.`, so the rebuilt spelling
        // is compared with the written one, as `check_probe_config` does.
        let plain: Option<Vec<String>> = dir
            .components()
            .map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        let below_root = plain.is_some_and(|parts| {
            !parts.is_empty() && parts.join("/") == dir.to_string_lossy()
        });
        if !below_root {
            // Pin paths are relative to the corpus root and name a directory
            // below it: reseed never discovers a probe anywhere else, and no
            // `[probe_config]` prefix (plain relative components) could cover
            // one at the root, outside it, or behind a `..`.
            problems.push(format!(
                "{id}: pins {} - a probe is a directory below the corpus root, named by \
                 plain relative components",
                pin.pine.path.display()
            ));
        }
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
    }
    problems
}

/// The rules every `[harness_files]` entry obeys: a path below the corpus
/// root in plain relative components, so the file verified is the one under
/// the root the harness was handed and no `..` can reach outside it; and a
/// stamped hash. Reseed reads the file without these rules, which is how a
/// hand-declared, path-only entry gets its hash.
fn check_harness_files(files: &BTreeMap<String, HarnessFile>) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, file) in files {
        let plain = file
            .path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !plain || file.path.as_os_str().is_empty() {
            problems.push(format!(
                "[harness_files.{name}]: pins {} - a harness file is a path below the \
                 corpus root, named by plain relative components",
                file.path.display()
            ));
        } else if file.xxh128.is_none() {
            problems.push(format!(
                "[harness_files.{name}]: no `xxh128` - stamp it with `brokkr corpus \
                 --reseed --all` (or `--probe <id>`; every reseed re-stamps the harness \
                 files)"
            ));
        }
    }
    problems
}

/// The rules `[pending]` obeys: each entry must be a registration a reseed
/// can complete, so none of them names a probe `--reseed --probe <id>` could
/// never discover at that path.
///
/// - The path is a directory below the corpus root in canonical plain
///   components, none starting with `.` - discovery skips dot-dirs.
/// - The id is the path's basename: discovery derives ids that way.
/// - The id is not also pinned, and no two entries share a path.
/// - No pending path is equal to, inside, or above another pending path or a
///   pinned probe's dir: discovery stops at a probe dir, so of two nested
///   probe dirs only the outer one is ever found.
///
/// That the path lies outside the registry dir, and that the probe is
/// actually on disk, are the reseed's to check - it is the one place that
/// walks the corpus, and it refuses a pending entry it cannot find.
fn check_pending(pending: &BTreeMap<String, PathBuf>, probes: &BTreeMap<String, Pin>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut dirs: Vec<(&str, &Path)> = probes.iter().map(|(id, p)| (id.as_str(), p.probe_dir())).collect();
    for (id, path) in pending {
        let parts: Option<Vec<String>> = path
            .components()
            .map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        let canonical = parts.as_ref().is_some_and(|parts| {
            !parts.is_empty()
                && parts.join("/") == path.to_string_lossy()
                && parts.iter().all(|p| !p.starts_with('.'))
        });
        if !canonical {
            problems.push(format!(
                "[pending] {id} = {}: a pending probe is a directory below the corpus root, \
                 named by plain relative components joined by `/`, none starting with `.`",
                path.display()
            ));
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) != Some(id.as_str()) {
            problems.push(format!(
                "[pending] {id} = {}: the id must be the directory's name (reseed derives ids \
                 from it, so this entry could never be pinned)",
                path.display()
            ));
        }
        if probes.contains_key(id) {
            problems.push(format!(
                "[pending] {id}: already pinned in [probes] - remove the pending entry"
            ));
        }
        for (other, dir) in &dirs {
            if path.starts_with(dir) || dir.starts_with(path) {
                problems.push(format!(
                    "[pending] {id} = {}: {} {other}'s directory {} - discovery stops at a \
                     probe directory, so nested probe dirs cannot both be pinned",
                    path.display(),
                    if path == dir { "is" } else { "nests with" },
                    dir.display()
                ));
            }
        }
        dirs.push((id.as_str(), path.as_path()));
    }
    problems
}

/// The one `[pending]` rule the file alone cannot settle: a pending path at
/// or inside the registry dir names a probe discovery never walks into, so
/// no reseed could ever pin it. Run by [`Registry::load`] and on reseed's
/// output, so a writer cannot produce a file the next load refuses.
pub fn check_pending_location(
    pending: &BTreeMap<String, PathBuf>,
    pins_path: &Path,
    registry_dir: &Path,
    corpus_root: &Path,
) -> Result<(), DevError> {
    let in_registry: Vec<String> = pending
        .iter()
        .filter(|(_, dir)| corpus_root.join(dir).starts_with(registry_dir))
        .map(|(id, dir)| format!("[pending] {id} = {}", dir.display()))
        .collect();
    if in_registry.is_empty() {
        return Ok(());
    }
    Err(DevError::Config(format!(
        "piners: {}: a pending probe inside the registry dir can never be pinned (reseed \
         does not walk {}):\n  {}",
        pins_path.display(),
        registry_dir.display(),
        in_registry.join("\n  ")
    )))
}

/// The rules `[probe_config]` obeys, over the declarations and the pinned
/// universe (pinned plus [pending](PinsData::pending)) together. Returns
/// every violation, one per line.
///
/// - A prefix is a relative directory path in canonical spelling - plain
///   components joined by `/`, no `.`/`..`, no leading or trailing or doubled
///   `/` - so two keys can never name one directory.
/// - A declaration sets at least one field.
/// - Every declared field wins for at least one pinned or pending probe, i.e.
///   some probe resolves it from this prefix - so a declaration written ahead
///   of its probe is valid once the probe is registered in `[pending]`, and
///   pinning one of several pending probes leaves the others' declarations
///   valid. A declaration covering no probe, or
///   one entirely shadowed by narrower ones, is stale: it reads as a fact
///   about the corpus while governing nothing. Coverage is structural (the
///   pinned paths), so an uninitialized submodule does not make it stale.
/// - No declared field equals what the prefix would inherit from its
///   ancestors. A redundant copy is not harmless: when the ancestor changes,
///   the copy silently keeps the old value.
/// - No probe with a `record` resolves a `tv_trades_csv_tz`: the harness then
///   judges against the record and never reads the CSV, so the value is dead.
///   A pending probe's oracle kind is unknown until it is pinned, so this one
///   is settled by the reseed that pins it (whose output passes through here).
///
/// Whether every probe resolves a feed, and whether that feed exists, is
/// [`Registry::lint`]'s: the writers run this check on their output, and a
/// freshly bootstrapped file has probes and no declarations yet.
fn check_probe_config(
    config: &BTreeMap<String, ProbeConfig>,
    probes: &BTreeMap<String, Pin>,
    pending: &BTreeMap<String, PathBuf>,
) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    for (prefix, entry) in config {
        let canonical = Path::new(prefix)
            .components()
            .map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Option<Vec<String>>>()
            .map(|parts| parts.join("/"));
        if canonical.as_deref() != Some(prefix.as_str()) || prefix.is_empty() {
            problems.push(format!(
                "[probe_config.\"{prefix}\"]: a prefix is a relative directory path, \
                 components joined by a single `/` (no `.`, `..` or stray slashes)"
            ));
            continue;
        }
        if entry.is_empty() {
            problems.push(format!("[probe_config.\"{prefix}\"]: declares no field"));
            continue;
        }
        let inherited: BTreeMap<String, ProbeConfig> = config
            .iter()
            .filter(|(other, _)| *other != prefix)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let above = resolve(&inherited, Path::new(prefix));
        for (i, name) in CONFIG_FIELDS.iter().enumerate() {
            if let Some(value) = entry.setting(i)
                && above.setting(i) == Some(value)
            {
                problems.push(format!(
                    "[probe_config.\"{prefix}\"]: `{name}` restates the value it already \
                     inherits; remove it (a copy would silently keep the old value when \
                     the ancestor changes)"
                ));
            }
        }
    }

    // Which (prefix, field) each probe resolves from.
    let mut winners: std::collections::BTreeSet<(&str, usize)> = std::collections::BTreeSet::new();
    for (id, pin) in probes {
        let chain = covering(config, pin.probe_dir());
        for i in 0..CONFIG_FIELDS.len() {
            if let Some((prefix, _)) = chain.iter().find(|(_, c)| c.setting(i).is_some()) {
                winners.insert((prefix, i));
            }
        }
        if pin.record.is_some()
            && let Some((prefix, _)) = chain.iter().find(|(_, c)| c.tv_trades_csv_tz.is_some())
        {
            problems.push(format!(
                "{id}: resolves `tv_trades_csv_tz` from [probe_config.\"{prefix}\"], which is \
                 dead beside its {RECORD_FILE} (the harness never reads {CSV_FILE} then); \
                 declare it on prefixes covering only CSV-oracle probes"
            ));
        }
    }
    for dir in pending.values() {
        let chain = covering(config, dir);
        for i in 0..CONFIG_FIELDS.len() {
            if let Some((prefix, _)) = chain.iter().find(|(_, c)| c.setting(i).is_some()) {
                winners.insert((prefix, i));
            }
        }
    }
    for (prefix, entry) in config {
        for (i, name) in CONFIG_FIELDS.iter().enumerate() {
            if entry.setting(i).is_some() && !winners.contains(&(prefix.as_str(), i)) {
                problems.push(format!(
                    "[probe_config.\"{prefix}\"]: `{name}` governs no pinned or pending probe \
                     (it covers none, or narrower declarations override it for all it covers); \
                     remove it, or - if it was written ahead of the probe it is for - register \
                     that probe under [pending] (`<id> = \"<dir>\"`) until `brokkr corpus \
                     --reseed --probe <id>` pins it"
                ));
            }
        }
    }
    problems
}

impl Registry {
    /// Load `pins.toml` and every sibling `<keyword>.toml` from
    /// `registry_dir`. Does not touch the corpus; call
    /// [`Registry::lint`] (and per-probe verification) for that.
    ///
    /// `corpus_root` is needed for the one `[pending]` rule the file alone
    /// cannot settle: a pending path at or inside the registry dir names a
    /// probe discovery never walks into, so no reseed could ever pin it.
    pub fn load(registry_dir: &Path, corpus_root: &Path) -> Result<Self, DevError> {
        if !registry_dir.is_dir() {
            return Err(DevError::Config(format!(
                "piners: registry directory not found: {}",
                registry_dir.display()
            )));
        }

        let pins_path = registry_dir.join(PINS_FILE);
        let pins_text = std::fs::read_to_string(&pins_path).map_err(|e| {
            DevError::Config(format!("piners: failed to read {}: {e}", pins_path.display()))
        })?;
        let data = parse_pins(&pins_text, &pins_path)?;
        check_pending_location(&data.pending, &pins_path, registry_dir, corpus_root)?;

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
            harness_files: data.harness_files,
            probe_config: data.probe_config,
            pending: data.pending,
            keywords,
            pins_text,
        })
    }

    /// The execution facts pinned probe `id` runs under, resolved from
    /// `[probe_config]`; `None` when `id` is not pinned.
    pub fn config(&self, id: &str) -> Option<ProbeConfig> {
        self.pins
            .get(id)
            .map(|pin| resolve(&self.probe_config, pin.probe_dir()))
    }

    /// Structural lint: every id referenced by a keyword file must exist
    /// in `pins.toml`, every pinned `expected` must be a known disposition
    /// label, every pinned probe must resolve a feed, and every `feed` in
    /// `[probe_config]` must name a `[feeds]` group. A keyword pointing at an
    /// unknown id means the registry is lying about what is selectable; an
    /// unknown `expected` means the gate could never be satisfied; a missing
    /// or unknown feed means the harness could not run the probe against the
    /// data its oracle was taken on.
    pub fn lint(&self) -> Result<(), DevError> {
        let mut dangling: Vec<String> = Vec::new();
        for (keyword, ids) in &self.keywords {
            for id in ids {
                if !self.pins.contains_key(id) && !self.pending.contains_key(id) {
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
        for (prefix, entry) in &self.probe_config {
            if let Some(feed) = &entry.feed
                && !self.feeds.contains_key(feed)
            {
                bad_feed.push(format!("[probe_config.\"{prefix}\"] -> feed = \"{feed}\""));
            }
        }
        // Pending probes too: the feed is an execution fact, settled by the
        // declarations alone, so a pending probe that would run feedless is
        // refused now rather than at the reseed that pins it.
        let feedless: Vec<String> = self
            .pins
            .iter()
            .map(|(id, pin)| (id, pin.probe_dir()))
            .chain(self.pending.iter().map(|(id, dir)| (id, dir.as_path())))
            .filter(|(_, dir)| resolve(&self.probe_config, dir).feed.is_none())
            .map(|(id, dir)| format!("{id} ({})", dir.display()))
            .collect();
        let mut errs: Vec<String> = Vec::new();
        if !dangling.is_empty() {
            errs.push(format!(
                "keyword file(s) reference ids neither pinned nor [pending] in {PINS_FILE}:\n  {}",
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
        if !feedless.is_empty() {
            errs.push(format!(
                "probe(s) resolve no feed - declare `feed` on a [probe_config.\"<prefix>\"] \
                 covering their directory (the [feeds] group their oracle was taken \
                 against):\n  {}",
                feedless.join("\n  ")
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
    /// The execution facts the probe runs under, resolved once here so the
    /// manifest cannot resolve them a second, possibly different way.
    pub config: ProbeConfig,
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
/// - A probe resolving no `feed`. The harness requires one per probe and
///   would abort the whole run over it, not just this probe. [`Registry::lint`]
///   refuses this for the whole universe first; this is the contract check at
///   the point the manifest entry is built.
pub fn verify_probe(
    id: &str,
    pin: &Pin,
    config: ProbeConfig,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<VerifiedProbe, DevError> {
    let subject = format!("probe '{id}'");
    if config.feed.is_none() {
        return Err(DevError::Preflight(vec![format!(
            "piners: {subject} resolves no `feed`; the harness needs the [feeds] group \
             its oracle was taken against. Declare `feed` on a [probe_config.\"<prefix>\"] \
             covering {}",
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
        config,
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

/// Hard-verify every `[harness_files]` pin against `corpus_root`, same
/// no-bypass policy as the probe files. All of them, not the selection's:
/// brokkr cannot know which probes a harness file governs. Returns the
/// number verified.
pub fn verify_harness_files(
    registry: &Registry,
    corpus_root: &Path,
    project_root: &Path,
) -> Result<usize, DevError> {
    for (name, file) in &registry.harness_files {
        let subject = format!("harness file '{name}'");
        // The loader refuses an unstamped entry; this is belt-and-braces.
        let xxh128 = file.xxh128.clone().ok_or_else(|| {
            DevError::Preflight(vec![format!("piners: {subject} has no pinned xxh128")])
        })?;
        let pin = FilePin {
            path: file.path.clone(),
            xxh128,
        };
        verify_one(&subject, "file", &pin, corpus_root, project_root)?;
    }
    Ok(registry.harness_files.len())
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
    // Every pin names a file the harness reads as one. Checked before the
    // LFS sniff opens it: a FIFO there would block verification forever,
    // and a directory would hash as a tree digest nothing consumes.
    if !abs.is_file() {
        return Err(DevError::Preflight(vec![format!(
            "piners: {subject} pins a {label} path that is not a regular file:\n  {}",
            abs.display()
        )]));
    }
    // Refuse an unmaterialized Git-LFS pointer before hashing: hashing the
    // pointer bytes would compare a 134-byte stub against the real feed's
    // digest (a spurious mismatch), or - worse, on reseed - stamp the pointer
    // hash into the pin. Cheap sniff, no-op for plaintext files.
    let origin = format!("{subject} ({label})");
    crate::piners::lfs::ensure_materialized(&abs).map_err(|e| with_origin(e, &origin, &abs))?;
    preflight::verify_file_hash(&abs, &file.xxh128, project_root, Some(&origin))
        .map_err(|e| with_origin(e, &origin, &abs))?;
    Ok(())
}

/// Name the probe, feed group or harness file (and the role or file label)
/// an error on the verify path concerns. The LFS refusal names only the path
/// and a hash read failure (an I/O error from the hasher) names nothing, so
/// on a 850-probe selection neither said which pin it was about. A preflight
/// message that already carries `origin` is left as it is.
pub(crate) fn with_origin(e: DevError, origin: &str, path: &Path) -> DevError {
    match e {
        DevError::Preflight(msgs) => DevError::Preflight(
            msgs.into_iter()
                .map(|m| {
                    if m.contains(origin) {
                        m
                    } else {
                        format!("{m}\n  origin: {origin}")
                    }
                })
                .collect(),
        ),
        other => DevError::Preflight(vec![format!(
            "piners: {origin}: could not verify {}: {other}",
            path.display()
        )]),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn verify_errors_name_the_pin_they_concern() {
        let path = Path::new("/c/data/feed.csv");
        let origin = "feed group 'eth' (base)";
        // An I/O error from the hasher said nothing about which pin.
        let io = DevError::Io(std::io::Error::other("short read"));
        let msg = with_origin(io, origin, path).to_string();
        assert!(msg.contains("feed group 'eth' (base)"), "{msg}");
        assert!(msg.contains("/c/data/feed.csv"), "{msg}");
        // The LFS refusal named the path only; it gains the origin.
        let lfs = DevError::Preflight(vec!["piners: /c/data/feed.csv is a pointer".to_owned()]);
        let DevError::Preflight(m) = with_origin(lfs, origin, path) else { panic!() };
        assert!(m[0].ends_with("origin: feed group 'eth' (base)"), "{m:?}");
        // A message already carrying the origin is not doubled.
        let has = DevError::Preflight(vec![format!("mismatch\n  origin: {origin}")]);
        let DevError::Preflight(m) = with_origin(has, origin, path) else { panic!() };
        assert_eq!(m[0].matches("origin:").count(), 1);
    }

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
        let mut feeds = BTreeMap::new();
        feeds.insert("f".to_owned(), feed_group("data/p.csv"));
        let mut probe_config = BTreeMap::new();
        probe_config.insert("validation".to_owned(), feed_decl("f"));
        Registry {
            pins,
            feeds,
            harness_files: BTreeMap::new(),
            probe_config,
            keywords,
            ..Registry::default()
        }
    }

    fn feed_decl(feed: &str) -> ProbeConfig {
        ProbeConfig {
            feed: Some(feed.to_owned()),
            ..ProbeConfig::default()
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
    fn lint_fails_on_unknown_declared_feed() {
        let mut r = registry_with(&[], &["a"]);
        r.probe_config
            .insert("validation/a".to_owned(), feed_decl("ghost-feed"));
        let err = r.lint().unwrap_err();
        assert!(format!("{err:?}").contains("ghost-feed"));
    }

    #[test]
    fn lint_fails_on_a_probe_that_resolves_no_feed() {
        let mut r = registry_with(&[], &["a"]);
        r.probe_config.clear();
        let err = format!("{:?}", r.lint().unwrap_err());
        assert!(err.contains("resolve no feed"));
        assert!(err.contains("a (validation/a)"));
    }

    fn file(path: &str) -> FilePin {
        FilePin {
            path: PathBuf::from(path),
            xxh128: "00".into(),
        }
    }

    /// A CSV-oracle pin in `dir`.
    fn pin_in(dir: &str) -> Pin {
        Pin::new(
            file(&format!("{dir}/strategy.pine")),
            file(&format!("{dir}/tv_trades.csv")),
        )
    }

    fn pins_of(dirs: &[(&str, &str)]) -> BTreeMap<String, Pin> {
        dirs.iter()
            .map(|(id, dir)| ((*id).to_owned(), pin_in(dir)))
            .collect()
    }

    #[test]
    fn resolve_takes_each_field_from_its_longest_declaring_prefix() {
        let mut config = BTreeMap::new();
        config.insert(
            "vendor/bench".to_owned(),
            ProbeConfig {
                feed: Some("bench".to_owned()),
                bar_budget: Some(54000),
                ..ProbeConfig::default()
            },
        );
        config.insert(
            "vendor/bench/strategies/one".to_owned(),
            ProbeConfig {
                ohlcv_start_ms: Some(1_743_379_200_000),
                ..ProbeConfig::default()
            },
        );
        config.insert("piners".to_owned(), feed_decl("live"));

        let one = resolve(&config, Path::new("vendor/bench/strategies/one"));
        assert_eq!(one.feed.as_deref(), Some("bench")); // inherited
        assert_eq!(one.bar_budget, Some(54000)); // inherited
        assert_eq!(one.ohlcv_start_ms, Some(1_743_379_200_000)); // own
        // Whole components only: `strategies/one` does not cover `onefold`.
        let onefold = resolve(&config, Path::new("vendor/bench/strategies/onefold"));
        assert_eq!(onefold.ohlcv_start_ms, None);
        assert_eq!(onefold.bar_budget, Some(54000));
        assert_eq!(resolve(&config, Path::new("elsewhere/p")), ProbeConfig::default());
    }

    #[test]
    fn the_resolved_feed_survives_losing_the_probe_entry() {
        // The failure this layout exists for: a live capture's feed used to
        // live on its pin, so an entry lost and re-added came back on the
        // directory default. Declared by prefix, it resolves the same
        // whatever the entry's history.
        let mut config = BTreeMap::new();
        config.insert("piners".to_owned(), feed_decl("eth-15m-2025"));
        config.insert("piners/live-01".to_owned(), feed_decl("eth-15m-live"));
        let pins = pins_of(&[("live-01", "piners/live-01"), ("old-01", "piners/old-01")]);
        assert!(check_probe_config(&config, &pins, &BTreeMap::new()).is_empty());
        let fresh = Pin::content(ProbeFiles {
            pine: file("piners/live-01/strategy.pine"),
            inputs: None,
            csv: Some(file("piners/live-01/tv_trades.csv")),
            record: None,
        });
        assert_eq!(
            resolve(&config, fresh.probe_dir()).feed.as_deref(),
            Some("eth-15m-live")
        );
    }

    #[test]
    fn a_declaration_must_be_a_canonical_nonempty_prefix() {
        let pins = pins_of(&[("a", "p/a")]);
        for bad in ["p/", "./p", "p//a", "/p", "p/../p", ""] {
            let mut config = BTreeMap::new();
            config.insert(bad.to_owned(), feed_decl("f"));
            let problems = check_probe_config(&config, &pins, &BTreeMap::new());
            assert!(
                problems.iter().any(|p| p.contains("relative directory path")),
                "{bad:?} accepted: {problems:?}"
            );
        }
        let mut config = BTreeMap::new();
        config.insert("p".to_owned(), ProbeConfig::default());
        assert!(check_probe_config(&config, &pins, &BTreeMap::new())[0].contains("declares no field"));
    }

    #[test]
    fn a_declaration_written_for_a_pending_probe_loads() {
        let text = "\
[probe_config.\"p\"]
feed = \"f\"

[probe_config.\"p/new\"]
bar_budget = 10

[pending]
new = \"p/new\"

[probes.old]
pine = { path = \"p/old/strategy.pine\", xxh128 = \"aa\" }
csv = { path = \"p/old/tv_trades.csv\", xxh128 = \"bb\" }
";
        let data = parse_pins(text, Path::new("pins.toml")).unwrap();
        assert_eq!(data.pending["new"], PathBuf::from("p/new"));
        // Without the registration the same declaration is stale.
        let bare = text.replace("[pending]\nnew = \"p/new\"\n", "");
        let err = parse_pins(&bare, Path::new("pins.toml")).unwrap_err();
        assert!(format!("{err:?}").contains("governs no pinned or pending probe"));
    }

    #[test]
    fn a_pending_entry_reseed_could_never_complete_is_refused() {
        let pin = |dir: &str| {
            Pin::new(
                FilePin { path: PathBuf::from(format!("{dir}/strategy.pine")), xxh128: "aa".into() },
                FilePin { path: PathBuf::from(format!("{dir}/tv_trades.csv")), xxh128: "bb".into() },
            )
        };
        let mut probes = BTreeMap::new();
        probes.insert("old".to_owned(), pin("p/old"));
        let check = |entries: &[(&str, &str)]| {
            let pending: BTreeMap<String, PathBuf> =
                entries.iter().map(|(k, v)| ((*k).to_owned(), PathBuf::from(v))).collect();
            check_pending(&pending, &probes)
        };
        assert!(check(&[("new", "p/new")]).is_empty());
        assert!(check(&[("other", "p/new")])[0].contains("must be the directory's name"));
        assert!(check(&[("x", ".hidden/x")])[0].contains("none starting with `.`"));
        assert!(check(&[("x", "p/../x")])[0].contains("plain relative components"));
        assert!(check(&[("old", "q/old")]).iter().any(|p| p.contains("already pinned")));
        assert!(check(&[("inner", "p/old/inner")])[0].contains("nests with"));
        assert!(check(&[("a", "p/a"), ("b", "p/a/b")]).iter().any(|p| p.contains("nests with")));
    }

    #[test]
    fn a_declared_field_that_governs_no_probe_is_rejected() {
        let pins = pins_of(&[("a", "p/a"), ("b", "p/b")]);
        let mut config = BTreeMap::new();
        config.insert(
            "p".to_owned(),
            ProbeConfig {
                feed: Some("f".to_owned()),
                bar_budget: Some(54000),
                ..ProbeConfig::default()
            },
        );
        // Both probes override the budget, so the root's budget is dead while
        // its feed still governs.
        for dir in ["p/a", "p/b"] {
            config.insert(
                dir.to_owned(),
                ProbeConfig {
                    bar_budget: Some(10000),
                    ..ProbeConfig::default()
                },
            );
        }
        // And a declaration over a directory no probe is pinned in.
        config.insert("q".to_owned(), feed_decl("f"));
        let problems = check_probe_config(&config, &pins, &BTreeMap::new());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("\"p\"]: `bar_budget` governs no")));
        assert!(problems.iter().any(|p| p.contains("\"q\"]: `feed` governs no")));
    }

    #[test]
    fn a_field_restating_its_inherited_value_is_rejected() {
        let pins = pins_of(&[("a", "p/fam/a"), ("b", "p/b")]);
        let mut config = BTreeMap::new();
        config.insert("p".to_owned(), feed_decl("f"));
        // A family-level copy is as stale-prone as an exact-dir one.
        config.insert("p/fam".to_owned(), feed_decl("f"));
        let problems = check_probe_config(&config, &pins, &BTreeMap::new());
        assert!(problems.iter().any(|p| p.contains("\"p/fam\"]: `feed` restates")));
        // An explicit value where the ancestor declares none is not a
        // restatement, even when it equals the harness default.
        let mut config = BTreeMap::new();
        config.insert("p".to_owned(), feed_decl("f"));
        config.insert(
            "p/b".to_owned(),
            ProbeConfig {
                bar_budget: Some(10000),
                ..ProbeConfig::default()
            },
        );
        assert!(check_probe_config(&config, &pins, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_csv_timezone_resolving_onto_a_record_probe_is_rejected() {
        let mut pins = pins_of(&[("csv", "p/csv")]);
        let mut rec = pin_in("p/rec");
        rec.csv = None;
        rec.record = Some(file("p/rec/tv_record.json"));
        pins.insert("rec".to_owned(), rec);
        let mut config = BTreeMap::new();
        config.insert(
            "p".to_owned(),
            ProbeConfig {
                tv_trades_csv_tz: Some("utc".to_owned()),
                ..ProbeConfig::default()
            },
        );
        let problems = check_probe_config(&config, &pins, &BTreeMap::new());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("rec: resolves `tv_trades_csv_tz`"));
        // Scoped to the CSV probe alone, it is fine.
        let mut config = BTreeMap::new();
        config.insert(
            "p/csv".to_owned(),
            ProbeConfig {
                tv_trades_csv_tz: Some("utc".to_owned()),
                ..ProbeConfig::default()
            },
        );
        assert!(check_probe_config(&config, &pins, &BTreeMap::new()).is_empty());
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

[probe_config."vendor/engine"]
feed = "eth-15m"
bar_budget = 38000

[probe_config."piners/beta-02"]
feed = "eth-15m"
ohlcv_start_ms = 1700000000000
tv_trades_csv_tz = "utc_minus_5"

[probes.alpha-01]
pine = { path = "vendor/engine/validation/alpha-01/strategy.pine", xxh128 = "aaa" }
csv  = { path = "vendor/engine/validation/alpha-01/tv_trades.csv", xxh128 = "bbb" }

[probes.beta-02]
pine = { path = "piners/beta-02/strategy.pine", xxh128 = "ccc" }
csv  = { path = "piners/beta-02/tv_trades.csv", xxh128 = "ddd" }
"#,
        )
        .unwrap();
        std::fs::write(dir.join("ema.toml"), "probes = [\"alpha-01\"]\n").unwrap();

        let r = Registry::load(&dir, dir.parent().unwrap()).unwrap();

        assert_eq!(r.pins.len(), 2);
        assert_eq!(r.pins["alpha-01"].pine.xxh128, "aaa");
        let alpha = r.config("alpha-01").unwrap();
        assert_eq!(alpha.feed.as_deref(), Some("eth-15m"));
        assert_eq!(alpha.bar_budget, Some(38000));
        let beta = r.config("beta-02").unwrap();
        assert_eq!(beta.ohlcv_start_ms, Some(1_700_000_000_000));
        assert_eq!(beta.tv_trades_csv_tz.as_deref(), Some("utc_minus_5"));
        assert_eq!(beta.bar_budget, None);
        assert_eq!(r.config("ghost"), None);
        let roles = r.feeds["eth-15m"].roles();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].0, "primary");
        assert_eq!(roles[0].1.xxh128, "f0");
        assert_eq!(roles[1].0, "warmup");
        assert_eq!(roles[1].1.xxh128, "f1");
        assert_eq!(r.keywords["ema"], vec!["alpha-01".to_owned()]);
        assert!(r.lint().is_ok());
    }

    #[test]
    fn load_refuses_a_pending_probe_inside_the_registry_dir() {
        let root = crate::test_scratch::scratch("piners_registry", "pending_in_registry");
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(
            registry.join("pins.toml"),
            "[feeds.f]\nbase = { path = \"d.csv\", xxh128 = \"00\" }\n\n\
             [probe_config.\"registry\"]\nfeed = \"f\"\n\n[pending]\nnew = \"registry/new\"\n",
        )
        .unwrap();
        let err = Registry::load(&registry, &root).unwrap_err();
        assert!(format!("{err:?}").contains("can never be pinned"));
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

    fn fed(files: ProbeFiles) -> Pin {
        Pin::content(files)
    }

    /// `verify_probe` with a resolved feed, the shape every caller hands it.
    fn verify(id: &str, pin: &Pin, root: &Path) -> Result<VerifiedProbe, DevError> {
        verify_probe(id, pin, feed_decl("f"), root, root)
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
        assert!(verify("probe", &pin, &root).is_ok());

        // A record appears on disk: the CSV-only pin is now refused.
        let record = write_file(&root, rel, RECORD_FILE, b"{}\n");
        let err = verify("probe", &pin, &root).unwrap_err();
        assert!(format!("{err:?}").contains("tv_record.json its pin does not declare"));

        // Pinned, it verifies and travels in the result.
        let mut with_record = pin.clone();
        with_record.record = Some(record.clone());
        let v = verify("probe", &with_record, &root).unwrap();
        assert_eq!(v.record.as_ref().unwrap().xxh128, record.xxh128);

        // Record drift is caught like any other pinned file.
        let mut drifted = with_record;
        drifted.record = Some(FilePin {
            path: record.path,
            xxh128: "0".repeat(32),
        });
        let err = verify("probe", &drifted, &root).unwrap_err();
        assert!(format!("{err:?}").contains("hash mismatch"));
    }

    #[test]
    fn verify_holds_every_harness_file_to_its_pin() {
        let root = crate::test_scratch::scratch("piners_registry", "verify_harness_files");
        let facts = write_file(&root, Path::new("facts"), "probe-facts.toml", b"[probes]\n");
        let mut registry = Registry::default();
        registry.harness_files.insert(
            "probe-facts".to_owned(),
            HarnessFile {
                path: facts.path.clone(),
                xxh128: Some(facts.xxh128.clone()),
            },
        );
        assert_eq!(verify_harness_files(&registry, &root, &root).unwrap(), 1);

        // An edit the pin does not know about is refused before any run.
        std::fs::write(root.join(&facts.path), b"[probes.x]\ncapture_origin_ms = 1\n").unwrap();
        let err = verify_harness_files(&registry, &root, &root).unwrap_err();
        assert!(format!("{err:?}").contains("harness file 'probe-facts'"));
    }

    #[test]
    fn a_harness_file_outside_the_corpus_root_is_refused() {
        let text = "[harness_files.facts]\npath = \"../facts.toml\"\nxxh128 = \"00\"\n";
        let err = parse_pins(text, Path::new("pins.toml")).unwrap_err();
        assert!(format!("{err:?}").contains("[harness_files.facts]: pins ../facts.toml"));
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
        let v = verify("rec", &pin, &root).unwrap();
        assert!(v.csv.is_none());
        assert!(v.record.is_some());

        // A CSV turning up beside a record-only pin is harmless (the record
        // outranks it), so it is not refused.
        write_file(&root, rel, CSV_FILE, b"a,b\n");
        assert!(verify("rec", &pin, &root).is_ok());
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
        let err = verify("inp", &pin, &root).unwrap_err();
        assert!(format!("{err:?}").contains("inputs.json its pin does not declare"));

        pin.inputs = Some(inputs.clone());
        let v = verify("inp", &pin, &root).unwrap();
        assert_eq!(v.inputs.unwrap().xxh128, inputs.xxh128);

        write_file(&root, rel, INPUTS_FILE, b"{\"Source\":\"low\"}\n");
        let err = verify("inp", &pin, &root).unwrap_err();
        assert!(format!("{err:?}").contains("hash mismatch"));
    }

    #[test]
    fn verify_refuses_a_probe_resolving_no_feed() {
        let root = crate::test_scratch::scratch("piners_registry", "verify_feedless");
        let rel = Path::new("p/nofeed");
        let pin = fed(ProbeFiles {
            pine: write_file(&root, rel, PINE_FILE, b"//@version=6\n"),
            inputs: None,
            csv: Some(write_file(&root, rel, CSV_FILE, b"a,b\n")),
            record: None,
        });
        let err = verify_probe("nofeed", &pin, ProbeConfig::default(), &root, &root).unwrap_err();
        assert!(format!("{err:?}").contains("resolves no `feed`"));
        // The resolved facts travel with the verified probe.
        let v = verify("nofeed", &pin, &root).unwrap();
        assert_eq!(v.config.feed.as_deref(), Some("f"));
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

        for (id, dir) in [
            ("rooted", ""),
            ("outside", "../p/"),
            ("absolute", "/abs/p/"),
            ("dotted", "p/./q/"),
        ] {
            let text = format!(
                "[probes.{id}]\npine = {{ path = \"{dir}strategy.pine\", xxh128 = \"aa\" }}\n\
                 csv = {{ path = \"{dir}tv_trades.csv\", xxh128 = \"cc\" }}\n"
            );
            let err = parse_pins(&text, Path::new("pins.toml")).unwrap_err();
            assert!(
                format!("{err:?}").contains(&format!("{id}: pins {dir}strategy.pine")),
                "{id}: {err:?}"
            );
        }
    }

    #[test]
    fn parse_holds_the_file_to_the_declaration_rules() {
        // The writers self-check through parse_pins, so these bind them too.
        let err = parse_pins(
            r#"
[probe_config."p/dead-tz"]
tv_trades_csv_tz = "utc"

[probes.dead-tz]
pine = { path = "p/dead-tz/strategy.pine", xxh128 = "aa" }
record = { path = "p/dead-tz/tv_record.json", xxh128 = "bb" }
"#,
            Path::new("pins.toml"),
        )
        .unwrap_err();
        assert!(format!("{err:?}").contains("resolves `tv_trades_csv_tz`"));
        // The old per-pin keys are gone, not silently ignored.
        let err = parse_pins(
            r#"
[probes.old]
feed = "f"
pine = { path = "p/old/strategy.pine", xxh128 = "aa" }
csv = { path = "p/old/tv_trades.csv", xxh128 = "bb" }
"#,
            Path::new("pins.toml"),
        )
        .unwrap_err();
        assert!(format!("{err:?}").contains("unknown field `feed`"));
    }

    #[test]
    fn lint_fails_on_unknown_expected_label() {
        let mut r = registry_with(&[], &["a"]);
        r.pins.get_mut("a").unwrap().expected = Some("totally-bogus".to_owned());
        let err = r.lint().unwrap_err();
        assert!(format!("{err:?}").contains("totally-bogus"));
    }

}
