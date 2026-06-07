//! `brokkr corpus --reseed`: stamp `pins.toml` from the corpus filesystem.
//!
//! This is the bootstrap and the after-re-pin re-stamp - the only
//! sanctioned way `pins.toml` comes into existence or gets refreshed.
//! `--verify-only` can only compare against pins that already exist; there
//! is no `xxhsum` on PATH. Reseed adopting the current corpus content
//! is the deliberate human act of re-validating the oracle, so `git diff
//! pins.toml` is the review surface and no drift override exists.
//!
//! Unlike every other mode, reseed's selection universe is the corpus
//! **filesystem**, not `pins.toml`: it must be able to pin probes that are
//! not pinned yet. Probe dirs are discovered anywhere under `corpus_root`
//! by the marker (a directory containing both `strategy.pine` and
//! `tv_trades.csv`), independent of depth and tree naming - the roots use
//! `validation/`, `strategies/`, and flat layouts. The registry dir is
//! explicitly excluded from the walk (it contains no probe markers, but
//! the exclusion is cheap insurance now that it lives inside the tree).
//!
//! - `--reseed --all` - walk `corpus_root`, stamp every probe.
//!   Authoritative full regen: a probe whose dir vanished upstream drops
//!   out of the file.
//! - `--reseed --probe <id>` (repeatable) - upsert the named probe(s),
//!   leaving the rest intact.
//!
//! Reseed touches the pinned *content* only: it re-hashes `pine`/`csv` and
//! the `[feeds]` group files, preserves `[roots]` verbatim, and carries
//! forward every probe's hand-maintained fields (`expected`, `feed`,
//! `bar_budget`, `ohlcv_start_ms`, `tv_trades_csv_tz`). A newly discovered
//! probe gets its `feed` assigned by the longest matching `[roots]` prefix.
//!
//! Output is deterministic (sections and entries sorted by key, inline
//! `{ path, xxh128 }` tables) for clean diffs, idempotent (re-stamping
//! overwrites hashes - the re-pin case), and comment-preserving: the file
//! is edited in place via [`crate::piners::pins_write`], so hand-written
//! TOML comments survive the re-stamp.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::PinersConfig;
use crate::error::DevError;
use crate::output;
use crate::piners::cmd::CorpusArgs;
use crate::piners::pins_write;
use crate::piners::registry::{self, FeedGroup, FilePin, Pin, PinsData, RootEntry};
use crate::preflight;

/// The two files whose joint presence marks a directory as a parity probe.
const PINE_FILE: &str = "strategy.pine";
const CSV_FILE: &str = "tv_trades.csv";
const PINS_FILE: &str = "pins.toml";

