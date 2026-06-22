//! The comment-preserving writer for `lints.toml`, the lint analogue of
//! [`crate::piners::pins_write`] (`--reseed`/`--bless`/`--reanchor` writers).
//!
//! Like the trade-corpus writer, this parses the existing file into a
//! `toml_edit` document and syncs the new state into it rather than
//! regenerating from scratch: values are replaced in place (keeping each
//! key's spacing and any trailing `# comment`), vanished probes are removed
//! (their attached comments go with them - correct: the comment described the
//! probe), and new entries are inserted in house style.
//!
//! `lints.toml` has no `[feeds]` and no `[roots]` - lint needs no market data,
//! so the file is a single section of sorted `[probes.<id>]` tables. Layout
//! stays deterministic: probes in `BTreeMap` order, one blank line between
//! blocks, no leading blank line, fields in contract-first order (`expected`/
//! `tv_anchored_at`/`tv` before the volatile `pine` hash).

use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, RawString, Table, Value};

use crate::error::DevError;
use crate::piners::lint::registry::{LintPin, TvDiag};
use crate::piners::registry::FilePin;

/// Field order inside a `[probes.<id>]` entry: the contract fields and TV
/// anchor first, then the volatile `pine` hash.
const PROBE_FIELDS: [&str; 4] = ["expected", "tv_anchored_at", "tv", "pine"];

/// Render the new lint pin state into `existing` (the current `lints.toml`
/// text; `None` on bootstrap), preserving comments and formatting of
/// everything that survives. See the module header for the sync rules.
pub fn render_lints(
    existing: Option<&str>,
    probes: &BTreeMap<String, LintPin>,
) -> Result<String, DevError> {
    let mut doc: DocumentMut = existing
        .unwrap_or("")
        .parse()
        .map_err(|e| DevError::Config(format!("piners lint: lints.toml: {e}")))?;
    sync_section(&mut doc, "probes", probes, fill_probe)?;
    finalize_layout(&mut doc, probes);
    Ok(doc.to_string())
}

/// Sync the `[probes.<id>]` sub-tables to `entries`: remove keys absent from
/// the map, create missing ones, and let `fill` stamp the fields of each
/// surviving table in place.
fn sync_section(
    doc: &mut DocumentMut,
    name: &str,
    entries: &BTreeMap<String, LintPin>,
    fill: impl Fn(&mut Table, &LintPin) -> Result<(), DevError>,
) -> Result<(), DevError> {
    if entries.is_empty() {
        doc.remove(name);
        return Ok(());
    }
    if !doc.contains_key(name) {
        let mut parent = Table::new();
        parent.set_implicit(true);
        doc.insert(name, Item::Table(parent));
    }
    let parent = doc.get_mut(name).and_then(Item::as_table_mut).ok_or_else(|| {
        DevError::Config(format!("piners lint: lints.toml: [{name}] is not a table"))
    })?;
    let stale: Vec<String> = parent
        .iter()
        .map(|(k, _)| k.to_owned())
        .filter(|k| !entries.contains_key(k))
        .collect();
    for key in &stale {
        parent.remove(key);
    }
    for (key, entry) in entries {
        if !parent.contains_key(key) {
            let mut fresh = Table::new();
            fresh.decor_mut().set_prefix("\n");
            parent.insert(key, Item::Table(fresh));
        }
        let table = parent.get_mut(key).and_then(Item::as_table_mut).ok_or_else(|| {
            DevError::Config(format!(
                "piners lint: lints.toml: [{name}.{key}] is not a table"
            ))
        })?;
        fill(table, entry)?;
    }
    Ok(())
}

/// Stamp a `[probes.<id>]` entry's fields in place.
fn fill_probe(table: &mut Table, pin: &LintPin) -> Result<(), DevError> {
    sync_opt(table, "expected", pin.expected.as_deref().map(Value::from));
    sync_opt(
        table,
        "tv_anchored_at",
        pin.tv_anchored_at.as_deref().map(Value::from),
    );
    sync_opt(table, "tv", tv_value(&pin.tv)?);
    set_value(table, "pine", pin_value(&pin.pine)?);
    sort_fields(table, &PROBE_FIELDS);
    Ok(())
}

/// Walk the canonical block order (probes sorted), pinning each table's
/// render position and its block spacing: one blank line before every block
/// but the first. Existing prefix decor that carries a comment is left alone -
/// only missing/whitespace-only prefixes are normalized.
fn finalize_layout(doc: &mut DocumentMut, probes: &BTreeMap<String, LintPin>) {
    let mut pos = 0isize;
    let mut place = |table: &mut Table| {
        set_block_prefix(table, pos == 0);
        table.set_position(Some(pos));
        pos += 1;
    };
    if let Some(parent) = doc.get_mut("probes").and_then(Item::as_table_mut) {
        for id in probes.keys() {
            if let Some(t) = parent.get_mut(id).and_then(Item::as_table_mut) {
                place(t);
            }
        }
    }
}

