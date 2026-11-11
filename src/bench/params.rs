//! A bench's params: read from the tokens after NAME, bound against the
//! resolved view (`super::view`) in dependency order, their files digested
//! in preflight, and handed on as the document's `params`, the iteration's
//! inputs and `bench(params, keys)`'s arguments (`docs/brokkr.toml.bench.md`,
//! Binding params).
//!
//! Every refusal here needs the command line or the resolved config, so it
//! happens when a bench runs, never at load: what a declaration alone can
//! refuse was refused when the config loaded.
//!
//! A param's values are every occurrence on the command line (else its
//! `default`), each split on `,` unless the param sets `split = false`. A `many` param binds them all as one
//! collection; any other param given several fans out, one arm per value,
//! and [`bind`] returns every arm, dependents resolved per parent binding.
//! Every refusal across every arm is collected before any is returned, so
//! one invocation is refused whole or not at all.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use super::lua;
use super::store;
use super::view::{self, FileNode, Segments, View};
use crate::config::ConfigSources;
use crate::config::bench::{Bench, Domain, FromSegment, Param};
use crate::error::DevError;

/// What the command line gave after NAME.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Given {
    /// `--help` or `-h` stood where a param's name could.
    pub(crate) help: bool,
    /// Each param's occurrences, in command-line order.
    values: BTreeMap<String, Vec<String>>,
}

