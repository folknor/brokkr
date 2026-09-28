//! The comment-preserving writer for `pins.toml`, shared by `--reseed` and
//! `--bless` (the file's only two writers).
//!
//! The writers used to regenerate the file from the in-memory maps, which
//! dropped every hand-written comment on each re-stamp. Instead the existing
//! file is parsed into a `toml_edit` document and the new state is synced
//! into it: values are replaced in place (keeping each key's spacing and any
//! trailing `# comment`), vanished probes are removed (their attached
//! comments go with them - correct: the comment described the probe), and
//! new entries are inserted in house style. `[probe_config]` is never
//! written - it is hand-maintained, and the writers only round-trip it
//! (see `registry` for why the execution facts live there and not on the
//! probe entries these writers own).
//!
//! Layout stays deterministic: `[feeds.<name>]` sorted, then
//! `[probe_config]`, then `[probes.<id>]` sorted (the `BTreeMap` order), one
//! blank line between blocks, fields in contract-first order (`expected`
//! before the volatile file hashes). Every render is parsed back through the
//! loader before it is returned (see [`render_pins`]).

use std::collections::BTreeMap;
use std::path::Path;

use toml_edit::{DocumentMut, Item, RawString, Table, Value};

use crate::error::DevError;
use crate::piners::registry::{self, FeedGroup, FilePin, Pin};

/// Field order inside a `[probes.<id>]` entry: the blessed contract first,
/// then the volatile hashes.
const PROBE_FIELDS: [&str; 5] = ["expected", "pine", "inputs", "csv", "record"];

/// Field order inside a `[feeds.<name>]` group. `base` is the single-base
/// form; `primary`/`warmup`/`lower` the role form. The two forms never
/// coexist, so this ordering only ever sorts one of them.
const FEED_FIELDS: [&str; 4] = ["base", "primary", "warmup", "lower"];

/// Render the new pin state into `existing` (the current `pins.toml` text;
/// `None` on bootstrap), preserving comments and formatting of everything
/// that survives. See the module header for the sync rules.
///
/// The rendered text is parsed back through [`registry::parse_pins`] before
/// it is returned, so a writer can never replace the file with one the next
/// load refuses - the loader's rules (at least one oracle, fixed file names,
/// no stale or dead `[probe_config]` declaration) bind the writers by
/// construction, not by each caller remembering them. That includes a
/// reseed that drops the last probe a declaration covered: it is refused
/// until the declaration goes in the same diff.
pub fn render_pins(
    existing: Option<&str>,
    feeds: &BTreeMap<String, FeedGroup>,
    probes: &BTreeMap<String, Pin>,
) -> Result<String, DevError> {
    let mut doc: DocumentMut = existing
        .unwrap_or("")
        .parse()
        .map_err(|e| DevError::Config(format!("piners: pins.toml: {e}")))?;
    sync_section(&mut doc, "feeds", feeds, fill_feed)?;
    sync_section(&mut doc, "probes", probes, fill_probe)?;
    finalize_layout(&mut doc, feeds, probes);
    let text = doc.to_string();
    registry::parse_pins(&text, Path::new("pins.toml (as rendered, not written)"))?;
    Ok(text)
}

/// Sync one keyed section (`[<name>.<key>]` sub-tables) to `entries`:
/// remove keys absent from the map, create missing ones, and let `fill`
/// stamp the fields of each surviving table in place.
fn sync_section<T>(
    doc: &mut DocumentMut,
    name: &str,
    entries: &BTreeMap<String, T>,
    fill: impl Fn(&mut Table, &T) -> Result<(), DevError>,
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
        DevError::Config(format!("piners: pins.toml: [{name}] is not a table"))
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
                "piners: pins.toml: [{name}.{key}] is not a table"
            ))
        })?;
        fill(table, entry)?;
    }
    Ok(())
}

/// Stamp a `[feeds.<name>]` group's fields in place. Each arm clears the keys
/// of the *other* form so a group that switched forms (e.g. `primary` -> a
/// single `base`) does not leave stale keys behind.
fn fill_feed(table: &mut Table, group: &FeedGroup) -> Result<(), DevError> {
    match group {
        FeedGroup::Roles {
            primary,
            warmup,
            lower,
        } => {
            set_value(table, "primary", pin_value(primary)?);
            sync_opt(table, "warmup", warmup.as_ref().map(pin_value).transpose()?);
            sync_opt(table, "lower", lower.as_ref().map(pin_value).transpose()?);
            sync_opt(table, "base", None);
        }
        FeedGroup::Base { base } => {
            set_value(table, "base", pin_value(base)?);
            sync_opt(table, "primary", None);
            sync_opt(table, "warmup", None);
            sync_opt(table, "lower", None);
        }
    }
    sort_fields(table, &FEED_FIELDS);
    Ok(())
}

