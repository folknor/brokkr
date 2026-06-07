//! The comment-preserving writer for `pins.toml`, shared by `--reseed` and
//! `--bless` (the file's only two writers).
//!
//! The writers used to regenerate the file from the in-memory maps, which
//! dropped every hand-written comment on each re-stamp. Instead the existing
//! file is parsed into a `toml_edit` document and the new state is synced
//! into it: values are replaced in place (keeping each key's spacing and any
//! trailing `# comment`), vanished probes are removed (their attached
//! comments go with them - correct: the comment described the probe), and
//! new entries are inserted in house style. `[roots]` is never touched once
//! it exists - it is hand-maintained and the writers only round-trip it.
//!
//! Layout stays deterministic: `[feeds.<name>]` sorted, then `[roots]`,
//! then `[probes.<id>]` sorted (the `BTreeMap` order), one blank line
//! between blocks, fields in contract-first order (`expected`/`feed`/
//! overrides before the volatile `pine`/`csv` hashes).

use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, RawString, Table, Value};

use crate::error::DevError;
use crate::piners::registry::{FeedGroup, FilePin, Pin, RootEntry};

/// Field order inside a `[probes.<id>]` entry: the hand-maintained contract
/// fields first, then the volatile hashes.
const PROBE_FIELDS: [&str; 7] = [
    "expected",
    "feed",
    "bar_budget",
    "ohlcv_start_ms",
    "tv_trades_csv_tz",
    "pine",
    "csv",
];

/// Field order inside a `[feeds.<name>]` group.
const FEED_FIELDS: [&str; 3] = ["primary", "warmup", "lower"];

