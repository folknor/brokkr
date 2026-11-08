//! The configuration text a run document stores: what the run was made from,
//! as one TOML document so `brokkr results --compare` can name what moved.
//!
//! - `[run]`: the live project root (where `data`, `scratch` and the other
//!   configured paths resolve) and the live build root, the same under
//!   `--commit`, whose worktree path is not part of what was asked.
//! - `[script]`: the bench's name, the file its script came from and the
//!   exact bytes that ran, as `source` when they are UTF-8 and `source_hex`
//!   otherwise, since a script file's provenance (which file a `script_file`
//!   resolved to, through which include) is not in the composed table.
//! - `[modules.NAME]`: each `bench_lib` module the run's preflight or an
//!   iteration asked `require` for, loaded or not: its file as `origin` and
//!   its exact bytes, as the script's are. Absent when none was.
//! - `[config]`: the composed table (includes folded, the user-wide layer
//!   beneath, every host section), keys sorted at every level so equal
//!   configs give equal text.
//!
//! TOML first, so `inf` and datetimes compare as themselves; a JSON object
//! of the same shape only if TOML cannot serialize it.

use super::Frozen;
use crate::project::Detection;

pub(super) fn render(
    detection: &Detection,
    bench: &str,
    frozen: &Frozen,
    modules: &std::collections::BTreeSet<String>,
) -> String {
    let mut run = toml::Table::new();
    run.insert(
        "build_root".into(),
        toml::Value::String(detection.build_root.as_path().display().to_string()),
    );
    run.insert(
        "project_root".into(),
        toml::Value::String(detection.project_root.as_path().display().to_string()),
    );
    let mut script = toml::Table::new();
    script.insert("bench".into(), toml::Value::String(bench.to_owned()));
    script.insert(
        "origin".into(),
        toml::Value::String(frozen.origin.display().to_string()),
    );
    insert_source(&mut script, &frozen.bytes);
    let mut document = toml::Table::new();
    if let Some(library) = &frozen.library
        && !modules.is_empty()
    {
        let mut table = toml::Table::new();
        for name in modules {
            let Some(module) = library.modules.get(name) else {
                continue;
            };
            let mut entry = toml::Table::new();
            entry.insert(
                "origin".into(),
                toml::Value::String(module.path.display().to_string()),
            );
            insert_source(&mut entry, &module.source);
            table.insert(name.clone(), toml::Value::Table(entry));
        }
        document.insert("modules".into(), toml::Value::Table(table));
    }
    document.insert(
        "config".into(),
        sorted(&toml::Value::Table(
            detection.config_sources.composed().clone(),
        )),
    );
    document.insert("run".into(), toml::Value::Table(run));
    document.insert("script".into(), toml::Value::Table(script));
    let document = match sorted(&toml::Value::Table(document)) {
        toml::Value::Table(table) => table,
        _ => toml::Table::new(),
    };
    toml::to_string(&document).unwrap_or_else(|_| {
        serde_json::to_string(&document)
            .unwrap_or_else(|e| serde_json::json!({ "unserializable": e.to_string() }).to_string())
    })
}

/// The exact bytes as `source` when they are UTF-8, else `source_hex`.
fn insert_source(table: &mut toml::Table, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) => table.insert("source".into(), toml::Value::String(text.to_owned())),
        Err(_) => table.insert("source_hex".into(), toml::Value::String(hex(bytes))),
    };
}

/// `value` with every table's keys in sorted order: the composed table keeps
/// insertion order, which depends on how the files were laid out.
fn sorted(value: &toml::Value) -> toml::Value {
    match value {
        toml::Value::Table(table) => {
            let mut keys: Vec<&String> = table.keys().collect();
            keys.sort();
            let mut out = toml::Table::new();
            for key in keys {
                out.insert(key.clone(), sorted(&table[key]));
            }
            toml::Value::Table(out)
        }
        toml::Value::Array(items) => toml::Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            // Writing to a String cannot fail.
            write!(out, "{b:02x}").ok();
            out
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn tables_are_sorted_at_every_level_and_bytes_are_kept() {
        let mut inner = toml::Table::new();
        inner.insert("z".into(), toml::Value::Integer(1));
        inner.insert("a".into(), toml::Value::Integer(2));
        let mut outer = toml::Table::new();
        outer.insert("y".into(), toml::Value::Table(inner.clone()));
        outer.insert(
            "b".into(),
            toml::Value::Array(vec![toml::Value::Table(inner)]),
        );
        let text = toml::to_string(&sorted(&toml::Value::Table(outer))).unwrap();
        assert!(
            text.find("[[b]]").unwrap() < text.find("[y]").unwrap(),
            "{text}"
        );
        assert!(
            text.find("a = 2").unwrap() < text.find("z = 1").unwrap(),
            "{text}"
        );
        assert_eq!(hex(&[0x00, 0xff, 0x1a]), "00ff1a");
    }
}