/// Stamp a `[probes.<id>]` entry's fields in place.
fn fill_probe(table: &mut Table, pin: &Pin) -> Result<(), DevError> {
    sync_opt(table, "expected", pin.expected.as_deref().map(Value::from));
    set_value(table, "pine", pin_value(&pin.pine)?);
    sync_opt(table, "inputs", pin.inputs.as_ref().map(pin_value).transpose()?);
    sync_opt(table, "csv", pin.csv.as_ref().map(pin_value).transpose()?);
    sync_opt(table, "record", pin.record.as_ref().map(pin_value).transpose()?);
    sort_fields(table, &PROBE_FIELDS);
    Ok(())
}

/// Walk the canonical block order (feeds sorted, `[probe_config]` as
/// written, probes sorted),
/// pinning each table's render position and its block spacing: one blank
/// line before every block but the first. Existing prefix decor that
/// carries a comment is left alone - only missing/whitespace-only prefixes
/// are normalized.
fn finalize_layout(
    doc: &mut DocumentMut,
    feeds: &BTreeMap<String, FeedGroup>,
    probes: &BTreeMap<String, Pin>,
) {
    let mut pos = 0isize;
    let mut place = |table: &mut Table| {
        set_block_prefix(table, pos == 0);
        table.set_position(Some(pos));
        pos += 1;
    };
    if let Some(parent) = doc.get_mut("feeds").and_then(Item::as_table_mut) {
        for name in feeds.keys() {
            if let Some(t) = parent.get_mut(name).and_then(Item::as_table_mut) {
                place(t);
            }
        }
    }
    // `[probe_config]` keeps its hand-written order. A `[probe_config]`
    // header of its own (inline `"prefix" = { ... }` values) is one block;
    // `[probe_config."prefix"]` sub-tables are one block each, in the order
    // they were written.
    if let Some(t) = doc.get_mut("probe_config").and_then(Item::as_table_mut) {
        if !t.is_implicit() {
            place(t);
        }
        let mut children: Vec<(isize, String)> = t
            .iter()
            .filter_map(|(k, item)| {
                item.as_table()
                    .map(|c| (c.position().unwrap_or(isize::MAX), k.to_owned()))
            })
            .collect();
        children.sort();
        for (_, key) in children {
            if let Some(child) = t.get_mut(&key).and_then(Item::as_table_mut) {
                place(child);
            }
        }
    }
    if let Some(parent) = doc.get_mut("probes").and_then(Item::as_table_mut) {
        for id in probes.keys() {
            if let Some(t) = parent.get_mut(id).and_then(Item::as_table_mut) {
                place(t);
            }
        }
    }
}

