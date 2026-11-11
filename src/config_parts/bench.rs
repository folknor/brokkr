//! `[bench.*]`, `[bench_params.*]` and `[data]`: the script-bench schema.
//!
//! A bench is a Lua script plus the build settings and params it runs with.
//! This module reads and validates the tables at load; nothing here reads a
//! script file or compiles Lua (`brokkr check`'s `bench` phase does), so a
//! broken script never blocks an unrelated command.
//!
//! Everything a later refusal would have to add is refused here, so a bench
//! table that loads keeps loading: param declarations are parsed strictly and
//! their reference graph (unknown `{param}` references, cycles, collections
//! named in a `from` path) is judged on each bench's effective params, after
//! `uses` has pulled in the shared sets and the bench's own params have
//! replaced theirs. What needs the resolved config (whether a key exists
//! under a `from` path, digests) is judged when a bench runs.
//!
//! `[data]` is reserved for project data that `from` paths walk; brokkr never
//! interprets it beyond requiring a table.

use super::ConfigSources;
use crate::build::RustProfile;
use crate::error::DevError;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The param name brokkr keeps for itself: every bench's `--help`. A bench's
/// params follow its name on the command line and brokkr's own options precede
/// it, so this is the whole set of names a param cannot take.
pub const RESERVED_PARAM_NAMES: &[&str] = &["help"];

/// One `[bench.NAME]`, its params already composed with the sets it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bench {
    pub name: String,
    pub script: BenchScript,
    /// The bin target to build; `None` means `[bin] default`.
    pub target: Option<String>,
    pub profile: Option<RustProfile>,
    pub features: Vec<String>,
    /// Whether cargo builds the package's default features; `true` unless the
    /// table sets `default_features = false`.
    pub default_features: bool,
    /// The package's own features that instrument it for hotpath: added to
    /// the build under `brokkr bench --hotpath`, and refused in a timing
    /// run's resolved features. Empty means the bench cannot be profiled.
    pub hotpath_features: Vec<String>,
    /// The package's own features that instrument it for hotpath's
    /// allocation tracking: added to the build under `brokkr bench --alloc`,
    /// and refused in a timing run's resolved features. Empty means the bench
    /// cannot be alloc-profiled.
    pub alloc_features: Vec<String>,
    /// The `[bench_params]` sets this bench pulled in, in the order given.
    pub uses: Vec<String>,
    /// The effective params: every used set's, then the bench's own, which
    /// replace a set's param of the same name whole.
    pub params: BTreeMap<String, Param>,
    /// The top-level `bench_lib` directory every bench's `require` loads
    /// modules from, `None` when unset ([`parse_lib`]).
    pub library: Option<PathBuf>,
    /// The devs' notes on the bench: description, version, date and
    /// changelog, every one valid when the config loaded.
    pub annotations: super::bench_annotations::Annotations,
}

/// Where a bench's Lua lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchScript {
    /// `script = "..."`, inline in the config file `declared_in`.
    Inline {
        source: String,
        declared_in: PathBuf,
    },
    /// `script_file = "..."`, resolved against the directory of the config
    /// file that set it (`declared_in`) to the absolute `path`.
    File { path: PathBuf, declared_in: PathBuf },
}

/// One param declaration, from a bench's `params` or a `[bench_params]` set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub help: Option<String>,
    /// What values the param takes: exactly one declaration says so.
    pub domain: Domain,
    /// The value used when the command line gives none, in the syntax the
    /// command line takes.
    pub default: Option<String>,
    /// `default_only = true`: absent from the command line, a `from` param
    /// binds the one key its table holds, and is refused, naming the keys,
    /// when it holds none or several. Never beside `default` or `required`.
    pub default_only: bool,
    /// A collection: one sample binds the whole list of values.
    pub many: bool,
    /// Whether a value is a list split on `,` (the default). `split = false`
    /// takes each occurrence, and the default, as one item, so a value may
    /// hold a `,`; only the comma tokenizing changes, so `all` and a range
    /// keep their meaning per item.
    pub split: bool,
    /// The command line must give it: its absence is refused when the bench
    /// binds, before anything is built. Never beside a `default`.
    pub required: bool,
    /// Where the declaration came from.
    pub origin: ParamOrigin,
}

/// A param's value domain. Every param declares one, so no value reaches a
/// script unchecked by accident: `free` is the deliberate opt-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Domain {
    /// `from = "..."`: the keys of a config table.
    From(FromPath),
    /// `choices = [...]`: one of these strings, checked when the bench binds.
    Choices(Vec<String>),
    /// `validated = true`: a plain string the script's `validate()` checks,
    /// which the script must then declare.
    Validated,
    /// `free = true`: a plain string brokkr deliberately does not check.
    Free,
    /// `integer = "u32"` (a width, `min`, `max`): a canonical decimal within
    /// the bounds, checked when the bench binds and delivered as its text.
    Integer(IntegerDomain),
    /// `number = true` (`min`, `max`): text that parses as a finite f64
    /// within the bounds, delivered as its text.
    Number {
        min: Option<FloatBound>,
        max: Option<FloatBound>,
    },
    /// `bbox = true`: `lon_min,lat_min,lon_max,lat_max` by the dataset bbox
    /// rules, each occurrence taken whole and delivered as given.
    Bbox,
}

/// A `number` bound: always finite (load refuses any other), so equality by
/// bits is equality by value and `Eq` holds.
#[derive(Debug, Clone, Copy)]
pub struct FloatBound(pub f64);

impl PartialEq for FloatBound {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for FloatBound {}

/// An `integer` domain's width and inclusive bounds, the bounds already
/// narrowed to the width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerDomain {
    /// The width as declared (`u32`, `i64`, ...).
    pub width: String,
    pub signed: bool,
    pub min: i128,
    pub max: i128,
}

/// The widths `integer` names, with their ranges.
const WIDTHS: &[(&str, i128, i128)] = &[
    ("u8", 0, u8::MAX as i128),
    ("u16", 0, u16::MAX as i128),
    ("u32", 0, u32::MAX as i128),
    ("u64", 0, u64::MAX as i128),
    ("i8", i8::MIN as i128, i8::MAX as i128),
    ("i16", i16::MIN as i128, i16::MAX as i128),
    ("i32", i32::MIN as i128, i32::MAX as i128),
    ("i64", i64::MIN as i128, i64::MAX as i128),
];

impl Domain {
    /// Whether this domain checks a plain param's value itself, so binding
    /// judges each item with [`Domain::admit`].
    pub fn typed(&self) -> bool {
        matches!(self, Self::Integer(_) | Self::Number { .. } | Self::Bbox)
    }

    /// Judge one item of a typed domain: `Err` says why it is refused, in
    /// the words a refusal follows the param and value with. Any other
    /// domain admits everything here.
    pub fn admit(&self, item: &str) -> Result<(), String> {
        match self {
            Self::Integer(domain) => {
                // Canonical decimal: one spelling per value, so the stored
                // key pairs; no sign but a `-` on a nonzero signed value.
                let (negative, digits) = match item.strip_prefix('-') {
                    Some(rest) if domain.signed => (true, rest),
                    _ => (false, item),
                };
                let canonical = !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && (digits == "0" || !digits.starts_with('0'))
                    && !(negative && digits == "0");
                if !canonical {
                    return Err(format!(
                        "is not a {} integer: digits only, {}no '+', no leading zero",
                        domain.width,
                        if domain.signed {
                            "a '-' for a negative value, "
                        } else {
                            "no sign, "
                        }
                    ));
                }
                let value = digits
                    .parse::<i128>()
                    .ok()
                    .map(|v| if negative { -v } else { v });
                match value {
                    Some(v) if (domain.min..=domain.max).contains(&v) => Ok(()),
                    _ => Err(format!(
                        "is outside {}..{} ({})",
                        domain.min, domain.max, domain.width
                    )),
                }
            }
            Self::Number { min, max } => {
                let value = item
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| "is not a finite number".to_owned())?;
                if min.is_some_and(|m| value < m.0) || max.is_some_and(|m| value > m.0) {
                    return Err(format!("is outside {}", number_range(*min, *max)));
                }
                Ok(())
            }
            Self::Bbox => item
                .parse::<crate::datasets::Bbox>()
                .map(|_| ())
                .map_err(|e| format!("is not a bbox lon_min,lat_min,lon_max,lat_max: {e}")),
            Self::From(_) | Self::Choices(_) | Self::Validated | Self::Free => Ok(()),
        }
    }
}

