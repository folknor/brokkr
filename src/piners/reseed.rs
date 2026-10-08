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
//! by the marker (a directory containing `strategy.pine` plus an oracle -
//! `tv_trades.csv`, `tv_record.json`, or both), independent of depth and
//! tree naming - the roots use `validation/`, `strategies/`, and flat
//! layouts. Every oracle present is pinned, and so is `inputs.json` when
//! the dir has one. The registry dir is explicitly excluded from the walk
//! (it contains no probe markers, but the exclusion is cheap insurance now
//! that it lives inside the tree).
//!
//! - `--reseed --all` - walk `corpus_root`, stamp every probe.
//!   Authoritative full regen: a probe whose dir vanished upstream drops
//!   out of the file.
//! - `--reseed --probe <id>` (repeatable) - upsert the named probe(s),
//!   leaving the rest intact.
//!
//! A probe registered in `[pending]` (declared ahead of its pin, so its
//! `[probe_config]` can be written first) is completed by the reseed that
//! pins it: the entry leaves `[pending]` in the same write, and must have
//! been found at exactly the registered path. `--all` completes every
//! pending entry and refuses if one is not on disk. Other pending entries
//! survive a `--probe` reseed untouched, which is what lets several new
//! probes be declared together and pinned one at a time.
//!
//! Reseed touches the pinned *content* only: it re-hashes the probe files,
//! the `[feeds]` group files and the `[harness_files]` (on every reseed,
//! `--probe` included), preserves `[probe_config]` verbatim, and
//! carries each surviving probe's blessed `expected` forward. It decides
//! nothing about how a probe runs - feed, budget, start and CSV timezone are
//! resolved from `[probe_config]` at run time - so a probe entry lost and
//! re-added comes back running exactly as before; only its `expected` has
//! to be re-blessed, which the gate demands loudly. A blessed probe whose
//! oracle changed kind keeps its `expected` but is named in a re-bless
//! warning.
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
use crate::piners::registry::{
    self, CSV_FILE, FeedGroup, FilePin, HarnessFile, INPUTS_FILE, PINE_FILE, Pin, PinsData,
    ProbeFiles, RECORD_FILE,
};
use crate::piners::registry_io;
use crate::preflight;

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
    if !args.all && args.probe.is_empty() {
        return Err(DevError::Config(
            "corpus --reseed requires --all (full regen) or --probe <id> \
             (repeatable upsert)."
                .into(),
        ));
    }

    let corpus_root = project_root.join(cfg.corpus_root());
    let registry_dir = project_root.join(cfg.registry_dir());
    let pins_path = registry_dir.join(PINS_FILE);

    // Held across the read-modify-write, so a concurrent `--bless` (which
    // holds the same lock for its run) cannot interleave with this rewrite.
    let _lock = registry_io::lock(project_root, "corpus-reseed")?;

    // Keep the raw text: the writer edits the existing document in place so
    // hand-written comments survive.
    let existing_text = if pins_path.exists() {
        Some(std::fs::read_to_string(&pins_path).map_err(DevError::Io)?)
    } else {
        None
    };
    let plan = plan(existing_text.as_deref(), &pins_path, &corpus_root, &registry_dir, args)?;

    std::fs::create_dir_all(&registry_dir).map_err(DevError::Io)?;
    registry_io::write_atomic(&pins_path, &plan.text)?;

    if plan.skipped > 0 {
        output::corpus_msg(&format!(
            "skipped {} non-parity dir(s) (no {CSV_FILE} or {RECORD_FILE})",
            plan.skipped
        ));
    }
    output::corpus_msg(&format!(
        "reseed: {} probe(s), {} feed group(s) -> {} (added={} changed={} removed={})",
        plan.probes,
        plan.feeds,
        pins_path.display(),
        plan.diff.added,
        plan.diff.changed,
        plan.diff.removed,
    ));
    if !plan.diff.oracle_switched.is_empty() {
        output::corpus_msg(&format!(
            "warning: {} probe(s) now judged against a different oracle ({CSV_FILE} <-> \
             {RECORD_FILE}); their `expected` was carried forward from the old one - \
             re-bless: brokkr corpus --bless --probe {}",
            plan.diff.oracle_switched.len(),
            plan.diff.oracle_switched.join(",")
        ));
    }
    Ok(())
}