/// Read the tokens after NAME: `--NAME VALUE` or `--NAME=VALUE`, a param as
/// often as it is given; `--help` or `-h` wherever a param's name could
/// stand. A token taken as a value is data, whatever it looks like.
pub(crate) fn parse(bench: &Bench, tokens: &[String]) -> Result<Given, DevError> {
    let usage = |why: String| DevError::Usage(format!("bench {}: {why}", bench.name));
    let mut given = Given::default();
    let mut tokens = tokens.iter();
    while let Some(token) = tokens.next() {
        if token == "--help" || token == "-h" {
            given.help = true;
            continue;
        }
        let Some(flag) = token.strip_prefix("--").filter(|f| !f.is_empty()) else {
            return Err(usage(format!(
                "{token:?} is not a param: a param is --NAME VALUE, and brokkr's own options go \
                 before the bench's name"
            )));
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (flag, None),
        };
        if !bench.params.contains_key(name) {
            return Err(usage(format!(
                "no param --{name}; {}",
                if bench.params.is_empty() {
                    "it declares none".to_owned()
                } else {
                    format!(
                        "its params: {}",
                        bench
                            .params
                            .keys()
                            .map(|p| format!("--{p}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            )));
        }
        let value = match inline {
            Some(value) => value,
            None => tokens
                .next()
                .cloned()
                .ok_or_else(|| usage(format!("--{name} needs a value")))?,
        };
        // A blank value is refused when it binds, beside every other
        // refusal of the invocation.
        given.values.entry(name.to_owned()).or_default().push(value);
    }
    Ok(given)
}

/// One item of a param's values, before it meets its table.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Key(String),
    /// A `from` param's `all`: every key of its table.
    All,
    /// A `from` collection's `LO..HI`, both ends canonical decimals.
    Range {
        lo: u64,
        hi: u64,
    },
}

/// Whether `key` is a canonical decimal: digits only, no sign, no leading
/// zero (`0` itself is one).
pub(crate) fn canonical_decimal(key: &str) -> bool {
    !key.is_empty()
        && key.bytes().all(|b| b.is_ascii_digit())
        && (key == "0" || !key.starts_with('0'))
}

/// A table's keys in key order: numeric when every key is a canonical
/// decimal (by length, then digits, so no key is too long to order), byte
/// order otherwise. `all` expands in this order, and every key list brokkr
/// prints uses it.
pub(crate) fn key_order(table: &toml::Table) -> Vec<String> {
    let mut keys: Vec<String> = table.keys().cloned().collect();
    if keys.iter().all(|k| canonical_decimal(k)) {
        keys.sort_unstable_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    } else {
        keys.sort_unstable();
    }
    keys
}

/// Split a param's occurrences into items. What a value means depends on
/// the param: `all` is every key only for a `from` param (a plain param's
/// `all` is the string), and `..` is a range only for a `from` collection (a
/// plain collection has no keys to range over, and any other param's `..` is
/// part of its key).
fn items(param: &Param, raw: &[String]) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    for occurrence in raw {
        if occurrence.trim().is_empty() {
            return Err("is blank: give a value".into());
        }
        // An unsplit param takes the occurrence whole, commas and all.
        let pieces: Vec<&str> = if param.split {
            occurrence.split(',').collect()
        } else {
            vec![occurrence.as_str()]
        };
        for item in pieces {
            if item.trim().is_empty() {
                return Err("has a blank item: every value between commas must be given".into());
            }
            if param.from().is_some() && item == "all" {
                items.push(Item::All);
            } else if param.many && item.contains("..") {
                if param.from().is_none() {
                    return Err(format!(
                        "holds the range {item:?}, which only a collection with a `from` path \
                         takes: a plain collection has no keys to range over"
                    ));
                }
                let (lo, hi) = item.split_once("..").unwrap_or((item, ""));
                if !canonical_decimal(lo) || !canonical_decimal(hi) {
                    return Err(format!(
                        "holds the range {item:?}, whose ends must be decimal keys with no sign \
                         or leading zero (LO..HI)"
                    ));
                }
                let (Ok(lo), Ok(hi)) = (lo.parse::<u64>(), hi.parse::<u64>()) else {
                    return Err(format!(
                        "holds the range {item:?}, whose ends must be at most {}",
                        u64::MAX
                    ));
                };
                if lo > hi {
                    return Err(format!("holds the range {item:?}, which runs backwards"));
                }
                items.push(Item::Range { lo, hi });
            } else {
                items.push(Item::Key(item.to_owned()));
            }
        }
    }
    if items.len() > 1 && items.contains(&Item::All) {
        return Err("gives `all` beside other values; `all` already names every key".into());
    }
    Ok(items)
}

/// The keys `items` select from `table` (at `path`), in item order, ranges
/// ascending in place and `all` in key order; a key that is not there is
/// refused, every missing one named.
fn select(table: &toml::Table, path: &[String], items: &[Item]) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for item in items {
        match item {
            Item::Key(key) if table.contains_key(key) => keys.push(key.clone()),
            Item::Key(key) => missing.push(format!("{key:?}")),
            Item::All => keys.extend(key_order(table)),
            Item::Range { lo, hi } => {
                let mut present: Vec<u64> = table
                    .keys()
                    .filter(|k| canonical_decimal(k))
                    .filter_map(|k| k.parse::<u64>().ok())
                    .filter(|k| (lo..=hi).contains(&k))
                    .collect();
                present.sort_unstable();
                let width = u128::from(*hi) - u128::from(*lo) + 1;
                let found = u128::try_from(present.len()).unwrap_or(u128::MAX);
                if found == width {
                    keys.extend(present.iter().map(u64::to_string));
                    continue;
                }
                // The gaps, from the keys that are there: never a walk over
                // the whole range, which may be 2^64 wide.
                const SHOWN: usize = 5;
                let mut gaps = Vec::new();
                let mut next = u128::from(*lo);
                for key in present
                    .iter()
                    .map(|k| u128::from(*k))
                    .chain([u128::from(*hi) + 1])
                {
                    while next < key && gaps.len() < SHOWN {
                        gaps.push(next.to_string());
                        next += 1;
                    }
                    if gaps.len() == SHOWN {
                        break;
                    }
                    next = key + 1;
                }
                let more = width - found - u128::try_from(gaps.len()).unwrap_or(0);
                let mut shown = gaps.join(", ");
                if more > 0 {
                    shown.push_str(&format!(" and {more} more"));
                }
                missing.push(format!("{lo}..{hi} (lacking {shown})"));
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "selects {}, not {} under {} (valid: {})",
            missing.join(", "),
            if missing.len() == 1 { "a key" } else { "keys" },
            view::shown(path),
            keys_of(table)
        ));
    }
    refuse_duplicates(&keys)?;
    Ok(keys)
}

fn refuse_duplicates(keys: &[String]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for key in keys {
        if !seen.insert(key.as_str()) {
            return Err(format!("selects {key:?} twice"));
        }
    }
    Ok(())
}

/// The params in an order where every `{param}` a `from` path references
/// comes before the param naming it. Load refused cycles.
fn order(bench: &Bench) -> Vec<&str> {
    fn visit<'a>(
        name: &'a str,
        params: &'a BTreeMap<String, Param>,
        done: &mut BTreeSet<&'a str>,
        out: &mut Vec<&'a str>,
    ) {
        if !done.insert(name) {
            return;
        }
        if let Some(from) = params.get(name).and_then(Param::from) {
            for reference in from.references() {
                if let Some((key, _)) = params.get_key_value(reference) {
                    visit(key, params, done, out);
                }
            }
        }
        out.push(name);
    }
    let mut done = BTreeSet::new();
    let mut out = Vec::new();
    for name in bench.params.keys() {
        visit(name, &bench.params, &mut done, &mut out);
    }
    out
}

/// One bound param.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Binding {
    many: bool,
    /// The `from` path with every `{param}` substituted; `None` for a plain
    /// param.
    from: Option<Segments>,
    values: Vec<BoundValue>,
}

#[derive(Debug, Clone, PartialEq)]
struct BoundValue {
    key: String,
    /// What the script receives, before any handle replaces a file's path.
    entry: lua::Value,
    /// The entry, when it is a file entry: the run's input.
    file: Option<FileNode>,
    /// The file's digest, once preflight computed it.
    digest: Option<String>,
}

/// Every bound param, by name. An unbound param is absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Bound {
    params: BTreeMap<String, Binding>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Given,
    Default,
}

/// A param's occurrences as the command line would spell them: an unsplit
/// param's commas are its own, so its occurrences stay separate flags.
fn spelled(name: &str, param: &Param, raw: &[String]) -> String {
    if param.split {
        raw.join(",")
    } else {
        raw.join(&format!(" --{name} "))
    }
}

/// A refusal, with whether it came from the command line or a default.
struct Refusal {
    message: String,
    source: Source,
}

/// One refusal as itself; several as one preflight list, so a whole
/// invocation names everything wrong with it at once.
fn refused(refusals: Vec<Refusal>) -> DevError {
    let mut seen = BTreeSet::new();
    let mut distinct: Vec<Refusal> = refusals
        .into_iter()
        .filter(|r| seen.insert(r.message.clone()))
        .collect();
    if distinct.len() == 1 {
        let only = distinct.remove(0);
        return match only.source {
            Source::Given => DevError::Usage(only.message),
            Source::Default => DevError::Config(only.message),
        };
    }
    DevError::Preflight(distinct.into_iter().map(|r| r.message).collect())
}

/// An arm being expanded: what is bound so far, and the params that were
/// refused in it, whose dependents are then not judged.
#[derive(Clone)]
struct Partial {
    bound: Bound,
    unavailable: BTreeSet<String>,
}

/// Bind every param that has a value, from the command line or its
/// `default`, in dependency order, into every arm the values fan out to:
/// parents outer, each param's values in their order; a param with neither
/// is unbound, unless it is `required`, which refuses its absence. Never
/// empty when it succeeds; one arm when nothing fans out.
pub(crate) fn bind(bench: &Bench, given: &Given, view: &View) -> Result<Vec<Bound>, DevError> {
    expand(bench, given, view, |_| true).map_err(refused)
}

/// [`bind`] over the params `include` admits, its refusals unrendered.
fn expand(
    bench: &Bench,
    given: &Given,
    view: &View,
    include: impl Fn(&str) -> bool,
) -> Result<Vec<Bound>, Vec<Refusal>> {
    let mut refusals = Vec::new();
    let mut partials = vec![Partial {
        bound: Bound::default(),
        unavailable: BTreeSet::new(),
    }];
    for name in order(bench) {
        if !include(name) {
            continue;
        }
        let param = &bench.params[name];
        // `default_only`: absent from the command line, the param binds the
        // one key its table holds in each arm, judged there below.
        let sole = param.default_only && !given.values.contains_key(name);
        let (raw, source): (Vec<String>, Source) = match (given.values.get(name), &param.default) {
            (Some(values), _) => (values.clone(), Source::Given),
            (None, Some(default)) => (vec![default.clone()], Source::Default),
            (None, None) if sole => (vec!["all".to_owned()], Source::Default),
            (None, None) if param.required => {
                refusals.push(absent(bench, name, param, &mut partials, view));
                continue;
            }
            (None, None) => continue,
        };
        let value = spelled(name, param, &raw);
        let refuse = |why: String| refusal(&bench.name, name, &value, source, &why);
        let items = match items(param, &raw) {
            Ok(items) => items,
            Err(why) => {
                refusals.push(refuse(why));
                for partial in &mut partials {
                    partial.unavailable.insert(name.to_owned());
                }
                continue;
            }
        };
        let mut next = Vec::with_capacity(partials.len());
        for mut partial in partials {
            let blocked = param
                .from()
                .is_some_and(|from| from.references().any(|p| partial.unavailable.contains(p)));
            if blocked {
                // Its parent was refused in this arm: judging it there would
                // only invent a refusal.
                partial.unavailable.insert(name.to_owned());
                next.push(partial);
                continue;
            }
            let resolved = resolve(name, param, &items, &partial.bound, view)
                .map_err(refuse)
                .and_then(|(from, values)| {
                    if sole && values.len() != 1 {
                        Err(not_sole(bench, name, from.as_deref(), &values))
                    } else {
                        Ok((from, values))
                    }
                });
            match resolved {
                Err(refusal) => {
                    refusals.push(refusal);
                    partial.unavailable.insert(name.to_owned());
                    next.push(partial);
                }
                Ok((from, values)) if param.many => {
                    partial.bound.params.insert(
                        name.to_owned(),
                        Binding {
                            many: true,
                            from,
                            values,
                        },
                    );
                    next.push(partial);
                }
                Ok((from, values)) if values.is_empty() => {
                    refusals.push(refuse(format!(
                        "selects no key: {} has none, so there is nothing to run",
                        from.as_deref().map_or_else(String::new, view::shown)
                    )));
                    partial.unavailable.insert(name.to_owned());
                    next.push(partial);
                }
                Ok((from, values)) => {
                    for value in values {
                        let mut arm = partial.clone();
                        arm.bound.params.insert(
                            name.to_owned(),
                            Binding {
                                many: false,
                                from: from.clone(),
                                values: vec![value],
                            },
                        );
                        next.push(arm);
                    }
                }
            }
        }
        partials = next;
    }
    if refusals.is_empty() {
        Ok(partials.into_iter().map(|p| p.bound).collect())
    } else {
        Err(refusals)
    }
}

/// `--name value` refused for `why`, worded for where the value came from.
fn refusal(bench: &str, name: &str, value: &str, source: Source, why: &str) -> Refusal {
    Refusal {
        message: match source {
            Source::Given => format!("bench {bench}: --{name} {value} {why}"),
            Source::Default => format!(
                "bench {bench}: --{name}'s default {value:?} {why}; give --{name} on the command \
                 line or fix the default"
            ),
        },
        source,
    }
}

/// A `default_only` param whose table, in one arm, holds no key or several:
/// the command line must choose, and the keys are named.
fn not_sole(bench: &Bench, name: &str, from: Option<&[String]>, values: &[BoundValue]) -> Refusal {
    let keys: Vec<&str> = values.iter().map(|v| v.key.as_str()).collect();
    let held = if keys.is_empty() {
        "none".to_owned()
    } else {
        keys.join(", ")
    };
    Refusal {
        message: format!(
            "bench {}: --{name} defaults to the only key of {}, which holds {} ({held}); give \
             --{name} on the command line",
            bench.name,
            from.map_or_else(String::new, view::shown),
            keys.len(),
        ),
        source: Source::Given,
    }
}

/// The one refusal of a `required` param the command line did not give. For
/// a `from` param it lists the keys under each distinct path the arms'
/// parents lead to, in arm order, as detail only: an arm whose parent was
/// refused, or whose path leads nowhere, adds nothing, so neither the number
/// of paths nor a failed lookup changes what is refused or how.
///
/// The param is absent whatever its parents bind, so it is reported even in
/// an arm whose parent was refused; it is made unavailable in every arm, so
/// its dependents are judged in none.
fn absent(
    bench: &Bench,
    name: &str,
    param: &Param,
    partials: &mut [Partial],
    view: &View,
) -> Refusal {
    let message = absence(bench, name, param, partials, view);
    for partial in partials {
        partial.unavailable.insert(name.to_owned());
    }
    Refusal {
        message,
        source: Source::Given,
    }
}

fn absence(bench: &Bench, name: &str, param: &Param, partials: &[Partial], view: &View) -> String {
    let mut message = format!(
        "bench {}: --{name} is required: give it on the command line",
        bench.name
    );
    if let Domain::Choices(choices) = &param.domain {
        message.push_str(&format!("\n  choices: {}", choices.join(", ")));
    }
    if let Some(from) = param.from() {
        let mut listed: Vec<String> = Vec::new();
        for partial in partials {
            if from.references().any(|p| partial.unavailable.contains(p)) {
                continue;
            }
            let chosen = |p: &str| {
                partial
                    .bound
                    .params
                    .get(p)
                    .and_then(|b| b.values.first())
                    .map(|v| v.key.clone())
            };
            let Ok(path) = substitute(name, &from.segments, chosen) else {
                continue;
            };
            let Ok(table) = walk(view.table(), &path) else {
                continue;
            };
            let shown = view::shown(&path);
            if listed.contains(&shown) {
                continue;
            }
            message.push_str(&if table.is_empty() {
                format!("\n  no keys under {shown} on this host")
            } else {
                format!("\n  valid under {shown}: {}", keys_of(table))
            });
            listed.push(shown);
        }
    }
    message
}

/// The values `items` select for `name` in one arm, and its substituted
/// `from` path.
fn resolve(
    name: &str,
    param: &Param,
    items: &[Item],
    bound: &Bound,
    view: &View,
) -> Result<(Option<Segments>, Vec<BoundValue>), String> {
    let Some(from) = param.from() else {
        let keys: Vec<String> = items
            .iter()
            .map(|item| match item {
                Item::Key(key) => key.clone(),
                // Neither reaches a plain param: `items` reads its `all` as
                // a key and refuses its ranges.
                Item::All => "all".to_owned(),
                Item::Range { lo, hi } => format!("{lo}..{hi}"),
            })
            .collect();
        refuse_duplicates(&keys)?;
        // A typed domain judges each item; the value still reaches the
        // script as the text given.
        for key in &keys {
            param
                .domain
                .admit(key)
                .map_err(|why| format!("holds {key:?}, which {why}"))?;
        }
        if let Domain::Choices(choices) = &param.domain {
            let wrong: Vec<String> = keys
                .iter()
                .filter(|key| !choices.contains(key))
                .map(|key| format!("{key:?}"))
                .collect();
            if !wrong.is_empty() {
                return Err(format!(
                    "holds {}, not among its choices: {}",
                    wrong.join(", "),
                    choices.join(", ")
                ));
            }
        }
        return Ok((
            None,
            keys.into_iter()
                .map(|key| BoundValue {
                    entry: lua::Value::Text(key.clone()),
                    key,
                    file: None,
                    digest: None,
                })
                .collect(),
        ));
    };
    let path = substitute(name, &from.segments, |p| {
        bound
            .params
            .get(p)
            .and_then(|b| b.values.first())
            .map(|v| v.key.clone())
    })?;
    let table = walk(view.table(), &path)?;
    let mut values = Vec::new();
    for key in select(table, &path, items)? {
        let at: Segments = [path.clone(), vec![key.clone()]].concat();
        let file = view.file(&at).cloned();
        if let Some(file) = &file
            && file.path.to_str().is_none()
        {
            return Err(format!(
                "names {}, which is not UTF-8 and a script cannot pass on",
                file.path.display()
            ));
        }
        // `select` returns only keys of this table.
        let Some(entry) = table.get(&key) else {
            return Err(format!("lost the key {key:?} it selected"));
        };
        values.push(BoundValue {
            entry: view::value(entry),
            key,
            file,
            digest: None,
        });
    }
    Ok((Some(path), values))
}

/// `segments` with each `{param}` replaced by that param's chosen key, which
/// `chosen` gives. A substituted key holding a dot is refused: the stored path
/// is its segments joined by dots.
fn substitute(
    name: &str,
    segments: &[FromSegment],
    chosen: impl Fn(&str) -> Option<String>,
) -> Result<Segments, String> {
    let shown_from = segments
        .iter()
        .map(|s| match s {
            FromSegment::Key(key) => key.clone(),
            FromSegment::Param(p) => format!("{{{p}}}"),
        })
        .collect::<Vec<_>>()
        .join(".");
    segments
        .iter()
        .map(|segment| match segment {
            FromSegment::Key(key) => Ok(key.clone()),
            FromSegment::Param(p) => {
                let key = chosen(p).ok_or_else(|| {
                    format!("selects under {shown_from}, which needs --{p}, and --{p} has no value")
                })?;
                if key.contains('.') {
                    return Err(format!(
                        "selects under {shown_from}, and --{p}'s key {key:?} holds a '.', which a \
                         key substituted into a path cannot (--{name} could not be stored \
                         unambiguously)"
                    ));
                }
                Ok(key)
            }
        })
        .collect()
}

/// The table at `path` in the view.
fn walk<'a>(table: &'a toml::Table, path: &[String]) -> Result<&'a toml::Table, String> {
    let mut current = table;
    for (i, segment) in path.iter().enumerate() {
        let here = || {
            if i == 0 {
                "the config".to_owned()
            } else {
                view::shown(&path[..i])
            }
        };
        match current.get(segment) {
            Some(toml::Value::Table(inner)) => current = inner,
            Some(other) => {
                return Err(format!(
                    "selects under {}, but {} is a {}, not a table of values",
                    view::shown(path),
                    view::shown(&path[..=i]),
                    other.type_str()
                ));
            }
            None => {
                return Err(format!(
                    "selects under {}, but {} has no key {segment:?} (keys: {})",
                    view::shown(path),
                    here(),
                    keys_of(current)
                ));
            }
        }
    }
    Ok(current)
}