/// Entry point for `brokkr corpus --reseed`.
pub fn run(
    project_root: &Path,
    cfg: &PinersConfig,
    args: &CorpusArgs,
) -> Result<(), DevError> {
    if !args.keywords.is_empty() {
        return Err(DevError::Config(
            "corpus --reseed: --keyword is not supported (keywords reference \
             already-pinned ids). Use --all or --probe."
                .into(),
        ));
    }
    if args.all && !args.probe.is_empty() {
        return Err(DevError::Config(
            "corpus --reseed: --all and --probe are mutually exclusive.".into(),
        ));
    }

    let corpus_root = project_root.join(cfg.corpus_root());
    let registry_dir = project_root.join(cfg.registry_dir());
    let pins_path = registry_dir.join(PINS_FILE);

    // Keep the raw text alongside the parsed data: the writer edits the
    // existing document in place so hand-written comments survive.
    let existing_text = if pins_path.exists() {
        Some(std::fs::read_to_string(&pins_path).map_err(DevError::Io)?)
    } else {
        None
    };
    let existing = match existing_text.as_deref() {
        Some(text) => registry::parse_pins(text, &pins_path)?,
        None => PinsData::default(),
    };

    let discovered = discover(&corpus_root, &registry_dir)?;

    let mut new_pins = if args.all {
        let mut pins = BTreeMap::new();
        for (id, rel_dir) in &discovered.probes {
            pins.insert(id.clone(), stamp_one(id, rel_dir, &corpus_root)?);
        }
        pins
    } else if !args.probe.is_empty() {
        let mut merged = existing.probes.clone();
        for id in &args.probe {
            let rel_dir = discovered.probes.get(id).ok_or_else(|| {
                DevError::Config(format!(
                    "corpus --reseed: probe '{id}' not found under {} \
                     (no directory named '{id}' containing {PINE_FILE} + {CSV_FILE})",
                    corpus_root.display()
                ))
            })?;
            merged.insert(id.clone(), stamp_one(id, rel_dir, &corpus_root)?);
        }
        merged
    } else {
        return Err(DevError::Config(
            "corpus --reseed requires --all (full regen) or --probe <id> \
             (repeatable upsert)."
                .into(),
        ));
    };

    // Reseed touches the pinned content (pine/csv/feed hashes) only. The
    // blessed `expected` disposition and the hand-maintained per-probe fields
    // (feed, bar_budget, ohlcv_start_ms, tv_trades_csv_tz) are independent
    // contracts, so carry them forward for every probe that survives the
    // re-stamp; then assign a feed (from [roots], longest prefix wins) to
    // probes that still have none.
    carry_preserved(&mut new_pins, &existing.probes);
    assign_feeds(&mut new_pins, &existing.roots);

    let feeds = restamp_feeds(&existing.feeds, &corpus_root)?;
    let diff = Diff::compute(&existing.probes, &new_pins);

    std::fs::create_dir_all(&registry_dir).map_err(DevError::Io)?;
    std::fs::write(
        &pins_path,
        pins_write::render_pins(existing_text.as_deref(), &feeds, &existing.roots, &new_pins)?,
    )
    .map_err(DevError::Io)?;

    if discovered.skipped > 0 {
        output::corpus_msg(&format!(
            "skipped {} non-parity dir(s) (no {CSV_FILE})",
            discovered.skipped
        ));
    }
    output::corpus_msg(&format!(
        "reseed: {} probe(s), {} feed group(s) -> {} (added={} changed={} removed={})",
        new_pins.len(),
        feeds.len(),
        pins_path.display(),
        diff.added,
        diff.changed,
        diff.removed,
    ));
    Ok(())
}

/// The discovery result: probe id -> dir relative to `corpus_root`, plus
/// the count of near-miss dirs (had `strategy.pine` but no oracle).
#[derive(Debug)]
struct Discovered {
    probes: BTreeMap<String, PathBuf>,
    skipped: usize,
}

/// Walk `corpus_root` recursively for probe dirs by the marker: a directory
/// containing both `strategy.pine` and `tv_trades.csv`. A probe dir is
/// terminal (no descent). A dir with `strategy.pine` but no oracle is a
/// non-parity dir (multi-mode self-test etc.) - skipped with a count, also
/// terminal. The registry dir and dot-dirs (`.git` in a plain checkout) are
/// excluded. The probe id is the dir basename; since ids key `pins.toml`, a
/// basename collision across roots is a hard error naming both paths.
fn discover(corpus_root: &Path, registry_dir: &Path) -> Result<Discovered, DevError> {
    if !corpus_root.is_dir() {
        return Err(DevError::Config(format!(
            "corpus --reseed: corpus root not found: {}",
            corpus_root.display()
        )));
    }
    let mut found = Discovered {
        probes: BTreeMap::new(),
        skipped: 0,
    };
    walk(corpus_root, corpus_root, registry_dir, &mut found)?;
    Ok(found)
}