/// Everything a reseed decided, before anything is written.
#[derive(Debug)]
struct Plan {
    /// The new `pins.toml` text, already parsed back through the loader.
    text: String,
    probes: usize,
    feeds: usize,
    diff: Diff,
    /// Near-miss dirs the walk passed over (`strategy.pine`, no oracle).
    skipped: usize,
}

/// Compute a reseed against `existing_text` (the current `pins.toml`, `None`
/// on bootstrap) without touching the file or taking the lock - `run` does
/// both around this, which keeps the whole decision testable.
///
/// The existing file is read *unchecked*: `--all` regenerates every probe from
/// the filesystem, so a hand edit that broke a structural rule is repaired by
/// the regen rather than blocking it. Whatever survives (the untouched probes
/// of a `--probe` upsert) is held to the rules again when the writer parses
/// its own output.
fn plan(
    existing_text: Option<&str>,
    pins_path: &Path,
    corpus_root: &Path,
    registry_dir: &Path,
    args: &CorpusArgs,
) -> Result<Plan, DevError> {
    let existing = match existing_text {
        Some(text) => registry::parse_pins_unchecked(text, pins_path)?,
        None => PinsData::default(),
    };

    let discovered = discover(corpus_root, registry_dir)?;

    // A [pending] registration is completed by the reseed that pins it, at
    // exactly the path it registered; `--all` completes every one, and
    // refuses rather than let a registration silently vanish.
    let mut pending = existing.pending.clone();
    let completing: Vec<&String> = if args.all {
        existing.pending.keys().collect()
    } else {
        args.probe.iter().filter(|id| existing.pending.contains_key(*id)).collect()
    };
    for id in completing {
        let registered = &existing.pending[id];
        match discovered.probes.get(id) {
            Some(found) if found == registered => {
                pending.remove(id);
            }
            Some(found) => {
                return Err(DevError::Config(format!(
                    "corpus --reseed: probe '{id}' is [pending] at {} but was found at {} - \
                     fix the [pending] entry",
                    registered.display(),
                    found.display()
                )));
            }
            None => {
                return Err(DevError::Config(format!(
                    "corpus --reseed: [pending] probe '{id}' was not found as a probe at {} \
                     (a directory holding {PINE_FILE} plus {CSV_FILE} or {RECORD_FILE}, outside \
                     dot-dirs and the registry dir)",
                    corpus_root.join(registered).display()
                )));
            }
        }
    }

    let mut new_pins = if args.all {
        let mut pins = BTreeMap::new();
        for (id, rel_dir) in &discovered.probes {
            pins.insert(id.clone(), stamp_one(id, rel_dir, corpus_root)?);
        }
        pins
    } else {
        let mut merged = existing.probes.clone();
        for id in &args.probe {
            let Some(rel_dir) = discovered.probes.get(id) else {
                return Err(not_a_probe(id, &discovered, corpus_root));
            };
            merged.insert(id.clone(), stamp_one(id, rel_dir, corpus_root)?);
        }
        merged
    };

    // Reseed touches the pinned files and feed hashes only. The blessed
    // `expected` disposition is an independent contract, so carry it forward
    // for every probe that survives the re-stamp.
    carry_expected(&mut new_pins, &existing.probes);

    let feeds = restamp_feeds(&existing.feeds, corpus_root)?;
    let files = restamp_harness_files(&existing.harness_files, corpus_root)?;
    let diff = Diff::compute(&existing.probes, &new_pins);
    let text = pins_write::render_pins(existing_text, &feeds, &files, &pending, &new_pins)?;
    registry::check_pending_location(&pending, pins_path, registry_dir, corpus_root)?;

    Ok(Plan {
        text,
        probes: new_pins.len(),
        feeds: feeds.len(),
        diff,
        skipped: discovered.skipped.len(),
    })
}

/// The error for a `--probe` id the walk did not find as a probe, naming
/// the near miss when there is one: a dir by that name with `strategy.pine`
/// but no oracle exists, it just is not a parity probe.
fn not_a_probe(id: &str, discovered: &Discovered, corpus_root: &Path) -> DevError {
    let near: Vec<String> = discovered
        .skipped
        .iter()
        .filter(|rel| rel.file_name().is_some_and(|n| n.to_string_lossy() == id))
        .map(|rel| rel.display().to_string())
        .collect();
    if near.is_empty() {
        DevError::Config(format!(
            "corpus --reseed: probe '{id}' not found under {} (no directory named \
             '{id}' containing {PINE_FILE})",
            corpus_root.display()
        ))
    } else {
        DevError::Config(format!(
            "corpus --reseed: '{id}' has {PINE_FILE} but no oracle ({CSV_FILE} or \
             {RECORD_FILE}), so it is not a parity probe:\n  {}",
            near.join("\n  ")
        ))
    }
}