/// Normalize a block's leading decor without disturbing comments: the first
/// block sheds a pure-newline prefix (no blank line at the top of the file),
/// every later block gains a `\n` when it has no prefix at all.
fn set_block_prefix(table: &mut Table, first: bool) {
    let decor = table.decor_mut();
    let prefix = decor
        .prefix()
        .and_then(RawString::as_str)
        .unwrap_or("")
        .to_owned();
    if first {
        if !prefix.is_empty() && prefix.chars().all(|c| c == '\n') {
            decor.set_prefix("");
        }
    } else if prefix.is_empty() {
        decor.set_prefix("\n");
    }
}

/// Set `key = value`, preserving the existing value's decor (the spacing
/// around `=` and any trailing `# comment`) when the key already exists.
fn set_value(table: &mut Table, key: &str, new: Value) {
    if let Some(Item::Value(existing)) = table.get_mut(key) {
        let mut new = new;
        *new.decor_mut() = existing.decor().clone();
        *existing = new;
    } else {
        table.insert(key, Item::Value(new));
    }
}

/// [`set_value`] for an optional field: `None` removes the key.
fn sync_opt(table: &mut Table, key: &str, value: Option<Value>) {
    match value {
        Some(v) => set_value(table, key, v),
        None => {
            table.remove(key);
        }
    }
}

/// Order a table's keys by `order` (unknown keys last, stable). Key decor
/// (a comment above a key) travels with its key.
fn sort_fields(table: &mut Table, order: &[&str]) {
    table.sort_values_by(|k1, _, k2, _| rank(order, k1.get()).cmp(&rank(order, k2.get())));
}

fn rank(order: &[&str], key: &str) -> usize {
    order.iter().position(|k| *k == key).unwrap_or(order.len())
}

/// The `tv` fingerprint as an inline array of inline tables, or `None` (drop
/// the key) when the anchor is empty. `col` is omitted per element when unset:
/// `tv = [ { line = 4, col = 8, severity = "error" }, { line = 9, severity = "warning" } ]`.
fn tv_value(tv: &[TvDiag]) -> Result<Option<Value>, DevError> {
    if tv.is_empty() {
        return Ok(None);
    }
    let elems: Vec<String> = tv.iter().map(tv_elem).collect();
    let value = parse_value(&format!("[ {} ]", elems.join(", ")))?;
    Ok(Some(value))
}

/// One `tv` element as an inline table, omitting `col` when it is `None`.
fn tv_elem(diag: &TvDiag) -> String {
    let mut out = format!("{{ line = {}", diag.line);
    if let Some(col) = diag.col {
        out.push_str(&format!(", col = {col}"));
    }
    out.push_str(&format!(", severity = {} }}", toml_str(&diag.severity)));
    out
}

/// A [`FilePin`] as an inline `{ path, xxh128 }` value in house spacing.
fn pin_value(pin: &FilePin) -> Result<Value, DevError> {
    parse_value(&format!(
        "{{ path = {}, xxh128 = {} }}",
        toml_str(&pin.path.to_string_lossy()),
        toml_str(&pin.xxh128),
    ))
}