fn walk(
    dir: &Path,
    corpus_root: &Path,
    registry_dir: &Path,
    found: &mut Discovered,
) -> Result<(), DevError> {
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(DevError::Io)? {
        let entry = entry.map_err(DevError::Io)?;
        if !entry.file_type().map_err(DevError::Io)?.is_dir() {
            continue;
        }
        let path = entry.path();
        if path == registry_dir {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        subdirs.push(path);
    }
    subdirs.sort();

    for sub in subdirs {
        let has_pine = sub.join(PINE_FILE).exists();
        let has_csv = sub.join(CSV_FILE).exists();
        if has_pine && has_csv {
            let id = sub
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel = sub
                .strip_prefix(corpus_root)
                .map_err(|_| {
                    DevError::Config(format!(
                        "corpus --reseed: probe dir escapes corpus root: {}",
                        sub.display()
                    ))
                })?
                .to_path_buf();
            if let Some(prev) = found.probes.get(&id) {
                return Err(DevError::Config(format!(
                    "corpus --reseed: probe id '{id}' is ambiguous - two dirs share \
                     the basename:\n  {}\n  {}\n  (ids key pins.toml; rename one)",
                    prev.display(),
                    rel.display()
                )));
            }
            found.probes.insert(id, rel);
        } else if has_pine {
            found.skipped += 1; // non-parity dir: self-test etc.
        } else {
            walk(&sub, corpus_root, registry_dir, found)?;
        }
    }
    Ok(())
}

/// Stamp a single discovered probe: hash both marker files in `rel_dir`.
fn stamp_one(id: &str, rel_dir: &Path, corpus_root: &Path) -> Result<Pin, DevError> {
    let pine_rel = rel_dir.join(PINE_FILE);
    let csv_rel = rel_dir.join(CSV_FILE);
    let pine_abs = corpus_root.join(&pine_rel);
    let csv_abs = corpus_root.join(&csv_rel);

    for (label, path) in [(PINE_FILE, &pine_abs), (CSV_FILE, &csv_abs)] {
        if !path.exists() {
            return Err(DevError::Config(format!(
                "corpus --reseed: probe '{id}' is missing {label}: {}",
                path.display()
            )));
        }
    }

    // Content-only stamp; the caller carries the hand-maintained fields
    // forward and assigns the feed.
    Ok(Pin::new(
        FilePin {
            path: pine_rel,
            xxh128: preflight::compute_xxh128(&pine_abs)?,
        },
        FilePin {
            path: csv_rel,
            xxh128: preflight::compute_xxh128(&csv_abs)?,
        },
    ))
}

/// Copy each surviving probe's hand-maintained fields (`expected`, `feed`,
/// `bar_budget`, `ohlcv_start_ms`, `tv_trades_csv_tz`) from the old pin set
/// into the freshly stamped one. A probe new to the corpus stays
/// `expected: None` (unblessed), which the gate treats as a hard "must
/// bless".
fn carry_preserved(new: &mut BTreeMap<String, Pin>, old: &BTreeMap<String, Pin>) {
    for (id, pin) in new.iter_mut() {
        if let Some(prev) = old.get(id) {
            pin.expected = prev.expected.clone();
            pin.feed = prev.feed.clone();
            pin.bar_budget = prev.bar_budget;
            pin.ohlcv_start_ms = prev.ohlcv_start_ms;
            pin.tv_trades_csv_tz = prev.tv_trades_csv_tz.clone();
        }
    }
}

/// Assign a feed to every probe that has none, by the longest `[roots]`
/// prefix matching its probe dir. An existing explicit `feed` (carried
/// forward by [`carry_preserved`]) is preserved; a probe under no root
/// stays feedless.
fn assign_feeds(pins: &mut BTreeMap<String, Pin>, roots: &BTreeMap<String, RootEntry>) {
    for pin in pins.values_mut() {
        if pin.feed.is_some() {
            continue;
        }
        let Some(probe_dir) = pin.pine.path.parent() else {
            continue;
        };
        pin.feed = roots
            .iter()
            .filter(|(prefix, _)| probe_dir.starts_with(Path::new(prefix)))
            .max_by_key(|(prefix, _)| Path::new(prefix.as_str()).components().count())
            .map(|(_, root)| root.feed.clone());
    }
}

/// Re-stamp every `[feeds]` group's file hashes from the corpus filesystem
/// (the feed sibling of the per-probe re-hash). Paths are preserved; a
/// missing feed file is a hard error - the table claims it exists.
fn restamp_feeds(
    feeds: &BTreeMap<String, FeedGroup>,
    corpus_root: &Path,
) -> Result<BTreeMap<String, FeedGroup>, DevError> {
    let mut out = BTreeMap::new();
    for (name, group) in feeds {
        let mut stamped = group.clone();
        for (role, pin) in [
            ("primary", Some(&mut stamped.primary)),
            ("warmup", stamped.warmup.as_mut()),
            ("lower", stamped.lower.as_mut()),
        ]
        .into_iter()
        .filter_map(|(role, pin)| pin.map(|p| (role, p)))
        {
            let abs = corpus_root.join(&pin.path);
            if !abs.exists() {
                return Err(DevError::Config(format!(
                    "corpus --reseed: feed group '{name}' ({role}) is missing: {}",
                    abs.display()
                )));
            }
            pin.xxh128 = preflight::compute_xxh128(&abs)?;
        }
        out.insert(name.clone(), stamped);
    }
    Ok(out)
}

/// Added/changed/removed counts between the old and new pin sets.
struct Diff {
    added: usize,
    changed: usize,
    removed: usize,
}

impl Diff {
    fn compute(old: &BTreeMap<String, Pin>, new: &BTreeMap<String, Pin>) -> Self {
        let mut added = 0;
        let mut changed = 0;
        for (id, pin) in new {
            match old.get(id) {
                None => added += 1,
                Some(prev) if prev != pin => changed += 1,
                Some(_) => {}
            }
        }
        let removed = old.keys().filter(|k| !new.contains_key(*k)).count();
        Self {
            added,
            changed,
            removed,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn pin(p: &str, h: &str) -> Pin {
        Pin::new(
            FilePin {
                path: PathBuf::from(format!("validation/{p}/strategy.pine")),
                xxh128: h.to_owned(),
            },
            FilePin {
                path: PathBuf::from(format!("validation/{p}/tv_trades.csv")),
                xxh128: h.to_owned(),
            },
        )
    }

    fn write_probe(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("strategy.pine"), b"//@version=6\n").unwrap();
        std::fs::write(dir.join("tv_trades.csv"), b"a,b\n1,2\n").unwrap();
    }

    #[test]
    fn carry_preserved_keeps_hand_maintained_fields_across_restamp() {
        // old probe was blessed and carried overrides; the re-stamp produced
        // a fresh content-only pin with a new hash. carry_preserved must
        // restore every hand-maintained field.
        let mut old = BTreeMap::new();
        let mut blessed = pin("keep", "old-hash");
        blessed.expected = Some("accepted".to_owned());
        blessed.feed = Some("eth-15m".to_owned());
        blessed.bar_budget = Some(38000);
        blessed.ohlcv_start_ms = Some(1_700_000_000_000);
        blessed.tv_trades_csv_tz = Some("America/New_York".to_owned());
        old.insert("keep".to_owned(), blessed);
        old.insert("vanished".to_owned(), pin("vanished", "x"));

        let mut new = BTreeMap::new();
        new.insert("keep".to_owned(), pin("keep", "new-hash")); // re-stamped
        new.insert("fresh".to_owned(), pin("fresh", "y")); // brand new

        carry_preserved(&mut new, &old);

        let kept = &new["keep"];
        assert_eq!(kept.expected.as_deref(), Some("accepted"));
        assert_eq!(kept.feed.as_deref(), Some("eth-15m"));
        assert_eq!(kept.bar_budget, Some(38000));
        assert_eq!(kept.ohlcv_start_ms, Some(1_700_000_000_000));
        assert_eq!(kept.tv_trades_csv_tz.as_deref(), Some("America/New_York"));
        assert_eq!(kept.pine.xxh128, "new-hash"); // content still updated
        assert_eq!(new["fresh"].expected, None); // unblessed newcomer
    }

    #[test]
    fn assign_feeds_uses_longest_root_prefix_and_preserves_explicit() {
        let mut pins = BTreeMap::new();
        let mut engine = Pin::new(
            FilePin {
                path: "vendor/engine/validation/a/strategy.pine".into(),
                xxh128: "0".into(),
            },
            FilePin {
                path: "vendor/engine/validation/a/tv_trades.csv".into(),
                xxh128: "0".into(),
            },
        );
        engine.feed = Some("explicit".to_owned()); // must survive
        pins.insert("a".to_owned(), engine);
        pins.insert(
            "b".to_owned(),
            Pin::new(
                FilePin {
                    path: "vendor/engine/nested/deep/b/strategy.pine".into(),
                    xxh128: "0".into(),
                },
                FilePin {
                    path: "vendor/engine/nested/deep/b/tv_trades.csv".into(),
                    xxh128: "0".into(),
                },
            ),
        );
        pins.insert(
            "c".to_owned(),
            Pin::new(
                FilePin {
                    path: "unrooted/c/strategy.pine".into(),
                    xxh128: "0".into(),
                },
                FilePin {
                    path: "unrooted/c/tv_trades.csv".into(),
                    xxh128: "0".into(),
                },
            ),
        );

        let mut roots = BTreeMap::new();
        roots.insert(
            "vendor".to_owned(),
            RootEntry {
                feed: "broad".to_owned(),
            },
        );
        roots.insert(
            "vendor/engine".to_owned(),
            RootEntry {
                feed: "narrow".to_owned(),
            },
        );

        assign_feeds(&mut pins, &roots);

        assert_eq!(pins["a"].feed.as_deref(), Some("explicit"));
        assert_eq!(pins["b"].feed.as_deref(), Some("narrow")); // longest wins
        assert_eq!(pins["c"].feed, None); // no matching root
    }

    #[test]
    fn diff_counts_added_changed_removed() {
        let mut old = BTreeMap::new();
        old.insert("keep".to_owned(), pin("keep", "11"));
        old.insert("change".to_owned(), pin("change", "11"));
        old.insert("gone".to_owned(), pin("gone", "11"));
        let mut new = BTreeMap::new();
        new.insert("keep".to_owned(), pin("keep", "11"));
        new.insert("change".to_owned(), pin("change", "22"));
        new.insert("fresh".to_owned(), pin("fresh", "33"));
        let d = Diff::compute(&old, &new);
        assert_eq!((d.added, d.changed, d.removed), (1, 1, 1));
    }

    #[test]
    fn discover_finds_probes_across_roots_and_depths() {
        let root =
            std::env::temp_dir().join(format!("brokkr_piners_disc_{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        // Engine layout: validation/<id>/.
        write_probe(&root.join("vendor/engine/validation/alpha-01"));
        // Bench layout: strategies/<id>/.
        write_probe(&root.join("vendor/bench-assets/strategies/01-trend"));
        // Flat piners layout: <id>/ directly under a top dir.
        write_probe(&root.join("piners/ha-close-01"));
        // A multi-mode self-test: strategy.pine but no tv_trades.csv.
        let selftest = root.join("vendor/engine/validation/selftest-01");
        std::fs::create_dir_all(&selftest).unwrap();
        std::fs::write(selftest.join("strategy.pine"), b"//@version=6\n").unwrap();
        std::fs::write(selftest.join("trades-mode-a.csv"), b"x\n").unwrap();
        // Nested per-symbol probes under a container.
        write_probe(&root.join("vendor/engine/validation/container/sym-eth"));
        // The relocated registry: a *.toml-bearing dir that must be excluded.
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(registry.join("pins.toml"), b"\n").unwrap();
        // A dot-dir (plain .git checkout) that must be ignored.
        write_probe(&root.join(".git/fake-probe"));

        let found = discover(&root, &registry).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let ids: Vec<&str> = found.probes.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            vec!["01-trend", "alpha-01", "ha-close-01", "sym-eth"]
        );
        assert_eq!(
            found.probes["01-trend"],
            PathBuf::from("vendor/bench-assets/strategies/01-trend")
        );
        assert_eq!(found.skipped, 1); // the self-test
    }

    #[test]
    fn discover_errors_on_duplicate_basename_across_roots() {
        let root =
            std::env::temp_dir().join(format!("brokkr_piners_dup_{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        write_probe(&root.join("vendor/engine/validation/same-id"));
        write_probe(&root.join("piners/same-id"));
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();

        let err = discover(&root, &registry).unwrap_err();
        std::fs::remove_dir_all(&root).ok();
        assert!(format!("{err:?}").contains("same-id"));
        assert!(format!("{err:?}").contains("ambiguous"));
    }

    #[test]
    fn restamp_feeds_rehashes_and_errors_on_missing_file() {
        let root =
            std::env::temp_dir().join(format!("brokkr_piners_feed_{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/15m.csv"), b"ohlcv\n").unwrap();

        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m".to_owned(),
            FeedGroup {
                primary: FilePin {
                    path: "data/15m.csv".into(),
                    xxh128: "stale".into(),
                },
                warmup: None,
                lower: None,
            },
        );

        let stamped = restamp_feeds(&feeds, &root).unwrap();
        assert_ne!(stamped["eth-15m"].primary.xxh128, "stale");
        assert_eq!(stamped["eth-15m"].primary.xxh128.len(), 32);

        feeds.get_mut("eth-15m").unwrap().primary.path = "data/missing.csv".into();
        let err = restamp_feeds(&feeds, &root).unwrap_err();
        std::fs::remove_dir_all(&root).ok();
        assert!(format!("{err:?}").contains("missing.csv"));
    }
}