fn keys_of(table: &toml::Table) -> String {
    let keys = key_order(table);
    if keys.is_empty() {
        return "none".to_owned();
    }
    keys.join(", ")
}

/// Every arm's bound files, each once, in first-seen order: what the run's
/// read hold reserves. Each arm keeps its own ordered [`Bound::inputs`],
/// which its handles index.
pub(crate) fn union_inputs(arms: &[Bound]) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    arms.iter()
        .flat_map(Bound::inputs)
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// Digest every arm's bound files, every refusal across the arms collected.
/// Each binding's shape and pin are judged on their own, since two entries
/// may name one file with different pins; only a file's content hash is
/// computed once.
pub(crate) fn digest_arms(
    arms: &mut [Bound],
    depth: Depth,
    project_root: &Path,
    sources: &ConfigSources,
) -> Result<(), DevError> {
    let mut hashed = HashMap::new();
    let mut errors = Vec::new();
    for arm in arms {
        arm.digest_with(depth, project_root, sources, &mut hashed, &mut errors);
    }
    digest_refused(errors)
}

/// One digest refusal as itself; several, once each, as one preflight list.
fn digest_refused(mut errors: Vec<DevError>) -> Result<(), DevError> {
    if errors.len() <= 1 {
        return errors.pop().map_or(Ok(()), Err);
    }
    let mut seen = BTreeSet::new();
    let messages: Vec<String> = errors
        .iter()
        .flat_map(|error| match error {
            DevError::Preflight(lines) => lines.clone(),
            DevError::Config(message) | DevError::Usage(message) => vec![message.clone()],
            other => vec![other.to_string()],
        })
        .filter(|m| seen.insert(m.clone()))
        .collect();
    if messages.len() == 1 {
        return errors.pop().map_or(Ok(()), Err);
    }
    Err(DevError::Preflight(messages))
}