/// The discovery result: probe id -> dir relative to `corpus_root`, plus
/// the near-miss dirs (had `strategy.pine` but no oracle), also relative.
#[derive(Debug)]
struct Discovered {
    probes: BTreeMap<String, PathBuf>,
    skipped: Vec<PathBuf>,
}

/// Walk `corpus_root` recursively for probe dirs by the marker: a directory
/// containing `strategy.pine` plus `tv_trades.csv` or `tv_record.json`, all
/// regular files. A probe dir is terminal (no descent). A dir with
/// `strategy.pine` but no oracle is a non-parity dir (multi-mode self-test
/// etc.) - skipped and remembered, also terminal. The registry dir and
/// dot-dirs (`.git` in a plain checkout) are excluded. The probe id is the
/// dir basename; since ids key `pins.toml`, a basename collision across
/// roots is a hard error naming both paths.
fn discover(corpus_root: &Path, registry_dir: &Path) -> Result<Discovered, DevError> {
    if !corpus_root.is_dir() {
        return Err(DevError::Config(format!(
            "corpus --reseed: corpus root not found: {}",
            corpus_root.display()
        )));
    }
    let mut found = Discovered {
        probes: BTreeMap::new(),
        skipped: Vec::new(),
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
        let has_pine = sub.join(PINE_FILE).is_file();
        let has_oracle = sub.join(CSV_FILE).is_file() || sub.join(RECORD_FILE).is_file();
        let rel = || {
            sub.strip_prefix(corpus_root)
                .map(Path::to_path_buf)
                .map_err(|_| {
                    DevError::Config(format!(
                        "corpus --reseed: probe dir escapes corpus root: {}",
                        sub.display()
                    ))
                })
        };
        if has_pine && has_oracle {
            let id = sub
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel = rel()?;
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
            found.skipped.push(rel()?); // non-parity dir: self-test etc.
        } else {
            walk(&sub, corpus_root, registry_dir, found)?;
        }
    }
    Ok(())
}

/// Stamp a single discovered probe: hash `strategy.pine` and every other
/// probe file present in `rel_dir` (`inputs.json` and the oracles). Absence
/// is re-read here, not trusted from the walk: a file gone since discovery
/// drops out of the pin, and a probe left with no oracle is an error.
fn stamp_one(id: &str, rel_dir: &Path, corpus_root: &Path) -> Result<Pin, DevError> {
    let pine = stamp_file(id, rel_dir, PINE_FILE, corpus_root)?.ok_or_else(|| {
        DevError::Config(format!(
            "corpus --reseed: probe '{id}' is missing {PINE_FILE}: {}",
            corpus_root.join(rel_dir).join(PINE_FILE).display()
        ))
    })?;
    let inputs = stamp_file(id, rel_dir, INPUTS_FILE, corpus_root)?;
    let csv = stamp_file(id, rel_dir, CSV_FILE, corpus_root)?;
    let record = stamp_file(id, rel_dir, RECORD_FILE, corpus_root)?;
    if csv.is_none() && record.is_none() {
        return Err(DevError::Config(format!(
            "corpus --reseed: probe '{id}' has no oracle ({CSV_FILE} or {RECORD_FILE}) in {}",
            corpus_root.join(rel_dir).display()
        )));
    }

    // Content-only stamp; the caller carries `expected` forward.
    Ok(Pin::content(ProbeFiles {
        pine,
        inputs,
        csv,
        record,
    }))
}

/// Hash `rel_dir/name` into a pin, or `None` when nothing is there. Anything
/// there that is not a regular file is an error: the harness reads it as a
/// file, and hashing a directory would pin a tree digest nothing consumes.
fn stamp_file(
    id: &str,
    rel_dir: &Path,
    name: &str,
    corpus_root: &Path,
) -> Result<Option<FilePin>, DevError> {
    let rel = rel_dir.join(name);
    let abs = corpus_root.join(&rel);
    if !abs.exists() {
        return Ok(None);
    }
    if !abs.is_file() {
        return Err(DevError::Config(format!(
            "corpus --reseed: probe '{id}': {} is not a regular file",
            abs.display()
        )));
    }
    // Probe files are not LFS today, but the guard is cheap insurance: a
    // pointer stamped as a probe hash would poison the pin exactly as a
    // feed pointer would.
    crate::piners::lfs::ensure_materialized(&abs)?;
    Ok(Some(FilePin {
        path: rel,
        xxh128: preflight::compute_xxh128(&abs)?,
    }))
}

