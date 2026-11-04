//! A bench's params: read from the tokens after NAME, bound against the
//! resolved view (`super::view`) in dependency order, their files digested
//! in preflight, and handed on as the document's `params`, the iteration's
//! inputs and `bench(params, keys)`'s arguments (`docs/brokkr.toml.bench.md`,
//! Binding params).
//!
//! Every refusal here needs the command line or the resolved config, so it
//! happens when a bench runs, never at load: what a declaration alone can
//! refuse was refused when the config loaded. Collections and fan-out are
//! not built yet, so a value spelled as a list, a range or `all` is refused
//! rather than read as one key, and no command line changes meaning when
//! they arrive.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::lua;
use super::store;
use super::view::{self, FileNode, Segments, View};
use crate::config::ConfigSources;
use crate::config::bench::{Bench, FromSegment, Param};
use crate::error::DevError;

/// What the command line gave after NAME.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Given {
    /// `--help` or `-h` stood where a param's name could.
    pub(crate) help: bool,
    values: BTreeMap<String, String>,
}

/// Read the tokens after NAME: `--NAME VALUE` or `--NAME=VALUE`, each param
/// once; `--help` or `-h` wherever a param's name could stand. A token taken
/// as a value is data, whatever it looks like.
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
        if value.trim().is_empty() {
            return Err(usage(format!("--{name} is blank: give a value")));
        }
        if given.values.insert(name.to_owned(), value).is_some() {
            return Err(usage(format!("--{name} is given twice")));
        }
    }
    Ok(given)
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
        if let Some(from) = params.get(name).and_then(|p| p.from.as_ref()) {
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

/// Bind every param that has a value, from the command line or its
/// `default`, in dependency order.
pub(crate) fn bind(bench: &Bench, given: &Given, view: &View) -> Result<Bound, DevError> {
    let mut bound = Bound::default();
    for name in order(bench) {
        let param = &bench.params[name];
        let (value, source) = match (given.values.get(name), &param.default) {
            (Some(value), _) => (value.as_str(), Source::Given),
            (None, Some(default)) => (default.as_str(), Source::Default),
            (None, None) => continue,
        };
        let refuse = |why: String| match source {
            Source::Given => {
                DevError::Usage(format!("bench {}: --{name} {value} {why}", bench.name))
            }
            Source::Default => DevError::Config(format!(
                "bench {}: --{name}'s default {value:?} {why}; give --{name} on the command line \
                 or fix the default",
                bench.name
            )),
        };
        if value.contains(',') {
            return Err(refuse(
                "is a list of values, which a param does not take yet".into(),
            ));
        }
        if param.from.is_some() && value == "all" {
            return Err(refuse(
                "asks for every key, which a param does not take yet".into(),
            ));
        }
        if param.many && value.contains("..") {
            return Err(refuse(
                "is a range of keys, which a collection does not take yet".into(),
            ));
        }
        let (from, bound_value) = match &param.from {
            None => (
                None,
                BoundValue {
                    key: value.to_owned(),
                    entry: lua::Value::Text(value.to_owned()),
                    file: None,
                    digest: None,
                },
            ),
            Some(from) => {
                let path = substitute(name, &from.segments, |p| {
                    bound
                        .params
                        .get(p)
                        .and_then(|b| b.values.first())
                        .map(|v| v.key.clone())
                })
                .map_err(refuse)?;
                let table = walk(view.table(), &path).map_err(refuse)?;
                let Some(entry) = table.get(value) else {
                    return Err(refuse(format!(
                        "is not a key under {} (valid: {})",
                        view::shown(&path),
                        keys_of(table)
                    )));
                };
                let at: Segments = [path.clone(), vec![value.to_owned()]].concat();
                let file = view.file(&at).cloned();
                if let Some(file) = &file
                    && file.path.to_str().is_none()
                {
                    return Err(refuse(format!(
                        "names {}, which is not UTF-8 and a script cannot pass on",
                        file.path.display()
                    )));
                }
                (
                    Some(path),
                    BoundValue {
                        key: value.to_owned(),
                        entry: view::value(entry),
                        file,
                        digest: None,
                    },
                )
            }
        };
        bound.params.insert(
            name.to_owned(),
            Binding {
                many: param.many,
                from,
                values: vec![bound_value],
            },
        );
    }
    Ok(bound)
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
    let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
    if keys.is_empty() {
        return "none".to_owned();
    }
    keys.sort_unstable();
    keys.join(", ")
}

impl Bound {
    /// The bound files, in the order their handles are numbered: param name,
    /// then value order.
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
    pub(crate) fn digest(
        &mut self,
        project_root: &Path,
        sources: &ConfigSources,
    ) -> Result<(), DevError> {
        for (name, binding) in &mut self.params {
            for value in &mut binding.values {
                let Some(file) = &value.file else { continue };
                value.digest = Some(digest_one(name, &value.key, file, project_root, sources)?);
            }
        }
        Ok(())
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
                                *field = lua::Value::Handle(handle.clone());
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

fn digest_one(
    name: &str,
    key: &str,
    file: &FileNode,
    project_root: &Path,
    sources: &ConfigSources,
) -> Result<String, DevError> {
    let what = format!("--{name} {key}");
    let pin_at = view::shown(&file.pin_at);
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
    let digest = crate::preflight::cached_path_digest(&file.path, project_root)?.value();
    let Some(pin) = &file.pin else {
        return Ok(digest.to_string());
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
    Ok(digest.to_string())
}

/// For `--help`: the keys `name` takes under its `from` path, its parents
/// chosen from the command line, else their defaults. `None` for a plain
/// param.
pub(crate) fn values_for(
    bench: &Bench,
    given: &Given,
    view: &View,
    name: &str,
) -> Option<Result<Vec<String>, String>> {
    let from = bench.params.get(name)?.from.as_ref()?;
    let chosen = |p: &str| {
        given
            .values
            .get(p)
            .cloned()
            .or_else(|| bench.params.get(p).and_then(|p| p.default.clone()))
    };
    Some(
        substitute(name, &from.segments, chosen)
            .and_then(|path| walk(view.table(), &path))
            .map(|table| {
                let mut keys: Vec<String> = table.keys().cloned().collect();
                keys.sort_unstable();
                keys
            }),
    )
}

#[cfg(test)]
#[path = "params_tests.rs"]
mod tests;