/// Parse a value the writer itself formatted; failure is a writer bug.
fn parse_value(text: &str) -> Result<Value, DevError> {
    text.parse().map_err(|e| {
        DevError::Config(format!(
            "piners lint: lints.toml writer produced an invalid TOML value `{text}`: {e}"
        ))
    })
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
    use std::path::PathBuf;

    use super::*;
    use crate::piners::lint::registry::LintsData;

    fn file_pin(path: &str, hash: &str) -> FilePin {
        FilePin {
            path: PathBuf::from(path),
            xxh128: hash.to_owned(),
        }
    }

    fn pin(id: &str, hash: &str) -> LintPin {
        LintPin {
            expected: None,
            tv_anchored_at: None,
            tv: Vec::new(),
            pine: file_pin(&format!("lint/{id}.pine"), hash),
        }
    }

    fn reparse(text: &str) -> LintsData {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn fresh_render_is_canonical_and_round_trips() {
        let mut probes = BTreeMap::new();
        let alpha = LintPin {
            expected: Some("agree_flagged".to_owned()),
            tv_anchored_at: Some("2026-06-22T14:03:00Z".to_owned()),
            tv: vec![
                TvDiag {
                    line: 4,
                    col: Some(8),
                    severity: "error".to_owned(),
                },
                TvDiag {
                    line: 9,
                    col: None,
                    severity: "warning".to_owned(),
                },
            ],
            pine: file_pin("lint/unterminated-01.pine", "aa"),
        };
        probes.insert("unterminated-01".to_owned(), alpha);
        let mut beta = pin("clean-01", "bb");
        beta.expected = Some("agree_clean".to_owned());
        probes.insert("clean-01".to_owned(), beta);

        let text = render_lints(None, &probes).unwrap();

        // No leading blank line; probes in BTreeMap order; blank between blocks.
        assert!(text.starts_with("[probes.clean-01]"));
        assert!(
            text.find("[probes.clean-01]").unwrap() < text.find("[probes.unterminated-01]").unwrap()
        );
        assert!(text.contains("\n\n[probes.unterminated-01]"));
        // Contract fields precede the volatile hash.
        assert!(text.find("expected").unwrap() < text.find("pine").unwrap());
        // The second tv element omits its absent col.
        assert!(text.contains("{ line = 9, severity = \"warning\" }"));

        let data = reparse(&text);
        let a = &data.probes["unterminated-01"];
        assert_eq!(a.expected.as_deref(), Some("agree_flagged"));
        assert_eq!(a.tv_anchored_at.as_deref(), Some("2026-06-22T14:03:00Z"));
        assert_eq!(a.tv.len(), 2);
        assert_eq!(a.tv[0].col, Some(8));
        assert_eq!(a.tv[1].col, None);
        assert_eq!(a.pine.xxh128, "aa");
        assert_eq!(data.probes["clean-01"].expected.as_deref(), Some("agree_clean"));
    }

    const COMMENTED: &str = "\
# top-of-file commentary
# alpha is the flagship probe
[probes.alpha-01]
expected = \"agree_flagged\" # blessed 2026-05
pine = { path = \"lint/alpha-01.pine\", xxh128 = \"a-old\" }

# zulu is on its way out
[probes.zulu-09]
pine = { path = \"lint/zulu-09.pine\", xxh128 = \"zz\" }
";

    #[test]
    fn restamp_preserves_comments_and_updates_values() {
        let mut probes = BTreeMap::new();
        let mut alpha = pin("alpha-01", "a-new"); // hash bumped by a reseed
        alpha.expected = Some("agree_flagged".to_owned());
        probes.insert("alpha-01".to_owned(), alpha);
        probes.insert("mid-05".to_owned(), pin("mid-05", "mm")); // newly discovered
        // zulu-09 vanished from the corpus.

        let text = render_lints(Some(COMMENTED), &probes).unwrap();

        // Comments survive: file header, block comment, trailing one.
        assert!(text.contains("# top-of-file commentary"));
        assert!(text.contains("# alpha is the flagship probe"));
        assert!(text.contains("# blessed 2026-05"));
        // Value updated in place.
        assert!(text.contains("\"a-new\""));
        assert!(!text.contains("a-old"));
        // The vanished probe and its comment are gone.
        assert!(!text.contains("zulu-09"));
        assert!(!text.contains("on its way out"));
        // The newcomer lands in sorted position, after alpha.
        assert!(text.find("[probes.alpha-01]").unwrap() < text.find("[probes.mid-05]").unwrap());
        assert!(text.contains("\n\n[probes.mid-05]"));

        let data = reparse(&text);
        assert_eq!(data.probes["alpha-01"].pine.xxh128, "a-new");
        assert_eq!(data.probes["mid-05"].expected, None);
    }

    #[test]
    fn setting_then_removing_expected_drops_the_key() {
        let existing = "\
[probes.alpha-01]
expected = \"agree_clean\"
pine = { path = \"lint/alpha-01.pine\", xxh128 = \"aa\" }
";
        let mut probes = BTreeMap::new();
        probes.insert("alpha-01".to_owned(), pin("alpha-01", "aa")); // expected = None
        let text = render_lints(Some(existing), &probes).unwrap();
        assert!(!text.contains("expected"));
        assert!(reparse(&text).probes["alpha-01"].expected.is_none());
    }

    #[test]
    fn non_empty_tv_round_trips_and_empty_tv_drops_the_key() {
        let mut anchored = pin("anchored-01", "aa");
        anchored.tv = vec![TvDiag {
            line: 3,
            col: Some(5),
            severity: "error".to_owned(),
        }];
        let mut probes = BTreeMap::new();
        probes.insert("anchored-01".to_owned(), anchored);
        probes.insert("bare-02".to_owned(), pin("bare-02", "bb")); // empty tv

        let text = render_lints(None, &probes).unwrap();

        let data = reparse(&text);
        assert_eq!(data.probes["anchored-01"].tv.len(), 1);
        assert_eq!(data.probes["anchored-01"].tv[0].line, 3);
        assert!(data.probes["bare-02"].tv.is_empty());
        // The empty-tv probe carries no `tv` key in its block.
        let bare_at = text.find("[probes.bare-02]").unwrap();
        assert!(!text[bare_at..].contains("tv ="));
    }

    #[test]
    fn tv_element_without_col_omits_the_col_key() {
        let mut anchored = pin("anchored-01", "aa");
        anchored.tv = vec![TvDiag {
            line: 7,
            col: None,
            severity: "warning".to_owned(),
        }];
        let mut probes = BTreeMap::new();
        probes.insert("anchored-01".to_owned(), anchored);

        let text = render_lints(None, &probes).unwrap();

        assert!(text.contains("{ line = 7, severity = \"warning\" }"));
        assert!(!text.contains("col"));
        assert_eq!(reparse(&text).probes["anchored-01"].tv[0].col, None);
    }
}