/// Copy each surviving probe's blessed `expected` from the old pin set into
/// the freshly stamped one. A probe new to the corpus stays `expected: None`
/// (unblessed), which the gate treats as a hard "must bless".
fn carry_expected(new: &mut BTreeMap<String, Pin>, old: &BTreeMap<String, Pin>) {
    for (id, pin) in new.iter_mut() {
        if let Some(prev) = old.get(id) {
            pin.expected = prev.expected.clone();
        }
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
        for (role, pin) in stamped.roles_mut() {
            let abs = corpus_root.join(&pin.path);
            if !abs.exists() {
                return Err(DevError::Config(format!(
                    "corpus --reseed: feed group '{name}' ({role}) is missing: {}",
                    abs.display()
                )));
            }
            // Refuse to stamp a Git-LFS pointer's hash: that would pin the
            // 134-byte stub and poison every future verify. The one LFS feed
            // is the base group; the guard is a no-op for plaintext feeds.
            crate::piners::lfs::ensure_materialized(&abs)?;
            pin.xxh128 = preflight::compute_xxh128(&abs)?;
        }
        out.insert(name.clone(), stamped);
    }
    Ok(out)
}

/// Re-stamp every `[harness_files]` hash from the corpus filesystem, the
/// way [`restamp_feeds`] does the feeds: paths preserved, a hand-declared
/// path-only entry gets its first hash, a missing file is a hard error.
fn restamp_harness_files(
    files: &BTreeMap<String, HarnessFile>,
    corpus_root: &Path,
) -> Result<BTreeMap<String, HarnessFile>, DevError> {
    let mut out = BTreeMap::new();
    for (name, file) in files {
        let abs = corpus_root.join(&file.path);
        if !abs.is_file() {
            return Err(DevError::Config(format!(
                "corpus --reseed: harness file '{name}' is missing or not a regular file: {}",
                abs.display()
            )));
        }
        crate::piners::lfs::ensure_materialized(&abs)?;
        out.insert(
            name.clone(),
            HarnessFile {
                path: file.path.clone(),
                xxh128: Some(preflight::compute_xxh128(&abs)?),
            },
        );
    }
    Ok(out)
}

/// Added/changed/removed counts between the old and new pin sets, plus the
/// surviving blessed probes whose effective oracle changed kind.
#[derive(Debug)]
struct Diff {
    added: usize,
    changed: usize,
    removed: usize,
    /// Blessed probes that gained or lost a `record` - the harness judges
    /// against the record whenever there is one, so this is exactly the set
    /// whose carried-forward `expected` was blessed against another oracle.
    /// A disposition that happens to match across the switch would pass the
    /// gate silently, so it is reported rather than folded into `changed`.
    oracle_switched: Vec<String>,
}