/// Normalize a block's leading decor without disturbing comments: the first
/// block sheds a pure-newline prefix (no blank line at the top of the
/// file), every later block gains a `\n` when it has no prefix at all.
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
            "piners: pins.toml writer produced an invalid TOML value `{text}`: {e}"
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
    use crate::piners::registry::PinsData;

    fn file_pin(path: &str, hash: &str) -> FilePin {
        FilePin {
            path: PathBuf::from(path),
            xxh128: hash.to_owned(),
        }
    }

    fn pin(id: &str, hash: &str) -> Pin {
        Pin::new(
            file_pin(&format!("validation/{id}/strategy.pine"), hash),
            file_pin(&format!("validation/{id}/tv_trades.csv"), hash),
        )
    }

    fn reparse(text: &str) -> PinsData {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn fresh_render_is_canonical_and_round_trips() {
        let mut feeds = BTreeMap::new();
        let group = FeedGroup::Roles {
            primary: file_pin("vendor/engine/data/15m.csv", "f0"),
            warmup: None,
            lower: Some(file_pin("vendor/engine/data/1m.csv", "f2")),
        };
        feeds.insert("eth-15m".to_owned(), group);
        let mut probes = BTreeMap::new();
        let mut p = pin("alpha-01", "aa");
        p.expected = Some("accepted".to_owned());
        probes.insert("alpha-01".to_owned(), p);

        let text = render_pins(None, &feeds, &probes).unwrap();

        // No leading blank line; sections in order; blank line between blocks.
        assert!(text.starts_with("[feeds.eth-15m]"));
        assert!(text.find("[feeds.eth-15m]").unwrap() < text.find("[probes.alpha-01]").unwrap());
        assert!(text.contains("\n\n[probes.alpha-01]"));
        // A writer never creates [probe_config]: it is hand-declared.
        assert!(!text.contains("probe_config"));
        // Contract fields precede the volatile hashes.
        assert!(text.find("expected").unwrap() < text.find("pine").unwrap());

        let data = reparse(&text);
        match &data.feeds["eth-15m"] {
            FeedGroup::Roles { lower, .. } => {
                assert_eq!(lower.as_ref().unwrap().xxh128, "f2");
            }
            other => panic!("expected role form, got {other:?}"),
        }
        let p = &data.probes["alpha-01"];
        assert_eq!(p.expected.as_deref(), Some("accepted"));
        assert_eq!(p.pine.xxh128, "aa");
    }

    const COMMENTED: &str = "\
# top-of-file commentary
[feeds.eth-15m]
primary = { path = \"data/15m.csv\", xxh128 = \"f-old\" } # pinned upstream

# each field from the longest prefix that sets it
[probe_config.\"validation\"]
feed = \"eth-15m\" # the engine export feed

[probe_config.\"validation/alpha-01\"]
bar_budget = 38000

# alpha is the flagship probe
[probes.alpha-01]
expected = \"accepted\" # blessed 2026-05
pine = { path = \"validation/alpha-01/strategy.pine\", xxh128 = \"a-old\" }
csv = { path = \"validation/alpha-01/tv_trades.csv\", xxh128 = \"a-old\" }

# zulu is on its way out
[probes.zulu-09]
pine = { path = \"validation/zulu-09/strategy.pine\", xxh128 = \"zz\" }
csv = { path = \"validation/zulu-09/tv_trades.csv\", xxh128 = \"zz\" }
";

    fn commented_feeds() -> BTreeMap<String, FeedGroup> {
        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m".to_owned(),
            FeedGroup::Roles {
                primary: file_pin("data/15m.csv", "f-new"),
                warmup: None,
                lower: None,
            },
        );
        feeds
    }

    #[test]
    fn restamp_preserves_comments_and_updates_values() {
        let feeds = commented_feeds();
        let mut probes = BTreeMap::new();
        let mut alpha = pin("alpha-01", "a-new");
        alpha.expected = Some("byte_exact".to_owned()); // changed by a bless
        probes.insert("alpha-01".to_owned(), alpha);
        probes.insert("mid-05".to_owned(), pin("mid-05", "mm")); // newly discovered
        // zulu-09 vanished from the corpus.

        let text = render_pins(Some(COMMENTED), &feeds, &probes).unwrap();

        // Comments survive: file header, block comment, both trailing ones.
        assert!(text.contains("# top-of-file commentary"));
        assert!(text.contains("# alpha is the flagship probe"));
        assert!(text.contains("# blessed 2026-05"));
        assert!(text.contains("# pinned upstream"));
        // [probe_config] round-trips byte for byte, in its written order,
        // between the feeds and the probes.
        let config = "# each field from the longest prefix that sets it\n\
                      [probe_config.\"validation\"]\n\
                      feed = \"eth-15m\" # the engine export feed\n\n\
                      [probe_config.\"validation/alpha-01\"]\nbar_budget = 38000\n";
        assert!(text.contains(config), "{text}");
        assert!(text.find("[feeds.eth-15m]").unwrap() < text.find("[probe_config").unwrap());
        assert!(text.find("alpha-01\"]").unwrap() < text.find("[probes.alpha-01]").unwrap());
        // Values updated in place.
        assert!(text.contains("\"f-new\""));
        assert!(!text.contains("f-old"));
        assert!(text.contains("expected = \"byte_exact\" # blessed 2026-05"));
        // The vanished probe and its comment are gone.
        assert!(!text.contains("zulu-09"));
        assert!(!text.contains("on its way out"));
        // The newcomer lands in sorted position, after alpha.
        assert!(text.find("[probes.alpha-01]").unwrap() < text.find("[probes.mid-05]").unwrap());
        assert!(text.contains("\n\n[probes.mid-05]"));

        let data = reparse(&text);
        assert_eq!(data.probes["alpha-01"].expected.as_deref(), Some("byte_exact"));
        assert_eq!(data.probes["mid-05"].expected, None);
        assert_eq!(data.feeds["eth-15m"].roles()[0].1.xxh128, "f-new");
        assert_eq!(data.probe_config["validation/alpha-01"].bar_budget, Some(38000));
    }

    #[test]
    fn a_restamp_that_strands_a_declaration_is_refused() {
        // alpha-01 vanished: its exact-dir declaration now governs nothing, so
        // the write is refused until the declaration leaves in the same diff.
        let feeds = commented_feeds();
        let mut probes = BTreeMap::new();
        probes.insert("zulu-09".to_owned(), pin("zulu-09", "zz"));
        let err = render_pins(Some(COMMENTED), &feeds, &probes).unwrap_err();
        assert!(format!("{err:?}").contains("validation/alpha-01\\\"]: `bar_budget` governs no"));
    }

    #[test]
    fn bless_insert_puts_expected_before_pine() {
        let feeds = commented_feeds();
        let mut probes = BTreeMap::new();
        let mut alpha = pin("alpha-01", "a-old");
        alpha.expected = Some("accepted".to_owned());
        probes.insert("alpha-01".to_owned(), alpha);
        let mut zulu = pin("zulu-09", "zz");
        zulu.expected = Some("compile_fail".to_owned()); // first bless
        probes.insert("zulu-09".to_owned(), zulu);

        let text = render_pins(Some(COMMENTED), &feeds, &probes).unwrap();

        let zulu_at = text.find("[probes.zulu-09]").unwrap();
        let block = &text[zulu_at..];
        assert!(block.find("expected = \"compile_fail\"").unwrap() < block.find("pine").unwrap());
        // Untouched neighbours keep their bytes.
        assert!(text.contains("expected = \"accepted\" # blessed 2026-05"));
        assert!(text.contains("# zulu is on its way out"));
    }

    #[test]
    fn single_base_feed_group_round_trips() {
        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m-2025".to_owned(),
            FeedGroup::Base {
                base: file_pin("vendor/engine/data/ohlcv_1m.csv", "b0"),
            },
        );
        let text =
            render_pins(None, &feeds, &BTreeMap::new()).unwrap();
        assert!(text.contains("base = { path ="));
        assert!(!text.contains("primary"));
        match &reparse(&text).feeds["eth-15m-2025"] {
            FeedGroup::Base { base } => assert_eq!(base.xxh128, "b0"),
            other => panic!("expected base form, got {other:?}"),
        }
    }

    #[test]
    fn switching_a_group_to_base_form_clears_role_keys() {
        // A group pinned in role form is re-stamped as single-base: the stale
        // primary/warmup/lower keys must not survive.
        let existing = "\
[feeds.eth-15m]
primary = { path = \"data/15m.csv\", xxh128 = \"f0\" }
warmup = { path = \"data/15m_warmup.csv\", xxh128 = \"f1\" }
";
        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m".to_owned(),
            FeedGroup::Base {
                base: file_pin("data/ohlcv_1m.csv", "b0"),
            },
        );
        let text =
            render_pins(Some(existing), &feeds, &BTreeMap::new()).unwrap();
        assert!(!text.contains("primary"));
        assert!(!text.contains("warmup"));
        assert!(text.contains("base = { path ="));
        assert!(matches!(
            reparse(&text).feeds["eth-15m"],
            FeedGroup::Base { .. }
        ));
    }

    #[test]
    fn record_oracle_round_trips_and_a_vanished_csv_drops_out() {
        let existing = "\
[probes.alpha-01]
pine = { path = \"validation/alpha-01/strategy.pine\", xxh128 = \"aa\" }
csv = { path = \"validation/alpha-01/tv_trades.csv\", xxh128 = \"aa\" } # the old export
";
        // The CSV was replaced by a tvr record on disk.
        let mut p = pin("alpha-01", "aa");
        p.csv = None;
        p.record = Some(file_pin("validation/alpha-01/tv_record.json", "rr"));
        let mut probes = BTreeMap::new();
        probes.insert("alpha-01".to_owned(), p);
        let text = render_pins(Some(existing), &BTreeMap::new(), &probes)
            .unwrap();
        assert!(!text.contains("tv_trades.csv"));
        assert!(text.contains("record = { path = \"validation/alpha-01/tv_record.json\""));
        assert!(text.find("pine").unwrap() < text.find("record").unwrap());
        let data = reparse(&text);
        assert!(data.probes["alpha-01"].csv.is_none());
        assert_eq!(data.probes["alpha-01"].record.as_ref().unwrap().xxh128, "rr");
    }

    #[test]
    fn render_refuses_state_the_loader_would_reject() {
        let mut p = pin("alpha-01", "aa");
        p.csv = None; // no oracle left
        let mut probes = BTreeMap::new();
        probes.insert("alpha-01".to_owned(), p);
        let err = render_pins(None, &BTreeMap::new(), &probes).unwrap_err();
        assert!(format!("{err:?}").contains("pins no oracle"));
    }
}