/// Render the new pin state into `existing` (the current `pins.toml` text;
/// `None` on bootstrap), preserving comments and formatting of everything
/// that survives. See the module header for the sync rules.
pub fn render_pins(
    existing: Option<&str>,
    feeds: &BTreeMap<String, FeedGroup>,
    roots: &BTreeMap<String, RootEntry>,
    probes: &BTreeMap<String, Pin>,
) -> Result<String, DevError> {
    let mut doc: DocumentMut = existing
        .unwrap_or("")
        .parse()
        .map_err(|e| DevError::Config(format!("piners: pins.toml: {e}")))?;
    sync_section(&mut doc, "feeds", feeds, fill_feed)?;
    ensure_roots(&mut doc, roots)?;
    sync_section(&mut doc, "probes", probes, fill_probe)?;
    finalize_layout(&mut doc, feeds, probes);
    Ok(doc.to_string())
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

/// Stamp a `[feeds.<name>]` group's fields in place.
fn fill_feed(table: &mut Table, group: &FeedGroup) -> Result<(), DevError> {
    set_value(table, "primary", pin_value(&group.primary)?);
    sync_opt(table, "warmup", group.warmup.as_ref().map(pin_value).transpose()?);
    sync_opt(table, "lower", group.lower.as_ref().map(pin_value).transpose()?);
    sort_fields(table, &FEED_FIELDS);
    Ok(())
}

/// Stamp a `[probes.<id>]` entry's fields in place.
fn fill_probe(table: &mut Table, pin: &Pin) -> Result<(), DevError> {
    sync_opt(table, "expected", pin.expected.as_deref().map(Value::from));
    sync_opt(table, "feed", pin.feed.as_deref().map(Value::from));
    sync_opt(table, "bar_budget", pin.bar_budget.map(budget_value).transpose()?);
    sync_opt(table, "ohlcv_start_ms", pin.ohlcv_start_ms.map(Value::from));
    sync_opt(
        table,
        "tv_trades_csv_tz",
        pin.tv_trades_csv_tz.as_deref().map(Value::from),
    );
    set_value(table, "pine", pin_value(&pin.pine)?);
    set_value(table, "csv", pin_value(&pin.csv)?);
    sort_fields(table, &PROBE_FIELDS);
    Ok(())
}

/// Create `[roots]` on bootstrap only. An existing table is left byte-for-
/// byte untouched (the verbatim guarantee) - no writer ever modifies roots.
fn ensure_roots(
    doc: &mut DocumentMut,
    roots: &BTreeMap<String, RootEntry>,
) -> Result<(), DevError> {
    if roots.is_empty() || doc.contains_key("roots") {
        return Ok(());
    }
    let mut table = Table::new();
    table.decor_mut().set_prefix("\n");
    for (prefix, root) in roots {
        let value = parse_value(&format!("{{ feed = {} }}", toml_str(&root.feed)))?;
        table.insert(prefix, Item::Value(value));
    }
    doc.insert("roots", Item::Table(table));
    Ok(())
}

/// Walk the canonical block order (feeds sorted, roots, probes sorted),
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
    if let Some(t) = doc.get_mut("roots").and_then(Item::as_table_mut) {
        place(t);
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

/// `bar_budget` as a TOML integer (TOML integers are i64).
fn budget_value(budget: u64) -> Result<Value, DevError> {
    i64::try_from(budget).map(Value::from).map_err(|_| {
        DevError::Config(format!(
            "piners: bar_budget {budget} exceeds TOML's integer range"
        ))
    })
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
        let mut group = FeedGroup {
            primary: file_pin("vendor/engine/data/15m.csv", "f0"),
            warmup: None,
            lower: None,
        };
        group.lower = Some(file_pin("vendor/engine/data/1m.csv", "f2"));
        feeds.insert("eth-15m".to_owned(), group);
        let mut roots = BTreeMap::new();
        roots.insert(
            "vendor/engine".to_owned(),
            RootEntry {
                feed: "eth-15m".to_owned(),
            },
        );
        let mut probes = BTreeMap::new();
        let mut p = pin("alpha-01", "aa");
        p.expected = Some("accepted".to_owned());
        p.feed = Some("eth-15m".to_owned());
        p.bar_budget = Some(38000);
        p.ohlcv_start_ms = Some(1_700_000_000_000);
        p.tv_trades_csv_tz = Some("America/New_York".to_owned());
        probes.insert("alpha-01".to_owned(), p);

        let text = render_pins(None, &feeds, &roots, &probes).unwrap();

        // No leading blank line; sections in order; blank line between blocks.
        assert!(text.starts_with("[feeds.eth-15m]"));
        assert!(text.find("[feeds.eth-15m]").unwrap() < text.find("[roots]").unwrap());
        assert!(text.find("[roots]").unwrap() < text.find("[probes.alpha-01]").unwrap());
        assert!(text.contains("\n\n[roots]"));
        assert!(text.contains("\n\n[probes.alpha-01]"));
        // Contract fields precede the volatile hashes.
        assert!(text.find("expected").unwrap() < text.find("pine").unwrap());

        let data = reparse(&text);
        assert_eq!(data.feeds["eth-15m"].lower.as_ref().unwrap().xxh128, "f2");
        assert_eq!(data.roots["vendor/engine"].feed, "eth-15m");
        let p = &data.probes["alpha-01"];
        assert_eq!(p.expected.as_deref(), Some("accepted"));
        assert_eq!(p.bar_budget, Some(38000));
        assert_eq!(p.ohlcv_start_ms, Some(1_700_000_000_000));
        assert_eq!(p.tv_trades_csv_tz.as_deref(), Some("America/New_York"));
        assert_eq!(p.pine.xxh128, "aa");
    }

    const COMMENTED: &str = "\
# top-of-file commentary
[feeds.eth-15m]
primary = { path = \"data/15m.csv\", xxh128 = \"f-old\" } # pinned upstream

[roots]
# longest prefix wins
\"vendor/engine\" = { feed = \"eth-15m\" }

# alpha is the flagship probe
[probes.alpha-01]
expected = \"accepted\" # blessed 2026-05
feed = \"eth-15m\"
pine = { path = \"validation/alpha-01/strategy.pine\", xxh128 = \"a-old\" }
csv = { path = \"validation/alpha-01/tv_trades.csv\", xxh128 = \"a-old\" }

# zulu is on its way out
[probes.zulu-09]
pine = { path = \"validation/zulu-09/strategy.pine\", xxh128 = \"zz\" }
csv = { path = \"validation/zulu-09/tv_trades.csv\", xxh128 = \"zz\" }
";

    fn commented_state() -> (BTreeMap<String, FeedGroup>, BTreeMap<String, RootEntry>) {
        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m".to_owned(),
            FeedGroup {
                primary: file_pin("data/15m.csv", "f-new"),
                warmup: None,
                lower: None,
            },
        );
        let mut roots = BTreeMap::new();
        roots.insert(
            "vendor/engine".to_owned(),
            RootEntry {
                feed: "eth-15m".to_owned(),
            },
        );
        (feeds, roots)
    }

    #[test]
    fn restamp_preserves_comments_and_updates_values() {
        let (feeds, roots) = commented_state();
        let mut probes = BTreeMap::new();
        let mut alpha = pin("alpha-01", "a-new");
        alpha.expected = Some("byte_exact".to_owned()); // changed by a bless
        alpha.feed = Some("eth-15m".to_owned());
        probes.insert("alpha-01".to_owned(), alpha);
        probes.insert("mid-05".to_owned(), pin("mid-05", "mm")); // newly discovered
        // zulu-09 vanished from the corpus.

        let text = render_pins(Some(COMMENTED), &feeds, &roots, &probes).unwrap();

        // Comments survive: file header, block comment, both trailing ones.
        assert!(text.contains("# top-of-file commentary"));
        assert!(text.contains("# alpha is the flagship probe"));
        assert!(text.contains("# blessed 2026-05"));
        assert!(text.contains("# pinned upstream"));
        assert!(text.contains("# longest prefix wins"));
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
        assert_eq!(data.feeds["eth-15m"].primary.xxh128, "f-new");
        assert_eq!(data.roots["vendor/engine"].feed, "eth-15m");
    }

    #[test]
    fn bless_insert_puts_expected_before_pine() {
        let (feeds, roots) = commented_state();
        let mut probes = BTreeMap::new();
        let mut alpha = pin("alpha-01", "a-old");
        alpha.expected = Some("accepted".to_owned());
        alpha.feed = Some("eth-15m".to_owned());
        probes.insert("alpha-01".to_owned(), alpha);
        let mut zulu = pin("zulu-09", "zz");
        zulu.expected = Some("compile_fail".to_owned()); // first bless
        probes.insert("zulu-09".to_owned(), zulu);

        let text = render_pins(Some(COMMENTED), &feeds, &roots, &probes).unwrap();

        let zulu_at = text.find("[probes.zulu-09]").unwrap();
        let block = &text[zulu_at..];
        assert!(block.find("expected = \"compile_fail\"").unwrap() < block.find("pine").unwrap());
        // Untouched neighbours keep their bytes.
        assert!(text.contains("expected = \"accepted\" # blessed 2026-05"));
        assert!(text.contains("# zulu is on its way out"));
    }

    #[test]
    fn removing_an_override_drops_the_key() {
        let existing = "\
[probes.alpha-01]
bar_budget = 38000
pine = { path = \"validation/alpha-01/strategy.pine\", xxh128 = \"aa\" }
csv = { path = \"validation/alpha-01/tv_trades.csv\", xxh128 = \"aa\" }
";
        let mut probes = BTreeMap::new();
        probes.insert("alpha-01".to_owned(), pin("alpha-01", "aa")); // no overrides
        let text = render_pins(Some(existing), &BTreeMap::new(), &BTreeMap::new(), &probes)
            .unwrap();
        assert!(!text.contains("bar_budget"));
        assert!(reparse(&text).probes["alpha-01"].bar_budget.is_none());
    }
}