impl Diff {
    fn compute(old: &BTreeMap<String, Pin>, new: &BTreeMap<String, Pin>) -> Self {
        let mut added = 0;
        let mut changed = 0;
        let mut oracle_switched = Vec::new();
        for (id, pin) in new {
            match old.get(id) {
                None => added += 1,
                Some(prev) if prev != pin => {
                    changed += 1;
                    if pin.expected.is_some() && prev.record.is_some() != pin.record.is_some() {
                        oracle_switched.push(id.clone());
                    }
                }
                Some(_) => {}
            }
        }
        let removed = old.keys().filter(|k| !new.contains_key(*k)).count();
        Self {
            added,
            changed,
            removed,
            oracle_switched,
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
    fn carry_expected_keeps_the_blessing_across_restamp() {
        let mut old = BTreeMap::new();
        let mut blessed = pin("keep", "old-hash");
        blessed.expected = Some("accepted".to_owned());
        old.insert("keep".to_owned(), blessed);
        old.insert("vanished".to_owned(), pin("vanished", "x"));

        let mut new = BTreeMap::new();
        new.insert("keep".to_owned(), pin("keep", "new-hash")); // re-stamped
        new.insert("fresh".to_owned(), pin("fresh", "y")); // brand new

        carry_expected(&mut new, &old);

        assert_eq!(new["keep"].expected.as_deref(), Some("accepted"));
        assert_eq!(new["keep"].pine.xxh128, "new-hash"); // content still updated
        assert_eq!(new["fresh"].expected, None); // unblessed newcomer
    }

    /// A tree with a directory default and one live capture that declares a
    /// different feed, as piners' first-party probes do.
    const DECLARED: &str = "\
[feeds.eth-15m-2025]
base = { path = \"data/old.csv\", xxh128 = \"00\" }

[feeds.eth-15m-live]
base = { path = \"data/live.csv\", xxh128 = \"00\" }

[probe_config.\"piners\"]
feed = \"eth-15m-2025\"

[probe_config.\"piners/live-01\"]
feed = \"eth-15m-live\"
bar_budget = 20200
";

    fn declared_tree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = crate::test_scratch::scratch("piners_reseed", name);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/old.csv"), b"t,o,h,l,c,v\n").unwrap();
        std::fs::write(root.join("data/live.csv"), b"t,o,h,l,c,v\n1,2,3,4,5,6\n").unwrap();
        write_probe(&root.join("piners/live-01"));
        write_probe(&root.join("piners/old-01"));
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        let pins_path = registry.join("pins.toml");
        (root, registry, pins_path)
    }

    fn live_facts(text: &str, pins_path: &Path) -> registry::ProbeConfig {
        let data = registry::parse_pins(text, pins_path).unwrap();
        registry::resolve(&data.probe_config, data.probes["live-01"].probe_dir())
    }

    #[test]
    fn first_reseed_resolves_the_declared_feed_not_the_directory_default() {
        // The live probe has no entry yet (never pinned, or its entry was
        // lost). Its first reseed adds it, and it resolves the feed and
        // budget its directory declares - not the `piners` default.
        let (root, registry, pins_path) = declared_tree("first_reseed");
        let with_old = format!(
            "{DECLARED}\n[probes.old-01]\n\
             pine = {{ path = \"piners/old-01/strategy.pine\", xxh128 = \"00\" }}\n\
             csv = {{ path = \"piners/old-01/tv_trades.csv\", xxh128 = \"00\" }}\n"
        );
        // The file as it stands is refused: the live declaration governs no
        // pinned probe yet. That is the loader's rule, and the reseed that
        // adds the probe is what satisfies it.
        assert!(registry::parse_pins(&with_old, &pins_path).is_err());
        let p = plan(Some(&with_old), &pins_path, &root, &registry, &args_probe(&["live-01"]))
            .unwrap();
        assert_eq!((p.diff.added, p.diff.changed, p.diff.removed), (1, 0, 0));
        let facts = live_facts(&p.text, &pins_path);
        assert_eq!(facts.feed.as_deref(), Some("eth-15m-live"));
        assert_eq!(facts.bar_budget, Some(20200));
        // The declarations were not rewritten.
        assert!(p.text.contains("[probe_config.\"piners/live-01\"]\nfeed = \"eth-15m-live\""));
        assert!(!p.text.contains("\n\n\n"), "stray blank line:\n{}", p.text);
    }

    #[test]
    fn pending_probes_are_pinned_one_at_a_time() {
        // Two live captures declared ahead of their pins, both registered.
        let (root, registry, pins_path) = declared_tree("pending_one_at_a_time");
        write_probe(&root.join("piners/live-02"));
        let declared = format!(
            "{DECLARED}\n[probe_config.\"piners/live-02\"]\nfeed = \"eth-15m-live\"\n\n\
             [pending]\nlive-01 = \"piners/live-01\"\nlive-02 = \"piners/live-02\"\n\n\
             [probes.old-01]\n\
             pine = {{ path = \"piners/old-01/strategy.pine\", xxh128 = \"00\" }}\n\
             csv = {{ path = \"piners/old-01/tv_trades.csv\", xxh128 = \"00\" }}\n"
        );
        // The file as written loads: both declarations govern a pending probe.
        registry::parse_pins(&declared, &pins_path).unwrap();

        let first =
            plan(Some(&declared), &pins_path, &root, &registry, &args_probe(&["live-01"])).unwrap();
        let data = registry::parse_pins(&first.text, &pins_path).unwrap();
        assert!(data.probes.contains_key("live-01"));
        assert!(!data.pending.contains_key("live-01"));
        assert_eq!(data.pending["live-02"], PathBuf::from("piners/live-02"));

        let second =
            plan(Some(&first.text), &pins_path, &root, &registry, &args_probe(&["live-02"])).unwrap();
        let data = registry::parse_pins(&second.text, &pins_path).unwrap();
        assert!(data.pending.is_empty());
        assert!(!second.text.contains("[pending]"));
    }

    #[test]
    fn a_pending_entry_at_the_wrong_path_or_absent_is_refused() {
        let (root, registry, pins_path) = declared_tree("pending_wrong_path");
        let wrong = format!("{DECLARED}\n[pending]\nlive-01 = \"other/live-01\"\n");
        let err = plan(Some(&wrong), &pins_path, &root, &registry, &args_probe(&["live-01"]))
            .unwrap_err();
        assert!(format!("{err:?}").contains("was found at piners/live-01"));
        // --all refuses a registration with nothing on disk.
        let absent = format!("{DECLARED}\n[pending]\nghost = \"piners/ghost\"\n");
        let err = plan(Some(&absent), &pins_path, &root, &registry, &args_all()).unwrap_err();
        assert!(format!("{err:?}").contains("[pending] probe 'ghost' was not found"));
    }

    #[test]
    fn re_reseed_keeps_the_declared_feed_and_the_blessing() {
        let (root, registry, pins_path) = declared_tree("re_reseed");
        let first = plan(Some(DECLARED), &pins_path, &root, &registry, &args_all()).unwrap();
        assert_eq!(first.diff.added, 2);
        let blessed = first.text.replace(
            "[probes.live-01]\n",
            "[probes.live-01]\nexpected = \"accepted\"\n",
        );
        // The capture is re-taken: new oracle bytes, same declared feed.
        std::fs::write(root.join("piners/live-01/tv_trades.csv"), b"a,b\n9,9\n").unwrap();
        let again =
            plan(Some(&blessed), &pins_path, &root, &registry, &args_probe(&["live-01"])).unwrap();
        assert_eq!((again.diff.added, again.diff.changed, again.diff.removed), (0, 1, 0));
        let facts = live_facts(&again.text, &pins_path);
        assert_eq!(facts.feed.as_deref(), Some("eth-15m-live"));
        let data = registry::parse_pins(&again.text, &pins_path).unwrap();
        assert_eq!(data.probes["live-01"].expected.as_deref(), Some("accepted"));
        assert!(!first.text.contains("\n\n\n"), "stray blank line:\n{}", first.text);
        assert!(!again.text.contains("\n\n\n"), "stray blank line:\n{}", again.text);
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
        let root = crate::test_scratch::scratch("piners_reseed", "discover_layouts");
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

        let ids: Vec<&str> = found.probes.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            vec!["01-trend", "alpha-01", "ha-close-01", "sym-eth"]
        );
        assert_eq!(
            found.probes["01-trend"],
            PathBuf::from("vendor/bench-assets/strategies/01-trend")
        );
        // The self-test, remembered by path for the --probe near-miss error.
        assert_eq!(
            found.skipped,
            vec![PathBuf::from("vendor/engine/validation/selftest-01")]
        );
    }

    #[test]
    fn reseed_discovers_and_stamps_record_oracles() {
        let root = crate::test_scratch::scratch("piners_reseed", "record_oracles");
        // A tvr capture with no CSV export.
        let rec_only = root.join("piners/rec-only");
        std::fs::create_dir_all(&rec_only).unwrap();
        std::fs::write(rec_only.join("strategy.pine"), b"//@version=6\n").unwrap();
        std::fs::write(rec_only.join("tv_record.json"), b"{}\n").unwrap();
        // Both oracles side by side.
        let both = root.join("piners/both");
        write_probe(&both);
        std::fs::write(both.join("tv_record.json"), b"{\"v\":1}\n").unwrap();
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();

        let found = discover(&root, &registry).unwrap();
        assert!(found.skipped.is_empty());
        let rec = stamp_one("rec-only", &found.probes["rec-only"], &root).unwrap();
        let dual = stamp_one("both", &found.probes["both"], &root).unwrap();

        assert!(rec.csv.is_none());
        assert_eq!(
            rec.record.as_ref().unwrap().path,
            PathBuf::from("piners/rec-only/tv_record.json")
        );
        assert_eq!(rec.record.as_ref().unwrap().xxh128.len(), 32);
        assert!(dual.csv.is_some());
        assert!(dual.record.is_some());

        // The oracle vanishing between walk and stamp is an error, not an
        // oracle-less pin.
        std::fs::remove_file(rec_only.join("tv_record.json")).unwrap();
        let err = stamp_one("rec-only", &found.probes["rec-only"], &root).unwrap_err();
        assert!(format!("{err:?}").contains("no oracle"));
    }

    #[test]
    fn stamp_pins_inputs_and_refuses_a_non_file() {
        let root = crate::test_scratch::scratch("piners_reseed", "inputs_and_non_file");
        let dir = root.join("piners/inp");
        write_probe(&dir);
        std::fs::write(dir.join("inputs.json"), b"{\"Source\":\"high\"}\n").unwrap();
        let pin = stamp_one("inp", Path::new("piners/inp"), &root).unwrap();
        assert_eq!(
            pin.inputs.unwrap().path,
            PathBuf::from("piners/inp/inputs.json")
        );

        // A directory where the record should be: not a probe in the walk,
        // and an error rather than a tree digest if stamped.
        let odd = root.join("piners/odd");
        std::fs::create_dir_all(odd.join("tv_record.json")).unwrap();
        std::fs::write(odd.join("strategy.pine"), b"//@version=6\n").unwrap();
        let err = stamp_one("odd", Path::new("piners/odd"), &root).unwrap_err();
        assert!(format!("{err:?}").contains("not a regular file"));
        let registry = root.join("registry");
        let found = discover(&root, &registry).unwrap();
        assert!(!found.probes.contains_key("odd"));
        assert_eq!(found.skipped, vec![PathBuf::from("piners/odd")]);
    }

    fn args_probe(ids: &[&str]) -> CorpusArgs {
        CorpusArgs {
            probe: ids.iter().map(|s| (*s).to_owned()).collect(),
            ..CorpusArgs::default()
        }
    }

    fn args_all() -> CorpusArgs {
        CorpusArgs {
            all: true,
            ..CorpusArgs::default()
        }
    }

    #[test]
    fn plan_switches_a_blessed_probe_from_csv_to_record() {
        let root = crate::test_scratch::scratch("piners_reseed", "plan_csv_to_record");
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        let pins_path = registry.join("pins.toml");
        let dir = root.join("piners/alpha");
        write_probe(&dir);
        let first = plan(None, &pins_path, &root, &registry, &args_all()).unwrap();
        // Bless it, by hand, with a comment.
        let blessed = first.text.replace(
            "[probes.alpha]\n",
            "# alpha is the flagship\n[probes.alpha]\nexpected = \"accepted\"\n",
        );

        // The capture is redone with tvr: the CSV goes, a record arrives.
        std::fs::remove_file(dir.join("tv_trades.csv")).unwrap();
        std::fs::write(dir.join("tv_record.json"), b"{}\n").unwrap();
        let p = plan(Some(&blessed), &pins_path, &root, &registry, &args_probe(&["alpha"]))
            .unwrap();

        assert_eq!((p.diff.added, p.diff.changed, p.diff.removed), (0, 1, 0));
        assert_eq!(p.diff.oracle_switched, vec!["alpha".to_owned()]);
        assert!(p.text.contains("# alpha is the flagship"));
        let data = registry::parse_pins(&p.text, &pins_path).unwrap();
        let alpha = &data.probes["alpha"];
        assert_eq!(alpha.expected.as_deref(), Some("accepted")); // carried
        assert!(alpha.csv.is_none());
        assert!(alpha.record.is_some());

        // A CSV timezone declared for the probe is now dead beside the
        // record, so the reseed refuses until the declaration goes too.
        let with_tz = format!(
            "[probe_config.\"piners/alpha\"]\ntv_trades_csv_tz = \"utc\"\n\n{blessed}"
        );
        let err = plan(Some(&with_tz), &pins_path, &root, &registry, &args_probe(&["alpha"]))
            .unwrap_err();
        assert!(format!("{err:?}").contains("resolves `tv_trades_csv_tz`"));
    }

    #[test]
    fn plan_all_repairs_a_hand_broken_file() {
        let root = crate::test_scratch::scratch("piners_reseed", "plan_repairs");
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        let pins_path = registry.join("pins.toml");
        write_probe(&root.join("piners/alpha"));
        // Hand-edited: the oracle line was deleted, which the loader refuses.
        let broken = "[probes.alpha]\nexpected = \"accepted\"\n\
                      pine = { path = \"piners/alpha/strategy.pine\", xxh128 = \"00\" }\n";
        assert!(registry::parse_pins(broken, &pins_path).is_err());
        let p = plan(Some(broken), &pins_path, &root, &registry, &args_all()).unwrap();
        let data = registry::parse_pins(&p.text, &pins_path).unwrap();
        assert!(data.probes["alpha"].csv.is_some());
        assert_eq!(data.probes["alpha"].expected.as_deref(), Some("accepted"));
    }

    #[test]
    fn plan_probe_names_a_dir_without_an_oracle() {
        let root = crate::test_scratch::scratch("piners_reseed", "plan_near_miss");
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        let dir = root.join("piners/selftest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("strategy.pine"), b"//@version=6\n").unwrap();
        let err = plan(None, &registry.join("pins.toml"), &root, &registry, &args_probe(&[
            "selftest",
        ]))
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("has strategy.pine but no oracle"));
        assert!(msg.contains("piners/selftest"));
    }

    #[test]
    fn discover_errors_on_duplicate_basename_across_roots() {
        let root = crate::test_scratch::scratch("piners_reseed", "duplicate_basename");
        write_probe(&root.join("vendor/engine/validation/same-id"));
        write_probe(&root.join("piners/same-id"));
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();

        let err = discover(&root, &registry).unwrap_err();
        assert!(format!("{err:?}").contains("same-id"));
        assert!(format!("{err:?}").contains("ambiguous"));
    }

    #[test]
    fn restamp_feeds_rehashes_and_errors_on_missing_file() {
        let root = crate::test_scratch::scratch("piners_reseed", "restamp_roles");
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/15m.csv"), b"ohlcv\n").unwrap();

        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m".to_owned(),
            FeedGroup::Roles {
                primary: FilePin {
                    path: "data/15m.csv".into(),
                    xxh128: "stale".into(),
                },
                warmup: None,
                lower: None,
            },
        );

        let stamped = restamp_feeds(&feeds, &root).unwrap();
        assert_ne!(stamped["eth-15m"].roles()[0].1.xxh128, "stale");
        assert_eq!(stamped["eth-15m"].roles()[0].1.xxh128.len(), 32);

        match feeds.get_mut("eth-15m").unwrap() {
            FeedGroup::Roles { primary, .. } => primary.path = "data/missing.csv".into(),
            _ => unreachable!(),
        }
        let err = restamp_feeds(&feeds, &root).unwrap_err();
        assert!(format!("{err:?}").contains("missing.csv"));
    }

    #[test]
    fn restamp_harness_files_stamps_a_path_only_entry_and_errors_on_missing_file() {
        let root = crate::test_scratch::scratch("piners_reseed", "restamp_harness_files");
        std::fs::create_dir_all(root.join("facts")).unwrap();
        std::fs::write(root.join("facts/probe-facts.toml"), b"[probes]\n").unwrap();

        let mut files = BTreeMap::new();
        files.insert(
            "probe-facts".to_owned(),
            HarnessFile {
                path: "facts/probe-facts.toml".into(),
                xxh128: None,
            },
        );
        let stamped = restamp_harness_files(&files, &root).unwrap();
        assert_eq!(stamped["probe-facts"].xxh128.as_ref().unwrap().len(), 32);

        files.get_mut("probe-facts").unwrap().path = "facts/gone.toml".into();
        let err = restamp_harness_files(&files, &root).unwrap_err();
        assert!(format!("{err:?}").contains("gone.toml"));
    }

    #[test]
    fn restamp_feeds_rehashes_a_single_base_group() {
        let root = crate::test_scratch::scratch("piners_reseed", "restamp_base");
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/ohlcv_1m.csv"), b"timestamp,o,h,l,c,v\n1,2,3,4,5,6\n")
            .unwrap();

        let mut feeds = BTreeMap::new();
        feeds.insert(
            "eth-15m-2025".to_owned(),
            FeedGroup::Base {
                base: FilePin {
                    path: "data/ohlcv_1m.csv".into(),
                    xxh128: "stale".into(),
                },
            },
        );

        let stamped = restamp_feeds(&feeds, &root).unwrap();
        match &stamped["eth-15m-2025"] {
            FeedGroup::Base { base } => {
                assert_ne!(base.xxh128, "stale");
                assert_eq!(base.xxh128.len(), 32);
            }
            _ => unreachable!(),
        }
    }
}