impl Param {
    /// The config table whose keys are the param's values; `None` for a
    /// plain param.
    pub fn from(&self) -> Option<&FromPath> {
        match &self.domain {
            Domain::From(path) => Some(path),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamOrigin {
    /// The bench's own `params`.
    Bench,
    /// The `[bench_params]` set of this name.
    Set(String),
}

/// A parsed `from` path: dot-separated segments, each a literal key or a
/// whole-segment `{param}` reference to another param's chosen key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromPath {
    pub segments: Vec<FromSegment>,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromSegment {
    Key(String),
    Param(String),
}

impl FromPath {
    /// The path as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The params this path references, in path order.
    pub fn references(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| match s {
            FromSegment::Param(name) => Some(name.as_str()),
            FromSegment::Key(_) => None,
        })
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut segments = Vec::new();
        for segment in text.split('.') {
            if segment.is_empty() {
                return Err("has an empty segment".into());
            }
            let parsed = match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) => {
                    if !is_param_name(name) {
                        return Err(format!("references {{{name}}}, which is not a param name"));
                    }
                    FromSegment::Param(name.to_owned())
                }
                None if segment.contains(['{', '}']) => {
                    return Err(format!(
                        "segment {segment:?} mixes a {{param}} reference with other text; \
                         a reference is a whole segment"
                    ));
                }
                None => FromSegment::Key(segment.to_owned()),
            };
            segments.push(parsed);
        }
        Ok(Self {
            segments,
            text: text.to_owned(),
        })
    }
}

/// A param name: also its `--NAME` flag, so lowercase, starting with a
/// letter, then letters, digits, `_` or `-`. A hotpath stage name follows
/// the same grammar.
pub fn is_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A bench or set name: the argument `brokkr bench` takes, so it may not
/// start with `-`.
fn is_bench_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParam {
    help: Option<String>,
    from: Option<String>,
    choices: Option<Vec<String>>,
    #[serde(default)]
    validated: bool,
    #[serde(default)]
    free: bool,
    integer: Option<String>,
    #[serde(default)]
    number: bool,
    #[serde(default)]
    bbox: bool,
    min: Option<toml::Value>,
    max: Option<toml::Value>,
    default: Option<String>,
    #[serde(default)]
    default_only: bool,
    #[serde(default)]
    many: bool,
    split: Option<bool>,
    #[serde(default)]
    required: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBench {
    script: Option<String>,
    script_file: Option<String>,
    target: Option<String>,
    profile: Option<RustProfile>,
    #[serde(default)]
    features: Vec<String>,
    default_features: Option<bool>,
    #[serde(default)]
    hotpath_features: Vec<String>,
    #[serde(default)]
    alloc_features: Vec<String>,
    #[serde(default)]
    uses: Vec<String>,
    #[serde(default)]
    params: BTreeMap<String, toml::Value>,
}

/// Parse `[bench.*]`, `[bench_params.*]` and `[data]` from the composed
/// table. `sources` locates the file that set each `script` and
/// `script_file`.
pub(crate) fn parse(
    table: &toml::Table,
    sources: &ConfigSources,
) -> Result<BTreeMap<String, Bench>, DevError> {
    if let Some(data) = table.get("data")
        && !data.is_table()
    {
        return Err(DevError::Config(
            "data must be a table: [data] holds project data that bench params select from".into(),
        ));
    }
    let sets = parse_sets(table)?;
    let library = parse_lib(table, sources)?;
    let Some(value) = table.get("bench") else {
        return Ok(BTreeMap::new());
    };
    let benches = value
        .as_table()
        .ok_or_else(|| DevError::Config("bench must be a table of [bench.NAME] tables".into()))?;
    let mut out = BTreeMap::new();
    for (name, value) in benches {
        let mut bench = parse_bench(name, value, &sets, sources)?;
        bench.library.clone_from(&library);
        out.insert(name.clone(), bench);
    }
    Ok(out)
}

fn parse_sets(table: &toml::Table) -> Result<BTreeMap<String, BTreeMap<String, Param>>, DevError> {
    let Some(value) = table.get("bench_params") else {
        return Ok(BTreeMap::new());
    };
    let sets = value.as_table().ok_or_else(|| {
        DevError::Config("bench_params must be a table of [bench_params.NAME] sets".into())
    })?;
    let mut out = BTreeMap::new();
    for (set, value) in sets {
        let at = format!("[bench_params.{set}]");
        if !is_bench_name(set) {
            return Err(DevError::Config(format!(
                "{at}: a set name is letters, digits, '_' and '-', not starting with '-'"
            )));
        }
        let params = value.as_table().ok_or_else(|| {
            DevError::Config(format!("{at} must be a table of param declarations"))
        })?;
        let mut parsed = BTreeMap::new();
        for (param, decl) in params {
            let declared = parse_param(
                &format!("{at} {param}"),
                param,
                decl,
                ParamOrigin::Set(set.clone()),
            )
            .map_err(DevError::Config)?;
            parsed.insert(param.clone(), declared);
        }
        out.insert(set.clone(), parsed);
    }
    Ok(out)
}

/// Parse one declaration. `at` names it in errors.
fn parse_param(
    at: &str,
    name: &str,
    value: &toml::Value,
    origin: ParamOrigin,
) -> Result<Param, String> {
    if !is_param_name(name) {
        return Err(format!(
            "{at}: a param name is its --flag: lowercase, starting with a letter, then \
             letters, digits, '_' or '-'"
        ));
    }
    if RESERVED_PARAM_NAMES.contains(&name) {
        return Err(format!(
            "{at}: '{name}' is reserved: every bench takes --{name} from brokkr"
        ));
    }
    let raw: RawParam = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("{at}: {e}"))?;
    let declared = [
        raw.from.is_some().then_some("from"),
        raw.choices.is_some().then_some("choices"),
        raw.validated.then_some("validated"),
        raw.free.then_some("free"),
        raw.integer.is_some().then_some("integer"),
        raw.number.then_some("number"),
        raw.bbox.then_some("bbox"),
    ];
    let declared: Vec<&str> = declared.into_iter().flatten().collect();
    match declared.as_slice() {
        [] => {
            return Err(format!(
                "{at}: declares no domain - give from (a config table's keys), choices (a list \
                 of values), integer = \"u32\" (or another width), number = true, bbox = true, \
                 validated = true (the script's validate() checks it) or free = true (any \
                 value, unchecked)"
            ));
        }
        [_] => {}
        several => {
            return Err(format!(
                "{at}: declares {}, which exclude each other - a param has one domain",
                several.join(" and ")
            ));
        }
    }
    let split = typed_shape(at, &raw)?;
    let domain = if let Some(width) = raw.integer.as_deref() {
        Domain::Integer(integer_domain(
            at,
            width,
            raw.min.as_ref(),
            raw.max.as_ref(),
        )?)
    } else if raw.number {
        number_domain(at, raw.min.as_ref(), raw.max.as_ref())?
    } else if raw.bbox {
        Domain::Bbox
    } else if let Some(text) = raw.from.as_deref() {
        Domain::From(FromPath::parse(text).map_err(|e| format!("{at}: from {text:?} {e}"))?)
    } else if let Some(choices) = raw.choices {
        check_choices(at, &choices, raw.many, split)?;
        Domain::Choices(choices)
    } else if raw.validated {
        Domain::Validated
    } else {
        Domain::Free
    };
    if let Some(default) = raw.default.as_deref().filter(|d| !d.trim().is_empty()) {
        check_default(at, &domain, default, split)?;
    }
    if raw.default.as_deref().is_some_and(|d| d.trim().is_empty()) {
        return Err(format!(
            "{at}: default is blank - give a value or remove the key"
        ));
    }
    if raw.required && raw.default.is_some() {
        return Err(format!(
            "{at}: required and default exclude each other - a param with a default is never \
             absent; remove one"
        ));
    }
    if raw.default_only {
        check_default_only(at, &domain, raw.many, raw.default.is_some(), raw.required)?;
    }
    Ok(Param {
        help: raw.help,
        domain,
        default: raw.default,
        default_only: raw.default_only,
        many: raw.many,
        split,
        required: raw.required,
        origin,
    })
}