/// For each arm, the params that fan out, as `name=key`, space separated:
/// the params whose key differs between arms. Empty for a single arm.
pub(crate) fn arm_labels(arms: &[Bound]) -> Vec<String> {
    let Some(first) = arms.first() else {
        return Vec::new();
    };
    let varying: Vec<&String> = first
        .params
        .iter()
        .filter(|(name, binding)| {
            !binding.many
                && arms.iter().any(|arm| {
                    arm.params.get(*name).map(|b| &b.values[..]).map(keys_vec)
                        != Some(keys_vec(&binding.values))
                })
        })
        .map(|(name, _)| name)
        .collect();
    arms.iter()
        .map(|arm| {
            varying
                .iter()
                .filter_map(|name| {
                    let binding = arm.params.get(*name)?;
                    Some(format!("{name}={}", keys_vec(&binding.values).join(",")))
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn keys_vec(values: &[BoundValue]) -> Vec<&str> {
    values.iter().map(|v| v.key.as_str()).collect()
}

impl Bound {
    /// The bound files, in the order their handles are numbered: param name,
    /// then value order. Two values may name one file; it is listed for
    /// each.
    pub(crate) fn inputs(&self) -> Vec<PathBuf> {
        self.files().map(|file| file.path.clone()).collect()
    }

    fn files(&self) -> impl Iterator<Item = &FileNode> {
        self.params
            .values()
            .flat_map(|binding| binding.values.iter())
            .filter_map(|value| value.file.as_ref())
    }

    /// Digest every bound file: a pinned one must match its pin, an
    /// unpinned one is hashed and recorded. `sources` names where a drifted
    /// pin was set. Runs while the inputs are reserved for reading.
    #[cfg(test)]
    pub(crate) fn digest(
        &mut self,
        project_root: &Path,
        sources: &ConfigSources,
    ) -> Result<(), DevError> {
        let mut errors = Vec::new();
        self.digest_with(
            Depth::Every,
            project_root,
            sources,
            &mut HashMap::new(),
            &mut errors,
        );
        digest_refused(errors)
    }

    /// Digest every bound file: a pinned one must match its pin, an
    /// unpinned one is hashed and recorded, reusing and filling `hashed`,
    /// each file's content digest by path. Every refusal goes to `errors`,
    /// the remaining files still judged.
    fn digest_with(
        &mut self,
        depth: Depth,
        project_root: &Path,
        sources: &ConfigSources,
        hashed: &mut HashMap<PathBuf, crate::preflight::Xxh128>,
        errors: &mut Vec<DevError>,
    ) {
        for (name, binding) in &mut self.params {
            for value in &mut binding.values {
                let Some(file) = &value.file else { continue };
                match digest_one(name, &value.key, file, depth, project_root, sources, hashed) {
                    Ok(digest) => value.digest = digest,
                    Err(error) => errors.push(error),
                }
            }
        }
    }

    /// The document's `params`.
    pub(crate) fn document(&self) -> BTreeMap<String, store::BoundParam> {
        self.params
            .iter()
            .map(|(name, binding)| {
                (
                    name.clone(),
                    store::BoundParam {
                        many: binding.many,
                        from: binding.from.as_deref().map(view::shown),
                        values: binding
                            .values
                            .iter()
                            .map(|value| store::ParamValue {
                                key: value.key.clone(),
                                digest: value.digest.clone(),
                            })
                            .collect(),
                    },
                )
            })
            .collect()
    }

    /// `bench(params, keys)`'s arguments: each file entry's path replaced by
    /// its input handle, numbered as [`Bound::inputs`] lists them.
    pub(crate) fn args(&self) -> lua::Args {
        self.args_with(true)
    }

    /// `preflight(params, keys)`'s arguments: [`Bound::args`]'s shape, each
    /// file entry's field the path string a handle's `path` would hold.
    /// Preflight holds no handle: nothing it does with the path is declared.
    pub(crate) fn preflight_args(&self) -> lua::Args {
        self.args_with(false)
    }

    fn args_with(&self, handles: bool) -> lua::Args {
        let mut next = 0;
        let mut args = lua::Args::default();
        for (name, binding) in &self.params {
            let mut entries = Vec::new();
            let mut keys = Vec::new();
            for value in &binding.values {
                let mut entry = value.entry.clone();
                if let Some(file) = &value.file {
                    let handle = lua::HandleRef {
                        index: next,
                        path: file.path.to_string_lossy().into_owned(),
                    };
                    next += 1;
                    if let lua::Value::Table(fields) = &mut entry {
                        for (key, field) in fields.iter_mut() {
                            if key == file.field {
                                *field = if handles {
                                    lua::Value::Handle(handle.clone())
                                } else {
                                    lua::Value::Text(handle.path.clone())
                                };
                            }
                        }
                    }
                }
                entries.push(entry);
                keys.push(lua::Value::Text(value.key.clone()));
            }
            let (entry, key) = if binding.many {
                (lua::Value::List(entries), lua::Value::List(keys))
            } else {
                match (entries.pop(), keys.pop()) {
                    (Some(entry), Some(key)) => (entry, key),
                    _ => continue,
                }
            };
            args.params.push((name.clone(), entry));
            args.keys.push((name.clone(), key));
        }
        args
    }
}

/// How far [`digest_arms`] reads the bound files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Depth {
    /// Every file hashed, its digest recorded: a run.
    Every,
    /// Every file's shape checked, only a pinned one hashed against its pin:
    /// a dry run, which records nothing.
    PinsOnly,
}

/// One bound file's shape checked and, unless `depth` skips it, its content
/// hashed and held to its pin; the digest, when hashed.
fn digest_one(
    name: &str,
    key: &str,
    file: &FileNode,
    depth: Depth,
    project_root: &Path,
    sources: &ConfigSources,
    hashed: &mut HashMap<PathBuf, crate::preflight::Xxh128>,
) -> Result<Option<String>, DevError> {
    let what = format!("--{name} {key}");
    let pin_at = view::shown(&file.pin_at);
    let pins_only = matches!(depth, Depth::PinsOnly);
    let meta = std::fs::metadata(&file.path).map_err(|e| {
        DevError::Preflight(vec![format!(
            "{what}: {} cannot be read: {e}",
            file.path.display()
        )])
    })?;
    // A FIFO, a device or a socket is no content a digest describes (and
    // hashing `/dev/zero` never ends).
    if !(meta.is_file() || file.tree && meta.is_dir()) {
        return Err(DevError::Preflight(vec![format!(
            "{what}: {} is not a regular file{}",
            file.path.display(),
            if file.tree {
                " or a directory, which a dataset's `path` names"
            } else {
                ", which a `file` entry names"
            }
        )]));
    }
    // A dry run records nothing, so an unpinned file's digest serves no one:
    // its shape is checked, its content left unread.
    if pins_only && file.pin.is_none() {
        return Ok(None);
    }
    // The shape was judged above for this entry; only the content is shared.
    let digest = match hashed.get(&file.path) {
        Some(digest) => *digest,
        None => {
            let digest = crate::preflight::cached_path_digest(&file.path, project_root)?.value();
            hashed.insert(file.path.clone(), digest);
            digest
        }
    };
    let Some(pin) = &file.pin else {
        return Ok(Some(digest.to_string()));
    };
    let expected: crate::preflight::Xxh128 = pin.parse().map_err(|e| {
        DevError::Config(format!(
            "{what}: the pin {pin_at} = {pin:?} is not a digest: {e}"
        ))
    })?;
    if expected != digest {
        let segments: Vec<&str> = file.pin_at.iter().map(String::as_str).collect();
        let set_in = sources
            .value_origin(&segments)
            .map(|origin| format!(" in {}", origin.file.display()))
            .unwrap_or_default();
        return Err(DevError::Preflight(vec![format!(
            "{what}: hash mismatch for {}\n  expected: {expected}\n  actual:   {digest}\n  pin: \
             {pin_at}{set_in}",
            file.path.display()
        )]));
    }
    Ok(Some(digest.to_string()))
}

/// Each distinct `from` path a param's parents lead to, shown, with its keys
/// in key order.
pub(crate) type KeysByPath = Vec<(String, Vec<String>)>;

/// For `--help`: the keys `name` takes under its `from` path, in key order,
/// per distinct path its parents lead to: the parents bound as a run would
/// bind them, from the command line, else their defaults, fanned out where
/// they list several. `None` for a plain param; an error when a parent is
/// refused or the path leads nowhere, as a sentence to print.
pub(crate) fn values_for(
    bench: &Bench,
    given: &Given,
    view: &View,
    name: &str,
) -> Option<Result<KeysByPath, String>> {
    let from = bench.params.get(name)?.from()?;
    let mut ancestors = BTreeSet::new();
    let mut pending: Vec<&str> = from.references().collect();
    while let Some(p) = pending.pop() {
        if ancestors.insert(p.to_owned())
            && let Some(parent) = bench.params.get(p).and_then(Param::from)
        {
            pending.extend(parent.references());
        }
    }
    let arms = match expand(bench, given, view, |p| ancestors.contains(p)) {
        Ok(arms) => arms,
        Err(refusals) => {
            return Some(Err(refusals
                .into_iter()
                .map(|r| r.message)
                .collect::<Vec<_>>()
                .join("; ")));
        }
    };
    let mut paths: Vec<(String, Vec<String>)> = Vec::new();
    for arm in &arms {
        let chosen = |p: &str| {
            arm.params
                .get(p)
                .and_then(|b| b.values.first())
                .map(|v| v.key.clone())
        };
        let listed = substitute(name, &from.segments, chosen).and_then(|path| {
            walk(view.table(), &path).map(|table| (view::shown(&path), key_order(table)))
        });
        match listed {
            Ok(listed) if paths.iter().any(|(path, _)| *path == listed.0) => {}
            Ok(listed) => paths.push(listed),
            Err(why) => return Some(Err(format!("it {why}"))),
        }
    }
    Some(Ok(paths))
}

#[cfg(test)]
#[path = "params_tests.rs"]
mod tests;
