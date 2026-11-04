//! The resolved config a script bench sees: `config()`'s table and the tree
//! `from` paths walk (`docs/brokkr.toml.bench.md`, The resolved view).
//!
//! A pure function of the composed table, the hostname and the project root,
//! which is what a run stores (`[config]` and `[run].project_root` in the
//! stored config, the document's hostname), so the view of any stored run
//! can be derived again and is not stored itself. Its shape:
//!
//! - every top-level table of the composed config that is not a host
//!   section, as composed;
//! - `host`: the selected host's section (`[<hostname>]` or
//!   `[host.<hostname>]`) without its datasets, with `name` the hostname and
//!   `data`, `scratch` and `output` absolute, defaults applied;
//! - `datasets`: the selected host's datasets, every file entry's `file`
//!   absolute against the host's data directory, a plain dataset's `path`
//!   against the project root, and each dataset's primary as
//!   `snapshot.base` (its `pbf`, `osc` and `download_date`);
//! - `data`: the project's `[data]`, every file entry's `file` absolute
//!   against the project root.
//!
//! A file entry is a table holding a string `file` under `datasets` or
//! `data`, its pin `xxhash`; a dataset holding a string `path` is one too,
//! its pin `xxh128`. Only a bound param makes one an input: the view itself
//! holds plain strings, and records where each file entry's pin was set so a
//! drifted pin can be named.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::lua;

/// A config path as segments: a key may hold a dot, so a joined string
/// would alias two nodes.
pub(crate) type Segments = Vec<String>;

/// A file entry of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileNode {
    /// The field holding the path: `file`, or a plain dataset's `path`.
    pub(crate) field: &'static str,
    /// The path, absolute.
    pub(crate) path: PathBuf,
    /// The pin as written, when the entry has one.
    pub(crate) pin: Option<String>,
    /// Where the pin is (or would be) in the composed config.
    pub(crate) pin_at: Segments,
    /// Whether the path may name a directory, digested as a tree: a plain
    /// dataset's `path` may; a `file` names a regular file.
    pub(crate) tree: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct View {
    table: toml::Table,
    files: BTreeMap<Segments, FileNode>,
}

impl View {
    pub(crate) fn table(&self) -> &toml::Table {
        &self.table
    }

    /// The file entry at `path`, if that node is one.
    pub(crate) fn file(&self, path: &[String]) -> Option<&FileNode> {
        self.files.get(path)
    }

    /// The whole view as the script's `config()` returns it.
    pub(crate) fn lua(&self) -> lua::Value {
        table_value(&self.table)
    }
}

/// Build the view of `composed` on `hostname`, paths against `project_root`.
pub(crate) fn build(composed: &toml::Table, hostname: &str, project_root: &Path) -> View {
    let mut table = toml::Table::new();
    for (key, value) in composed {
        if key == "host" || crate::config::is_host_key(key, value) {
            continue;
        }
        table.insert(key.clone(), value.clone());
    }
    let mut files = BTreeMap::new();
    let (prefix, section) = selected_host(composed, hostname);
    let section = section.cloned().unwrap_or_default();
    let dir = |key: &str, default: &str| {
        resolve(
            project_root,
            section
                .get(key)
                .and_then(toml::Value::as_str)
                .unwrap_or(default),
        )
    };
    let data_dir = dir("data", "data");
    let mut host = section.clone();
    host.remove("datasets");
    for (key, default) in [
        ("data", "data"),
        ("scratch", "data/scratch"),
        ("output", "data/tilegen"),
    ] {
        host.insert(key.into(), path_value(&dir(key, default)));
    }
    host.insert("name".into(), toml::Value::String(hostname.to_owned()));
    let mut datasets = match section.get("datasets") {
        Some(toml::Value::Table(datasets)) => datasets.clone(),
        _ => toml::Table::new(),
    };
    for (name, dataset) in &mut datasets {
        let toml::Value::Table(dataset) = dataset else {
            continue;
        };
        let at = vec!["datasets".to_owned(), name.clone()];
        let raw = [prefix.clone(), at.clone()].concat();
        dataset_view(dataset, &at, &raw, &data_dir, project_root, &mut files);
    }
    table.insert("datasets".into(), toml::Value::Table(datasets));
    table.insert("host".into(), toml::Value::Table(host));
    if let Some(toml::Value::Table(data)) = table.get_mut("data") {
        let at = vec!["data".to_owned()];
        walk_files(data, Some(&at), &at, project_root, &mut files);
    }
    View { table, files }
}

/// The host section for `hostname` and its path in the composed table.
fn selected_host<'a>(
    composed: &'a toml::Table,
    hostname: &str,
) -> (Segments, Option<&'a toml::Table>) {
    if let Some(section) = composed
        .get("host")
        .and_then(toml::Value::as_table)
        .and_then(|hosts| hosts.get(hostname))
        .and_then(toml::Value::as_table)
    {
        return (vec!["host".into(), hostname.to_owned()], Some(section));
    }
    match composed.get(hostname) {
        Some(value @ toml::Value::Table(section))
            if crate::config::is_host_key(hostname, value) =>
        {
            (vec![hostname.to_owned()], Some(section))
        }
        _ => (vec![hostname.to_owned()], None),
    }
}