/// Whether the param splits its values, and the typed keys' placement: a
/// bbox's own commas separate its coordinates, so it takes each occurrence
/// whole and refuses `split = true`; `min` and `max` only on `integer` or
/// `number`, never silently ignored.
fn typed_shape(at: &str, raw: &RawParam) -> Result<bool, String> {
    if raw.bbox && raw.split == Some(true) {
        return Err(format!(
            "{at}: a bbox holds commas, so it is never split; remove split = true"
        ));
    }
    if (raw.min.is_some() || raw.max.is_some()) && raw.integer.is_none() && !raw.number {
        return Err(format!(
            "{at}: min and max bound an integer or number param; this one is neither"
        ));
    }
    Ok(raw.split.unwrap_or(!raw.bbox))
}

/// A `choices` or typed default judged here as binding would judge it, so
/// one that loads binds: every item in the domain, none twice. An unsplit
/// default is one item. Any other domain's default is judged at bind.
fn check_default(at: &str, domain: &Domain, default: &str, split: bool) -> Result<(), String> {
    if !matches!(domain, Domain::Choices(_)) && !domain.typed() {
        return Ok(());
    }
    let mut seen = BTreeSet::new();
    let items: Vec<&str> = if split {
        default.split(',').collect()
    } else {
        vec![default]
    };
    for item in items {
        let why = match domain {
            Domain::Choices(choices) if !choices.iter().any(|c| c == item) => {
                Some(format!("is not among its choices ({})", choices.join(", ")))
            }
            Domain::Choices(_) => None,
            typed => typed.admit(item).err(),
        };
        if let Some(why) = why {
            return Err(format!(
                "{at}: default {default:?} holds {item:?}, which {why}"
            ));
        }
        if !seen.insert(item) {
            return Err(format!("{at}: default {default:?} gives {item:?} twice"));
        }
    }
    Ok(())
}

/// A `number = true` domain, its `min` and `max` finite and in order. An
/// integer bound is refused past 2^53, where an f64 would silently move it.
fn number_domain(
    at: &str,
    min: Option<&toml::Value>,
    max: Option<&toml::Value>,
) -> Result<Domain, String> {
    let bound = |key: &str, value: Option<&toml::Value>| match value {
        None => Ok(None),
        Some(toml::Value::Float(f)) if f.is_finite() => Ok(Some(FloatBound(*f))),
        Some(toml::Value::Integer(i)) if i.unsigned_abs() <= 1 << 53 => {
            Ok(Some(FloatBound(*i as f64)))
        }
        Some(other) => Err(format!(
            "{at}: {key} {other} is not a finite number an f64 holds exactly"
        )),
    };
    let (min, max) = (bound("min", min)?, bound("max", max)?);
    if let (Some(lo), Some(hi)) = (min, max)
        && lo.0 > hi.0
    {
        return Err(format!("{at}: min {} is above max {}", lo.0, hi.0));
    }
    Ok(Domain::Number { min, max })
}

/// A `number` domain's bounds as `MIN..MAX`, an absent end left empty.
pub fn number_range(min: Option<FloatBound>, max: Option<FloatBound>) -> String {
    let end = |b: Option<FloatBound>| b.map_or_else(String::new, |b| b.0.to_string());
    format!("{}..{}", end(min), end(max))
}

