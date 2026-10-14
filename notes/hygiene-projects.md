# Hygiene - per-project layers

Hygiene findings from the hunt for the per-project command layers: pbfhogg/nidhogg (`src/pbfhogg/`, `src/pbfhogg_mod/`, `src/nidhogg/`, `src/osc.rs`), elivagar (`src/elivagar/` incl. `corpus/`, `regress/`, `src/pmtiles.rs`), ratatoskr (`src/ratatoskr/`, `src/ratatoskr_sync/`), piners (`src/piners/` incl. `lint/`, `corpus_db/`), and litehtml/sluggrs/dellingr/mogwai (+ `scripts/litehtml-prepare/`). Themes that recur across projects (state root, output channel, spawning, errors, clean, tests on `/tmp`, …) are filed once in `hygiene-platform.md` (PLT) and referenced here. Siblings: `hygiene-validation.md` (VAL), `hygiene-measurement.md` (MEA). Entries record what hunters reported; nothing has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PRJ-008 - Denmark-only fixtures on dataset-parameterized commands

Reported by: pbfhogg-nidhogg.

Residue: Danish geocode fixtures and default term and the fixed `TimeFilter` date remain, and GETID hit counts are still not recorded.

## PRJ-015 - Cross-repo output contracts are unenforced

Reported by: pbfhogg-nidhogg.

`bench_tiles::is_ready_line` keys on the substring "listening" in nidhogg's stderr; pbfhogg `inspect` and `diff` text formats and nidhogg's shutdown kv are parsed without a pinned contract. The hunter says these can't be enforced mechanically from here; they need a structured output mode in those tools or a pinned fixture test on each side.

## PRJ-031 - Dead code in elivagar

Reported by: elivagar.

Residue: the trait-required `package()` remains.

## PRJ-032 - The ratatoskr protocol list is spelled in eight places

Reported by: ratatoskr.

Residue: `RatatoskrConfig` still has nine `test_endpoint_env_*` fields instead of a protocol-keyed map.

## PRJ-034 - ratatoskr configuration is checked at use, and frontmatter typos are ignored

Reported by: ratatoskr.

Residue: the hard-coded saehrimnir binary path lives in the external ratatoskr checkout, which nothing in this repository can change.

## PRJ-056 - litehtml/sluggrs configuration is untyped

Reported by: small-benches.

Residue: visual thresholds and offender limits are still not configurable (Puppeteer is pinned, with an unverified hand-written lockfile).