fn dataset_view(
    dataset: &mut toml::Table,
    at: &[String],
    raw: &[String],
    data_dir: &Path,
    project_root: &Path,
    files: &mut BTreeMap<Segments, FileNode>,
) {
    if let Some(toml::Value::String(path)) = dataset.get("path") {
        let path = resolve(project_root, path);
        files.insert(
            at.to_vec(),
            FileNode {
                field: "path",
                pin: dataset.get("xxh128").map(pin_text),
                pin_at: [raw, &["xxh128".to_owned()]].concat(),
                path: path.clone(),
                tree: true,
            },
        );
        dataset.insert("path".into(), path_value(&path));
    }
    for (key, value) in dataset.iter_mut() {
        if key == "path" {
            continue;
        }
        let at = [at, std::slice::from_ref(key)].concat();
        let raw = [raw, std::slice::from_ref(key)].concat();
        walk_value(value, Some(&at), &raw, data_dir, files);
    }
    // The primary as a snapshot beside the named ones, whose files are the
    // primary's and whose pins are the primary's pins.
    let mut base = toml::Table::new();
    for section in ["pbf", "osc"] {
        let entries = match dataset.get(section) {
            Some(value @ toml::Value::Table(_)) => value.clone(),
            _ => toml::Value::Table(toml::Table::new()),
        };
        base.insert(section.into(), entries);
        let from = [at, &[section.to_owned()]].concat();
        let to = [at, &["snapshot".into(), "base".into(), section.to_owned()]].concat();
        let copies: Vec<(Segments, FileNode)> = files
            .range(from.clone()..)
            .take_while(|(path, _)| path.starts_with(&from))
            .map(|(path, node)| ([to.as_slice(), &path[from.len()..]].concat(), node.clone()))
            .collect();
        files.extend(copies);
    }
    if let Some(date) = dataset.get("download_date") {
        base.insert("download_date".into(), date.clone());
    }
    match dataset.get_mut("snapshot") {
        Some(toml::Value::Table(snapshots)) => {
            snapshots.insert("base".into(), toml::Value::Table(base));
        }
        _ => {
            let mut snapshots = toml::Table::new();
            snapshots.insert("base".into(), toml::Value::Table(base));
            dataset.insert("snapshot".into(), toml::Value::Table(snapshots));
        }
    }
}

/// Make every file entry at or below `table` absolute against `base`,
/// recording each reachable by a path of keys (`at` is `None` inside an
/// array, which no `from` path can enter). A file entry's subtables may be
/// entries too.
fn walk_files(
    table: &mut toml::Table,
    at: Option<&[String]>,
    raw: &[String],
    base: &Path,
    files: &mut BTreeMap<Segments, FileNode>,
) {
    if let Some(toml::Value::String(file)) = table.get("file") {
        let path = resolve(base, file);
        if let Some(at) = at {
            files.insert(
                at.to_vec(),
                FileNode {
                    field: "file",
                    pin: table.get("xxhash").map(pin_text),
                    pin_at: [raw, &["xxhash".to_owned()]].concat(),
                    path: path.clone(),
                    tree: false,
                },
            );
        }
        table.insert("file".into(), path_value(&path));
    }
    for (key, value) in table.iter_mut() {
        let at = at.map(|at| [at, std::slice::from_ref(key)].concat());
        let raw = [raw, std::slice::from_ref(key)].concat();
        walk_value(value, at.as_deref(), &raw, base, files);
    }
}

fn walk_value(
    value: &mut toml::Value,
    at: Option<&[String]>,
    raw: &[String],
    base: &Path,
    files: &mut BTreeMap<Segments, FileNode>,
) {
    match value {
        toml::Value::Table(inner) => walk_files(inner, at, raw, base, files),
        toml::Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let raw = [raw, &[i.to_string()]].concat();
                walk_value(item, None, &raw, base, files);
            }
        }
        _ => {}
    }
}

/// A pin as written: a string as it is, anything else as its TOML text,
/// which no digest parses, so a malformed pin is refused when bound rather
/// than read as no pin.
fn pin_text(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `rel` against `base`, an absolute `rel` as it is (as the typed resolver
/// does).
fn resolve(base: &Path, rel: &str) -> PathBuf {
    let path = Path::new(rel);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn path_value(path: &Path) -> toml::Value {
    toml::Value::String(path.to_string_lossy().into_owned())
}

/// The largest integer a Lua number (an f64) holds exactly, with every
/// smaller one.
const EXACT: i64 = 1 << 53;

/// A TOML value as the script sees it: integers a number while exact, their
/// decimal text beyond; datetimes their TOML text.
pub(crate) fn value(value: &toml::Value) -> lua::Value {
    match value {
        toml::Value::String(text) => lua::Value::Text(text.clone()),
        #[allow(clippy::cast_precision_loss)] // Exact within EXACT.
        toml::Value::Integer(n) if (-EXACT..=EXACT).contains(n) => lua::Value::Number(*n as f64),
        toml::Value::Integer(n) => lua::Value::Text(n.to_string()),
        toml::Value::Float(n) => lua::Value::Number(*n),
        toml::Value::Boolean(b) => lua::Value::Bool(*b),
        toml::Value::Datetime(when) => lua::Value::Text(when.to_string()),
        toml::Value::Array(items) => lua::Value::List(items.iter().map(self::value).collect()),
        toml::Value::Table(table) => table_value(table),
    }
}

pub(crate) fn table_value(table: &toml::Table) -> lua::Value {
    lua::Value::Table(
        table
            .iter()
            .map(|(key, value)| (key.clone(), self::value(value)))
            .collect(),
    )
}

/// Display a config path: segments joined by dots, a segment that is not a
/// bare key quoted, as TOML writes it.
pub(crate) fn shown(path: &[String]) -> String {
    path.iter()
        .map(|segment| {
            let bare = !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if bare {
                segment.clone()
            } else {
                // A TOML basic string, escapes as TOML spells them.
                toml::Value::String(segment.clone()).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