/// An `integer = WIDTH` domain, its `min` and `max` TOML integers within the
/// width and in order. A bound is a TOML integer, so a u64 bound above
/// `i64::MAX` cannot be written; the width's own maximum needs none.
fn integer_domain(
    at: &str,
    width: &str,
    min: Option<&toml::Value>,
    max: Option<&toml::Value>,
) -> Result<IntegerDomain, String> {
    let Some(&(_, lo, hi)) = WIDTHS.iter().find(|(name, _, _)| *name == width) else {
        return Err(format!(
            "{at}: integer {width:?} is not a width - give one of {}",
            WIDTHS
                .iter()
                .map(|(n, _, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let bound = |key: &str, value: Option<&toml::Value>, or: i128| match value {
        None => Ok(or),
        Some(toml::Value::Integer(i)) if (lo..=hi).contains(&i128::from(*i)) => Ok(i128::from(*i)),
        Some(other) => Err(format!(
            "{at}: {key} {other} is not an integer within {width} ({lo}..{hi})"
        )),
    };
    let (min, max) = (bound("min", min, lo)?, bound("max", max, hi)?);
    if min > max {
        return Err(format!("{at}: min {min} is above max {max}"));
    }
    Ok(IntegerDomain {
        width: width.to_owned(),
        signed: lo < 0,
        min,
        max,
    })
}

/// `default_only` on a scalar `from` param with neither `default` nor
/// `required`, the only shape whose absence it can fill with one key.
fn check_default_only(
    at: &str,
    domain: &Domain,
    many: bool,
    default: bool,
    required: bool,
) -> Result<(), String> {
    let clash = if !matches!(domain, Domain::From(_)) {
        "default_only takes the one key of a `from` table; this param has no from"
    } else if many {
        "default_only binds one key, and a `many` param binds a collection"
    } else if default {
        "default_only and default exclude each other - remove one"
    } else if required {
        "default_only and required exclude each other - a defaulted param is never absent"
    } else {
        return Ok(());
    };
    Err(format!("{at}: {clash}"))
}

/// A `choices` list every value of which the command line can give: not
/// empty, no value blank, repeated or, unless the param is unsplit, holding
/// the `,` the command line splits values on, and for a collection (`many`)
/// none holding the `..` that binding reads there as a range.
fn check_choices(at: &str, choices: &[String], many: bool, split: bool) -> Result<(), String> {
    if choices.is_empty() {
        return Err(format!("{at}: choices is empty - list the values it takes"));
    }
    let mut seen = BTreeSet::new();
    for choice in choices {
        if choice.trim().is_empty() {
            return Err(format!("{at}: choices has a blank value"));
        }
        if split && choice.contains(',') {
            return Err(format!(
                "{at}: choice {choice:?} holds a ',', which the command line splits values on, \
                 so it could never be given; split = false takes each value whole"
            ));
        }
        if many && choice.contains("..") {
            return Err(format!(
                "{at}: choice {choice:?} holds '..', which a collection reads as a range, so it \
                 could never be given"
            ));
        }
        if !seen.insert(choice.as_str()) {
            return Err(format!("{at}: choices lists {choice:?} twice"));
        }
    }
    Ok(())
}

fn parse_bench(
    name: &str,
    value: &toml::Value,
    sets: &BTreeMap<String, BTreeMap<String, Param>>,
    sources: &ConfigSources,
) -> Result<Bench, DevError> {
    let at = format!("[bench.{name}]");
    let err = |msg: String| DevError::Config(format!("{at}: {msg}"));
    if !is_bench_name(name) {
        return Err(err(
            "a bench name is letters, digits, '_' and '-', not starting with '-'".into(),
        ));
    }
    let Some(table) = value.as_table() else {
        return Err(DevError::Config(format!("{at} must be a table")));
    };
    let annotations = super::bench_annotations::Annotations::read(table);
    if let Some(problem) = annotations.problems().first() {
        return Err(err(problem.clone()));
    }
    let mut rest = table.clone();
    for key in super::bench_annotations::KEYS {
        rest.remove(key);
    }
    let raw: RawBench = toml::Value::Table(rest)
        .try_into()
        .map_err(|e: toml::de::Error| err(e.to_string()))?;
    let script = bench_script(name, raw.script, raw.script_file, sources).map_err(err)?;
    if raw.target.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err(err(
            "target is blank - name a bin target or remove the key".into()
        ));
    }
    if raw.features.iter().any(|f| f.trim().is_empty()) {
        return Err(err("features has a blank entry".into()));
    }
    // The package's own feature names, as cargo reports the package's
    // resolved features: a dependency's (`dep/feat`) or an optional
    // dependency (`dep:name`) is not one brokkr can find in that list.
    for (key, list) in [
        ("hotpath_features", &raw.hotpath_features),
        ("alloc_features", &raw.alloc_features),
    ] {
        if let Some(bad) = list.iter().find(|f| {
            f.trim().is_empty() || f.contains(['/', ':']) || f.contains(char::is_whitespace)
        }) {
            return Err(err(format!(
                "{key} entry {bad:?} is not one of the package's own feature names"
            )));
        }
        if let Some(both) = list.iter().find(|f| raw.features.contains(f)) {
            return Err(err(format!(
                "{both:?} is in both features and {key}: a timing run would be instrumented"
            )));
        }
    }
    let params = effective_params(&raw.uses, raw.params, sets).map_err(err)?;
    check_references(&params).map_err(err)?;
    Ok(Bench {
        name: name.to_owned(),
        script,
        target: raw.target,
        profile: raw.profile,
        features: raw.features,
        default_features: raw.default_features.unwrap_or(true),
        hotpath_features: raw.hotpath_features,
        alloc_features: raw.alloc_features,
        uses: raw.uses,
        params,
        library: None,
        annotations,
    })
}

/// Parse the optional top-level `bench_lib = "dir"`: the directory a bench
/// script's `require(name)` loads modules from, resolved against the
/// directory of the config file that set it, as `script_file` is. Claimed
/// only as a string: a `[bench_lib]` table is a hostname section, as it
/// always was (`parser::VALUE_KEYS`).
pub(crate) fn parse_lib(
    table: &toml::Table,
    sources: &ConfigSources,
) -> Result<Option<PathBuf>, DevError> {
    let Some(value) = table.get("bench_lib").filter(|value| !value.is_table()) else {
        return Ok(None);
    };
    let dir = value.as_str().ok_or_else(|| {
        DevError::Config(
            "bench_lib must be a string: the directory bench scripts require modules from".into(),
        )
    })?;
    if dir.trim().is_empty() {
        return Err(DevError::Config("bench_lib is blank".into()));
    }
    let origin = sources.value_origin(&["bench_lib"]).ok_or_else(|| {
        DevError::Config("bench_lib: no config file is recorded as setting it".into())
    })?;
    Ok(Some(resolved_beside(&origin.base.join(dir))))
}

/// The optional bench-script registry: `bench_scripts = "dir"` and
/// `bench_scripts_exempt = ["path", ...]`. With it set, `brokkr check`'s
/// `bench` phase refuses every `.lua` entry under `dir` that no bench's
/// `script_file` names, that is not a module `bench_lib` loads, and that no
/// exemption covers (`crate::bench::check_registry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registry {
    /// The scripts directory, resolved beside the file that set it.
    pub(crate) dir: PathBuf,
    /// Each a file or a directory covering everything below it, resolved
    /// beside the file that set the list. Matched by the location of the
    /// entry, never what a link names.
    pub(crate) exempt: Vec<PathBuf>,
    /// `bench_lib`, carried here because a config with no bench has none
    /// on any [`Bench`].
    pub(crate) library: Option<PathBuf>,
}

/// Parse `bench_scripts` and `bench_scripts_exempt`. Both are claimed only
/// as non-table values, like `bench_lib` (`parser::VALUE_KEYS`).
pub(crate) fn parse_registry(
    table: &toml::Table,
    sources: &ConfigSources,
) -> Result<Option<Registry>, DevError> {
    let value_of = |key: &str| table.get(key).filter(|value| !value.is_table());
    let base_of = |key: &str| {
        sources
            .value_origin(&[key])
            .map(|origin| origin.base.to_path_buf())
            .ok_or_else(|| {
                DevError::Config(format!("{key}: no config file is recorded as setting it"))
            })
    };
    let exempt = match value_of("bench_scripts_exempt") {
        None => None,
        Some(value) => {
            let shape = || {
                DevError::Config(
                    "bench_scripts_exempt must be a list of strings: the files and directories \
                     under bench_scripts no bench need name"
                        .into(),
                )
            };
            let items = value.as_array().ok_or_else(shape)?;
            let base = base_of("bench_scripts_exempt")?;
            let mut out = Vec::new();
            for item in items {
                let path = item.as_str().ok_or_else(shape)?;
                if path.trim().is_empty() {
                    return Err(DevError::Config(
                        "bench_scripts_exempt holds a blank path".into(),
                    ));
                }
                let joined = base.join(path);
                // A path ending in `..` has no name to keep as spelled: it
                // is a directory, resolved whole, so it can contain the
                // walk's canonical paths.
                out.push(if joined.file_name().is_none() {
                    std::fs::canonicalize(&joined).unwrap_or(joined)
                } else {
                    resolved_beside(&joined)
                });
            }
            Some(out)
        }
    };
    let Some(value) = value_of("bench_scripts") else {
        if exempt.is_some() {
            return Err(DevError::Config(
                "bench_scripts_exempt is set without bench_scripts, the directory it exempts from"
                    .into(),
            ));
        }
        return Ok(None);
    };
    let dir = value.as_str().ok_or_else(|| {
        DevError::Config(
            "bench_scripts must be a string: the directory every bench script lives under".into(),
        )
    })?;
    if dir.trim().is_empty() {
        return Err(DevError::Config("bench_scripts is blank".into()));
    }
    Ok(Some(Registry {
        dir: resolved_beside(&base_of("bench_scripts")?.join(dir)),
        exempt: exempt.unwrap_or_default(),
        library: parse_lib(table, sources)?,
    }))
}

/// `path` with its directory resolved on disk, so a `script_file` or
/// `bench_lib` spelled through `..` (`brokkr.d/../benches/x.lua`) records as
/// the one file it names. `..` is resolved by the filesystem, never dropped
/// textually: through a linked directory it need not undo the component
/// before it. The last component is kept as spelled, so a linked script is
/// named as itself. A directory that cannot be resolved leaves the path as
/// joined, for the read to report.
fn resolved_beside(path: &std::path::Path) -> PathBuf {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    match std::fs::canonicalize(dir) {
        Ok(dir) => dir.join(name),
        Err(_) => path.to_path_buf(),
    }
}

fn bench_script(
    name: &str,
    script: Option<String>,
    script_file: Option<String>,
    sources: &ConfigSources,
) -> Result<BenchScript, String> {
    let declaring = |field: &str| {
        sources
            .value_origin(&["bench", name, field])
            .ok_or_else(|| format!("no config file is recorded as setting {field}"))
    };
    match (script, script_file) {
        (Some(_), Some(_)) => Err(
            "sets both script and script_file - give the Lua inline or name its file, not both"
                .into(),
        ),
        (None, None) => Err("needs script (inline Lua) or script_file (a .lua file)".into()),
        (Some(source), None) => {
            if source.trim().is_empty() {
                return Err("script is blank".into());
            }
            let origin = declaring("script")?;
            Ok(BenchScript::Inline {
                source,
                declared_in: origin.file.to_path_buf(),
            })
        }
        (None, Some(file)) => {
            if file.trim().is_empty() {
                return Err("script_file is blank".into());
            }
            let origin = declaring("script_file")?;
            Ok(BenchScript::File {
                path: resolved_beside(&origin.base.join(file)),
                declared_in: origin.file.to_path_buf(),
            })
        }
    }
}

/// Compose a bench's params: each used set's in turn, then the bench's own,
/// which replace a set's whole. Two sets declaring one param the bench does
/// not declare itself are refused: neither order is something the config says.
fn effective_params(
    uses: &[String],
    own: BTreeMap<String, toml::Value>,
    sets: &BTreeMap<String, BTreeMap<String, Param>>,
) -> Result<BTreeMap<String, Param>, String> {
    let mut out: BTreeMap<String, Param> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for set in uses {
        if !seen.insert(set.as_str()) {
            return Err(format!("uses lists {set:?} twice"));
        }
        let params = sets.get(set).ok_or_else(|| {
            let known: Vec<&str> = sets.keys().map(String::as_str).collect();
            let known = if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            };
            format!("uses {set:?}, which is not a [bench_params] set (defined: {known})")
        })?;
        for (name, param) in params {
            if own.contains_key(name) {
                continue;
            }
            if let Some(previous) = out.get(name)
                && let ParamOrigin::Set(other) = &previous.origin
            {
                return Err(format!(
                    "param {name:?} is declared by both used sets {other:?} and {set:?}; \
                     declare it in the bench to choose"
                ));
            }
            out.insert(name.clone(), param.clone());
        }
    }
    for (name, decl) in own {
        let param = parse_param(&format!("params.{name}"), &name, &decl, ParamOrigin::Bench)?;
        out.insert(name, param);
    }
    Ok(out)
}

/// The static half of the `from` rules, on the effective params: every
/// `{param}` names a param of this bench, none names a collection, and the
/// references form no cycle.
fn check_references(params: &BTreeMap<String, Param>) -> Result<(), String> {
    for (name, param) in params {
        let Some(from) = param.from() else { continue };
        for reference in from.references() {
            let Some(target) = params.get(reference) else {
                return Err(format!(
                    "param {name:?}: from {:?} references {{{reference}}}, which is not a param \
                     of this bench",
                    from.as_str()
                ));
            };
            if target.many {
                return Err(format!(
                    "param {name:?}: from {:?} references {{{reference}}}, a collection \
                     (many = true); a path can only take one key",
                    from.as_str()
                ));
            }
        }
    }
    // Depth-first, three colours: a reference back onto the current path is a
    // cycle, reported along that path.
    let mut done: BTreeSet<&str> = BTreeSet::new();
    for start in params.keys() {
        let mut path: Vec<&str> = Vec::new();
        visit(start, params, &mut path, &mut done)?;
    }
    Ok(())
}

fn visit<'a>(
    name: &'a str,
    params: &'a BTreeMap<String, Param>,
    path: &mut Vec<&'a str>,
    done: &mut BTreeSet<&'a str>,
) -> Result<(), String> {
    if done.contains(name) {
        return Ok(());
    }
    if let Some(at) = path.iter().position(|p| *p == name) {
        let mut cycle: Vec<&str> = path[at..].to_vec();
        cycle.push(name);
        return Err(format!(
            "params reference each other in a cycle: {}",
            cycle.join(" -> ")
        ));
    }
    path.push(name);
    if let Some(from) = params.get(name).and_then(Param::from) {
        for reference in from.references() {
            if let Some((key, _)) = params.get_key_value(reference) {
                visit(key, params, path, done)?;
            }
        }
    }
    path.pop();
    done.insert(name);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    /// A scratch tree for one test, populated with `(relative path, contents)`.
    fn tree(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = crate::test_scratch::scratch("config_bench", test_name);
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn benches_of(dir: &Path) -> Result<BTreeMap<String, Bench>, String> {
        super::super::load(dir)
            .map(|(_, cfg, _)| cfg.benches)
            .map_err(|e| e.to_string())
    }

    /// A one-file project config with `body` after the project line.
    fn single(test_name: &str, body: &str) -> Result<BTreeMap<String, Bench>, String> {
        let dir = tree(
            test_name,
            &[("brokkr.toml", &format!("project = \"p\"\n{body}"))],
        );
        benches_of(&dir)
    }

    fn err_of(test_name: &str, body: &str) -> String {
        match single(test_name, body) {
            Ok(_) => panic!("{test_name}: the config should be refused"),
            Err(e) => e,
        }
    }

    #[test]
    fn an_inline_script_and_its_build_settings_load() {
        let benches = single(
            "inline",
            "[bench.cat]\nscript = \"function bench() end\"\ntarget = \"pbfhogg\"\n\
             profile = \"release\"\nfeatures = [\"zlib-ng\"]\n",
        )
        .unwrap();
        let cat = &benches["cat"];
        assert!(matches!(
            &cat.script,
            BenchScript::Inline { source, declared_in }
                if source == "function bench() end" && declared_in.ends_with("brokkr.toml")
        ));
        assert_eq!(cat.target.as_deref(), Some("pbfhogg"));
        assert_eq!(cat.profile, Some(RustProfile::Release));
        assert_eq!(cat.features, ["zlib-ng"]);
        assert!(
            cat.default_features,
            "default features unless the table says otherwise"
        );
    }

    #[test]
    fn hotpath_features_are_the_packages_own_and_apart_from_features() {
        let benches = single(
            "hotpath_features",
            "[bench.cat]\nscript = \"x = 1\"\nhotpath_features = [\"hotpath\"]\n",
        )
        .unwrap();
        assert_eq!(benches["cat"].hotpath_features, ["hotpath"]);
        assert!(
            single("hotpath_absent", "[bench.cat]\nscript = \"x = 1\"\n").unwrap()["cat"]
                .hotpath_features
                .is_empty()
        );
        for (name, list) in [
            ("hotpath_dep", "[\"dep/hotpath\"]"),
            ("hotpath_optional", "[\"dep:hotpath\"]"),
            ("hotpath_blank", "[\" \"]"),
        ] {
            let err = err_of(
                name,
                &format!("[bench.cat]\nscript = \"x = 1\"\nhotpath_features = {list}\n"),
            );
            assert!(err.contains("own feature names"), "{name}: {err}");
        }
        let err = err_of(
            "hotpath_both",
            "[bench.cat]\nscript = \"x = 1\"\nfeatures = [\"hotpath\"]\n\
             hotpath_features = [\"hotpath\"]\n",
        );
        assert!(err.contains("both features and hotpath_features"), "{err}");
    }

    #[test]
    fn alloc_features_are_held_to_the_same_rules() {
        let benches = single(
            "alloc_features",
            "[bench.cat]\nscript = \"x = 1\"\nalloc_features = [\"hotpath-alloc\"]\n",
        )
        .unwrap();
        assert_eq!(benches["cat"].alloc_features, ["hotpath-alloc"]);
        assert!(benches["cat"].hotpath_features.is_empty());
        let err = err_of(
            "alloc_dep",
            "[bench.cat]\nscript = \"x = 1\"\nalloc_features = [\"dep/hotpath-alloc\"]\n",
        );
        assert!(err.contains("alloc_features entry"), "{err}");
        let err = err_of(
            "alloc_both",
            "[bench.cat]\nscript = \"x = 1\"\nfeatures = [\"a\"]\nalloc_features = [\"a\"]\n",
        );
        assert!(err.contains("both features and alloc_features"), "{err}");
    }

    #[test]
    fn a_script_file_resolves_beside_the_file_that_set_it() {
        let dir = tree(
            "script_file_base",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"benches/benches.toml\"]\n",
                ),
                (
                    "benches/benches.toml",
                    "[bench.cat]\nscript_file = \"cat.lua\"\n",
                ),
                ("benches/cat.lua", "function bench() end\n"),
            ],
        );
        let benches = benches_of(&dir).unwrap();
        let BenchScript::File { path, declared_in } = &benches["cat"].script else {
            panic!("expected a script file");
        };
        let base = std::fs::canonicalize(dir.join("benches")).unwrap();
        assert_eq!(path, &base.join("cat.lua"));
        assert_eq!(declared_in, &base.join("benches.toml"));
    }

    #[test]
    fn a_script_file_through_dot_dot_records_as_the_file_it_names() {
        let dir = tree(
            "script_file_dotdot",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"brokkr.d/benches.toml\"]\n",
                ),
                (
                    "brokkr.d/benches.toml",
                    "[bench.cat]\nscript_file = \"../benches/./cat.lua\"\n",
                ),
                ("benches/cat.lua", "function bench() end\n"),
            ],
        );
        let benches = benches_of(&dir).unwrap();
        let BenchScript::File { path, .. } = &benches["cat"].script else {
            panic!("expected a script file");
        };
        let root = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(path, &root.join("benches/cat.lua"));
    }

    /// `bench_lib` resolves beside the file that set it, reaches every
    /// bench, and is claimed only as a string: a `[bench_lib]` table stays
    /// the hostname section it always was.
    #[test]
    fn bench_lib_resolves_beside_its_file_and_a_table_stays_a_host() {
        let dir = tree(
            "bench_lib_base",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"benches/benches.toml\"]\n",
                ),
                (
                    "benches/benches.toml",
                    "bench_lib = \"lib\"\n[bench.cat]\nscript = \"function bench() end\"\n\
                     [bench.dog]\nscript = \"function bench() end\"\n",
                ),
            ],
        );
        let benches = benches_of(&dir).unwrap();
        let base = std::fs::canonicalize(dir.join("benches")).unwrap();
        for bench in benches.values() {
            assert_eq!(bench.library.as_deref(), Some(base.join("lib").as_path()));
        }
        let host = tree(
            "bench_lib_host",
            &[(
                "brokkr.toml",
                "project = \"p\"\n[bench_lib]\nfeatures = [\"x\"]\n\
                 [bench.cat]\nscript = \"function bench() end\"\n",
            )],
        );
        assert_eq!(benches_of(&host).unwrap()["cat"].library, None);
        for (name, value, needle) in [
            ("bench_lib_number", "1", "must be a string"),
            ("bench_lib_blank", "\" \"", "blank"),
        ] {
            let bad = tree(
                name,
                &[(
                    "brokkr.toml",
                    &format!(
                        "project = \"p\"\nbench_lib = {value}\n\
                         [bench.cat]\nscript = \"function bench() end\"\n"
                    ),
                )],
            );
            let err = benches_of(&bad).unwrap_err();
            assert!(err.contains(needle), "{name}: {err}");
        }
    }

    #[test]
    fn bench_scripts_and_its_exemptions_resolve_beside_their_files() {
        let registry_of = |dir: &Path| {
            super::super::load(dir)
                .map(|(_, cfg, _)| cfg.bench_registry)
                .map_err(|e| e.to_string())
        };
        let dir = tree(
            "bench_scripts_base",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"brokkr.d/benches.toml\"]\nbench_lib = \"benches/lib\"\n",
                ),
                (
                    "brokkr.d/benches.toml",
                    "bench_scripts = \"../benches\"\nbench_scripts_exempt = [\"../benches/scratch\", \"../benches/scratch/..\"]\n",
                ),
                ("benches/scratch/x.lua", ""),
                ("benches/lib/m.lua", ""),
            ],
        );
        let root = std::fs::canonicalize(&dir).unwrap();
        let registry = registry_of(&dir).unwrap().unwrap();
        assert_eq!(registry.dir, root.join("benches"));
        assert_eq!(
            registry.exempt,
            [root.join("benches/scratch"), root.join("benches")],
            "a trailing .. is resolved, so it contains the walk's canonical paths"
        );
        assert_eq!(registry.library, Some(root.join("benches/lib")));
        let host = tree(
            "bench_scripts_host",
            &[(
                "brokkr.toml",
                "project = \"p\"\n[bench_scripts]\nfeatures = [\"x\"]\n",
            )],
        );
        assert_eq!(registry_of(&host).unwrap(), None);
        for (name, body, needle) in [
            (
                "bench_scripts_number",
                "bench_scripts = 1",
                "must be a string",
            ),
            ("bench_scripts_blank", "bench_scripts = \" \"", "blank"),
            (
                "bench_scripts_exempt_alone",
                "bench_scripts_exempt = [\"x\"]",
                "without bench_scripts",
            ),
            (
                "bench_scripts_exempt_shape",
                "bench_scripts = \"b\"\nbench_scripts_exempt = \"x\"",
                "list of strings",
            ),
        ] {
            let bad = tree(
                name,
                &[("brokkr.toml", &format!("project = \"p\"\n{body}\n"))],
            );
            let err = registry_of(&bad).unwrap_err();
            assert!(err.contains(needle), "{name}: {err}");
        }
    }

    #[test]
    fn script_and_script_file_are_one_setting_across_layers() {
        // The including file swaps the included inline script for a file;
        // the included target still applies.
        let dir = tree(
            "script_slot",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"a.toml\"]\n[bench.cat]\nscript_file = \"cat.lua\"\n",
                ),
                (
                    "a.toml",
                    "[bench.cat]\nscript = \"function bench() end\"\ntarget = \"t\"\n",
                ),
            ],
        );
        let cat = &benches_of(&dir).unwrap()["cat"];
        assert!(matches!(cat.script, BenchScript::File { .. }));
        assert_eq!(cat.target.as_deref(), Some("t"));

        // One file giving both is refused, whatever the layers beneath hold.
        let both = tree(
            "script_slot_both",
            &[
                (
                    "brokkr.toml",
                    "project = \"p\"\ninclude = [\"a.toml\"]\n[bench.cat]\nscript = \"x = 1\"\nscript_file = \"cat.lua\"\n",
                ),
                ("a.toml", "[bench.cat]\nscript = \"function bench() end\"\n"),
            ],
        );
        let err = benches_of(&both).unwrap_err();
        assert!(err.contains("both script and script_file"), "{err}");
    }

    #[test]
    fn a_bench_needs_exactly_one_nonblank_script_and_known_fields() {
        assert!(err_of("no_script", "[bench.cat]\ntarget = \"t\"\n").contains("needs script"));
        assert!(err_of("blank_script", "[bench.cat]\nscript = \"  \"\n").contains("blank"));
        assert!(
            err_of(
                "unknown_field",
                "[bench.cat]\nscript = \"x = 1\"\nexpect = 1\n"
            )
            .contains("expect")
        );
        assert!(
            err_of(
                "blank_target",
                "[bench.cat]\nscript = \"x = 1\"\ntarget = \"\"\n"
            )
            .contains("target")
        );
        assert!(
            err_of(
                "bad_profile",
                "[bench.cat]\nscript = \"x = 1\"\nprofile = \"debug\"\n"
            )
            .contains("debug")
        );
        assert!(err_of("bad_name", "[bench.\"a b\"]\nscript = \"x = 1\"\n").contains("bench name"));
        assert!(err_of("not_table", "bench = 1\n").contains("table"));
    }

    #[test]
    fn sets_compose_into_a_bench_and_the_bench_overrides_whole() {
        let benches = single(
            "uses",
            "[bench_params.data]\ndataset = { from = \"datasets\", default = \"denmark\" }\n\
             variant = { from = \"datasets.{dataset}.pbf\", help = \"set\" }\n\
             [bench.cat]\nscript = \"x = 1\"\nuses = [\"data\"]\n\
             params.variant = { from = \"datasets.{dataset}.pbf\", default = \"indexed\" }\n\
             params.jobs = { free = true, help = \"threads\" }\n",
        )
        .unwrap();
        let params = &benches["cat"].params;
        assert_eq!(
            params.keys().collect::<Vec<_>>(),
            ["dataset", "jobs", "variant"]
        );
        assert_eq!(params["dataset"].origin, ParamOrigin::Set("data".into()));
        // Replaced whole: the set's help does not survive the bench's own.
        assert_eq!(params["variant"].origin, ParamOrigin::Bench);
        assert_eq!(params["variant"].help, None);
        assert_eq!(params["variant"].default.as_deref(), Some("indexed"));
        let refs: Vec<&str> = params["variant"].from().unwrap().references().collect();
        assert_eq!(refs, ["dataset"]);
    }

    #[test]
    fn uses_must_name_distinct_sets_that_do_not_overlap() {
        let sets = "[bench_params.a]\nx = { free = true }\n[bench_params.b]\nx = { free = true }\n";
        let err = err_of(
            "uses_unknown",
            &format!("{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"z\"]\n"),
        );
        assert!(
            err.contains("not a [bench_params] set") && err.contains("a, b"),
            "{err}"
        );
        let err = err_of(
            "uses_twice",
            &format!("{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"a\", \"a\"]\n"),
        );
        assert!(err.contains("twice"), "{err}");
        let err = err_of(
            "uses_overlap",
            &format!("{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"a\", \"b\"]\n"),
        );
        assert!(err.contains("both used sets"), "{err}");
        // The bench declaring the param itself settles the overlap.
        single(
            "uses_overlap_settled",
            &format!(
                "{sets}[bench.c]\nscript = \"x = 1\"\nuses = [\"a\", \"b\"]\nparams.x = {{ free = true }}\n"
            ),
        )
        .unwrap();
        // A set no bench uses is not an error: a shared include may carry
        // sets for several projects.
        single("unused_set", sets).unwrap();
    }

    #[test]
    fn from_references_are_judged_on_the_effective_params() {
        // A set referencing a param it does not declare is fine once the
        // bench supplies it.
        single(
            "ref_supplied",
            "[bench_params.v]\nvariant = { from = \"datasets.{dataset}.pbf\" }\n\
             [bench.c]\nscript = \"x = 1\"\nuses = [\"v\"]\nparams.dataset = { from = \"datasets\" }\n",
        )
        .unwrap();
        let err = err_of(
            "ref_unknown",
            "[bench.c]\nscript = \"x = 1\"\nparams.variant = { from = \"datasets.{dataset}.pbf\" }\n",
        );
        assert!(err.contains("not a param of this bench"), "{err}");
        let err = err_of(
            "ref_cycle",
            "[bench.c]\nscript = \"x = 1\"\nparams.a = { from = \"x.{b}\" }\nparams.b = { from = \"y.{a}\" }\n",
        );
        assert!(
            err.contains("cycle") && err.contains("a -> b -> a"),
            "{err}"
        );
        let err = err_of(
            "ref_many",
            "[bench.c]\nscript = \"x = 1\"\nparams.osc = { from = \"o\", many = true }\n\
             params.v = { from = \"x.{osc}\" }\n",
        );
        assert!(err.contains("collection"), "{err}");
        let err = err_of(
            "ref_mixed",
            "[bench.c]\nscript = \"x = 1\"\nparams.v = { from = \"x.a{b}\" }\n",
        );
        assert!(err.contains("whole segment"), "{err}");
        let err = err_of(
            "ref_empty",
            "[bench.c]\nscript = \"x = 1\"\nparams.v = { from = \"x..y\" }\n",
        );
        assert!(err.contains("empty segment"), "{err}");
    }

    #[test]
    fn param_names_are_flags_and_help_is_brokkrs() {
        let err = err_of(
            "param_help",
            "[bench.c]\nscript = \"x = 1\"\nparams.help = {}\n",
        );
        assert!(err.contains("reserved"), "{err}");
        let err = err_of(
            "param_upper",
            "[bench.c]\nscript = \"x = 1\"\nparams.Jobs = {}\n",
        );
        assert!(err.contains("--flag"), "{err}");
        let err = err_of(
            "param_field",
            "[bench.c]\nscript = \"x = 1\"\nparams.j = { flag = \"x\" }\n",
        );
        assert!(err.contains("flag"), "{err}");
        let err = err_of(
            "param_required_default",
            "[bench.c]\nscript = \"x = 1\"\nparams.j = { free = true, required = true, default = \"1\" }\n",
        );
        assert!(err.contains("required and default"), "{err}");
        // Judged on the declaration, so in a set no bench uses too.
        let err = err_of(
            "set_required_default",
            "[bench_params.s]\nj = { free = true, required = true, default = \"1\" }\n",
        );
        assert!(err.contains("required and default"), "{err}");
        let benches = single(
            "param_required",
            "[bench.c]\nscript = \"x = 1\"\nparams.j = { free = true, required = true }\n\
             params.k = { free = true }\n",
        )
        .unwrap();
        assert!(benches["c"].params["j"].required);
        assert!(!benches["c"].params["k"].required, "absent means optional");
        single(
            "param_kebab",
            "[bench.c]\nscript = \"x = 1\"\nparams.osc-range = { free = true, many = true }\n",
        )
        .unwrap();
    }

    /// Every param declares exactly one domain; `choices` lists values the
    /// command line can give, and a default is one of them.
    #[test]
    fn every_param_declares_one_domain() {
        let benches = single(
            "domains",
            "[bench.c]\nscript = \"x = 1\"\nparams.cache = { choices = [\"cold\", \"warm\"] }\n\
             params.row = { validated = true }\nparams.label = { free = true }\n\
             params.dataset = { from = \"datasets\" }\n",
        )
        .unwrap();
        let params = &benches["c"].params;
        assert_eq!(
            params["cache"].domain,
            Domain::Choices(vec!["cold".into(), "warm".into()])
        );
        assert_eq!(params["row"].domain, Domain::Validated);
        assert_eq!(params["label"].domain, Domain::Free);
        assert!(params["dataset"].from().is_some());
        for (name, decl, needle) in [
            ("domain_none", "{}", "declares no domain"),
            ("domain_help_only", "{ help = \"h\" }", "declares no domain"),
            (
                "domain_two",
                "{ free = true, choices = [\"a\"] }",
                "choices and free, which exclude each other",
            ),
            (
                "domain_from_choices",
                "{ from = \"datasets\", choices = [\"a\"] }",
                "from and choices",
            ),
            ("choices_empty", "{ choices = [] }", "choices is empty"),
            ("choices_blank", "{ choices = [\" \"] }", "blank value"),
            (
                "choices_twice",
                "{ choices = [\"a\", \"a\"] }",
                "lists \"a\" twice",
            ),
            ("choices_comma", "{ choices = [\"a,b\"] }", "holds a ','"),
            (
                "choices_default",
                "{ choices = [\"a\", \"b\"], default = \"a,c\" }",
                "holds \"c\", which is not among its choices",
            ),
            (
                "choices_default_twice",
                "{ choices = [\"a\"], default = \"a,a\" }",
                "gives \"a\" twice",
            ),
            (
                "choices_unsplit_default",
                "{ choices = [\"a,b\"], split = false, default = \"a\" }",
                "holds \"a\", which is not among its choices",
            ),
            (
                "choices_many_range",
                "{ choices = [\"a..b\"], many = true }",
                "a collection reads as a range",
            ),
        ] {
            let err = err_of(
                name,
                &format!("[bench.c]\nscript = \"x = 1\"\nparams.p = {decl}\n"),
            );
            assert!(err.contains(needle), "{name}: {err}");
        }
        // Under `split = false` a choice may hold a `,`.
        let benches = single(
            "choices_unsplit",
            "[bench.c]\nscript = \"x = 1\"\nparams.p = { choices = [\"a,b\"], split = false, \
             default = \"a,b\" }\n",
        )
        .unwrap();
        assert!(!benches["c"].params["p"].split);
        // `false` is no declaration.
        let err = err_of(
            "domain_false",
            "[bench.c]\nscript = \"x = 1\"\nparams.p = { free = false }\n",
        );
        assert!(err.contains("declares no domain"), "{err}");
    }

    #[test]
    fn typed_domains_parse_their_bounds_and_judge_their_defaults() {
        let benches = single(
            "typed",
            "[bench.c]\nscript = \"x = 1\"\n\
             params.jobs = { integer = \"u32\", min = 1, default = \"1,2,4\" }\n\
             params.seq = { integer = \"u64\" }\n\
             params.scale = { number = true, min = 0, max = 1.5 }\n\
             params.box = { bbox = true, default = \"9.5,54.5,10,55\" }\n",
        )
        .unwrap();
        let params = &benches["c"].params;
        let Domain::Integer(jobs) = &params["jobs"].domain else {
            panic!("{:?}", params["jobs"].domain)
        };
        assert_eq!(
            (jobs.min, jobs.max, jobs.signed),
            (1, i128::from(u32::MAX), false)
        );
        assert!(params["jobs"].split);
        assert!(!params["box"].split, "a bbox takes each occurrence whole");
        let seq = &params["seq"].domain;
        assert!(seq.admit("18446744073709551615").is_ok());
        assert!(
            seq.admit("18446744073709551616")
                .unwrap_err()
                .contains("outside")
        );
        for spelling in ["+1", "01", "-1", " 1", "", "1.0"] {
            assert!(seq.admit(spelling).is_err(), "{spelling:?}");
        }
        let signed = integer_domain("x", "i8", None, None)
            .map(Domain::Integer)
            .unwrap();
        assert!(signed.admit("-128").is_ok());
        assert!(signed.admit("-0").is_err());
        assert!(signed.admit("-129").is_err());
        let scale = &params["scale"].domain;
        assert!(scale.admit("1.5").is_ok());
        assert!(scale.admit("1e0").is_ok());
        for bad in ["1.6", "-0.1", "inf", "NaN", "x"] {
            assert!(scale.admit(bad).is_err(), "{bad:?}");
        }
        let bbox = &params["box"].domain;
        for bad in ["1,2,3", "200,0,201,1", "10,0,9,1", "0,0,nan,1"] {
            assert!(bbox.admit(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn typed_declarations_are_refused_at_load() {
        for (name, decl, needle) in [
            ("int_width", "{ integer = \"u128\" }", "is not a width"),
            (
                "int_min_neg",
                "{ integer = \"u8\", min = -1 }",
                "not an integer within u8",
            ),
            (
                "int_max_wide",
                "{ integer = \"u8\", max = 256 }",
                "not an integer within u8",
            ),
            (
                "int_order",
                "{ integer = \"u8\", min = 5, max = 4 }",
                "above max",
            ),
            (
                "int_float_bound",
                "{ integer = \"u8\", max = 1.5 }",
                "not an integer",
            ),
            (
                "int_default",
                "{ integer = \"u8\", default = \"1,300\" }",
                "holds \"300\"",
            ),
            (
                "int_default_twice",
                "{ integer = \"u8\", default = \"1,1\" }",
                "twice",
            ),
            (
                "num_bound",
                "{ number = true, max = 9007199254740993 }",
                "exactly",
            ),
            (
                "num_order",
                "{ number = true, min = 2, max = 1 }",
                "above max",
            ),
            ("bounds_free", "{ free = true, min = 1 }", "neither"),
            ("bbox_split", "{ bbox = true, split = true }", "never split"),
            (
                "bbox_default",
                "{ bbox = true, default = \"1,2,3\" }",
                "is not a bbox",
            ),
            (
                "typed_two",
                "{ bbox = true, number = true }",
                "exclude each other",
            ),
        ] {
            let err = err_of(
                name,
                &format!("[bench.c]\nscript = \"x = 1\"\nparams.p = {decl}\n"),
            );
            assert!(err.contains(needle), "{name}: {err}");
        }
    }

    #[test]
    fn annotations_load_and_an_invalid_one_refuses() {
        let benches = single(
            "annotations",
            "[bench.cat]\nscript = \"x = 1\"\ndescription = \"d\"\nversion = 2\n\
             date = 2026-11-08\nchangelog = [{ note = \"n\" }]\nparams.changelog = { free = true }\n",
        )
        .unwrap();
        let notes = &benches["cat"].annotations;
        assert_eq!(notes.version.value(), Some(&2));
        assert_eq!(notes.changelog.value().map(Vec::len), Some(1));
        assert!(benches["cat"].params.contains_key("changelog"));
        let err = err_of(
            "annotations_quoted_date",
            "[bench.cat]\nscript = \"x = 1\"\ndate = \"2026-11-08\"\n",
        );
        assert!(
            err.contains("[bench.cat]") && err.contains("local date"),
            "{err}"
        );
    }

    /// A layer setting a bench's changelog replaces the one beneath whole,
    /// empty or not, and is recorded as the value's origin; a `[[check]]`
    /// style join is left to every other array of tables.
    #[test]
    fn a_changelog_is_replaced_whole_across_layers() {
        let log = |body: &str| -> (Vec<String>, Option<PathBuf>) {
            let dir = tree(
                &format!("changelog_layers_{}", body.len()),
                &[
                    (
                        "brokkr.toml",
                        &format!("project = \"p\"\ninclude = [\"a.toml\"]\n[bench.cat]\n{body}"),
                    ),
                    (
                        "a.toml",
                        "include = [\"b.toml\"]\n[bench.cat]\n\
                         changelog = [{ note = \"a2\" }, { note = \"a1\" }]\n",
                    ),
                    (
                        "b.toml",
                        "[bench.cat]\nscript = \"x = 1\"\nchangelog = [{ note = \"b\" }]\n",
                    ),
                ],
            );
            let (_, cfg, _, sources) = super::super::load_with_sources(&dir).unwrap();
            let notes: Vec<String> = cfg.benches["cat"]
                .annotations
                .changelog
                .value()
                .map(|l| l.iter().map(|e| e.note.clone()).collect())
                .unwrap_or_default();
            let origin = sources
                .value_origin(&["bench", "cat", "changelog"])
                .map(|o| o.file.to_path_buf());
            (notes, origin)
        };
        let (notes, origin) = log("changelog = [{ note = \"top\" }]\n");
        assert_eq!(notes, ["top"]);
        assert!(origin.unwrap().ends_with("brokkr.toml"));
        let (notes, origin) = log("");
        assert_eq!(notes, ["a2", "a1"]);
        assert!(origin.unwrap().ends_with("a.toml"));
        let (notes, _) = log("changelog = []\n");
        assert!(notes.is_empty());
    }

    #[test]
    fn data_is_an_uninterpreted_table() {
        // Including a `datasets` table holding `sha256`, which in a host
        // section is the retired digest key.
        single(
            "data_opaque",
            "[data.datasets.x]\nsha256 = \"project data\"\n[data.workloads.w]\nanything = [1, 2]\n",
        )
        .unwrap();
        assert!(err_of("data_scalar", "data = 1\n").contains("data must be a table"));
    }

    #[test]
    fn hosts_named_like_the_bench_sections_use_the_host_namespace() {
        let dir = tree(
            "host_namespace",
            &[(
                "brokkr.toml",
                "project = \"p\"\n[host.data]\nport = 1\n[host.bench]\nport = 2\n",
            )],
        );
        let (_, cfg, _) = super::super::load(&dir).unwrap();
        let hosts: HashMap<&str, Option<u16>> = cfg
            .hosts
            .iter()
            .map(|(name, host)| (name.as_str(), host.port))
            .collect();
        assert_eq!(hosts["data"], Some(1));
        assert_eq!(hosts["bench"], Some(2));
        assert!(cfg.benches.is_empty());
    }
}
