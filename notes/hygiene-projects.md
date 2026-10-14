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

## PRJ-005 - bbox parsing re-implemented

Reported by: pbfhogg-nidhogg.

Residue: bboxes are still strings in config and clap, with coordinate ranges and min/max ordering unvalidated.

## PRJ-006 - Two compression validators that disagree

Reported by: pbfhogg-nidhogg, config-cli-bootstrap.

Residue: there is no shared `Compression: FromStr` for clap and schema defaults are not centralized.

## PRJ-008 - Denmark-only fixtures on dataset-parameterized commands

Reported by: pbfhogg-nidhogg.

Residue: Danish geocode fixtures (`GEOCODE_TEST_QUERIES`, the `geocode` default term) and the fixed `TimeFilter` date remain, and GETID hit counts are not recorded.

## PRJ-009 - Dataset registry writers and naming conventions

Reported by: pbfhogg-nidhogg.

Residue: TOML appender/rewrite consolidation, atomic dataset writes and a shared `CivilDate` for `tools::parse_http_date` and `header` remain.

## PRJ-011 - pbfhogg/nidhogg configuration used before it is validated

Reported by: pbfhogg-nidhogg.

Residue: CLI enum integration and typed resolver errors remain.

## PRJ-012 - pbfhogg/nidhogg policy invented per call site

Reported by: pbfhogg-nidhogg.

Residue: the resolver redesign (`ResolveError` variants and a generic `select_one`) in `src/resolve_parts/**` is not done.

## PRJ-013 - pbfhogg/nidhogg errors and silent fallbacks

Reported by: pbfhogg-nidhogg.

Residue: typed resolver errors remain.

## PRJ-015 - Cross-repo output contracts are unenforced

Reported by: pbfhogg-nidhogg.

`bench_tiles::is_ready_line` keys on the substring "listening" in nidhogg's stderr; pbfhogg `inspect` and `diff` text formats and nidhogg's shutdown kv are parsed without a pinned contract. The hunter says these can't be enforced mechanically from here; they need a structured output mode in those tools or a pinned fixture test on each side.

## PRJ-016 - Stale claims in pbfhogg/nidhogg docs and comments

Reported by: pbfhogg-nidhogg, elivagar.

Residue: snapshot OSC help in `cli/schema.rs` is still stale.

## PRJ-018 - Dead code in pbfhogg/nidhogg

Reported by: pbfhogg-nidhogg.

Residue: unused context parameters still reach the pbfhogg/nidhogg handlers from `verify_cmd.rs` and `bootstrap.rs`.

## PRJ-024 - elivagar configuration and tunables

Reported by: elivagar.

Residue: the elivagar config struct still deserializes tilegen values as strings and validates them at planning time.

## PRJ-026 - elivagar errors

Reported by: elivagar.

Residue: the linked PMTiles decoder's panic risk at the `eliv.rs` seam remains.

## PRJ-029 - Stale claims in elivagar docs and help

Reported by: elivagar, config-cli-bootstrap.

Residue: CLI help for the regress overlay and corpus modes is still stale.

## PRJ-031 - Dead code in elivagar

Reported by: elivagar.

Residue: the unused `_lock`, the elivagar config dead-code allowance and the trait-required `package()` remain.

## PRJ-032 - The ratatoskr protocol list is spelled in eight places

Reported by: ratatoskr.

Residue: `RatatoskrConfig` still has nine separate `test_endpoint_env_*` fields instead of a protocol-keyed map.

## PRJ-033 - ratatoskr paths and harness env have several owners

Reported by: ratatoskr, piners.

Residue: piners still duplicates the harness flag and environment literals ratatoskr now shares.

## PRJ-034 - ratatoskr configuration is checked at use, and frontmatter typos are ignored

Reported by: ratatoskr.

Residue: only the hard-coded sæhrimnir binary path in the external ratatoskr checkout remains.

## PRJ-035 - ratatoskr timing residue

Reported by: ratatoskr.

Residue: the generic child runner's copied wait constants are not injectable, and the sync `--bench` default is still the string literal `"3"`.

## PRJ-037 - ratatoskr errors

Reported by: ratatoskr.

Residue: the generic child runner still classifies a deadline as a `Config` error.

## PRJ-039 - ratatoskr gate rules that can't fail

Reported by: ratatoskr.

Residue: `MetricRule` still deserializes seven independent optional predicates with no rejection of empty or invalid rules at load.

## PRJ-044 - piners registry residue

Reported by: piners.

Residue: the shared `HarnessConfig` projection is not done, and the duplicate quoter in `rustflags.rs` remains.

## PRJ-049 - piners tests

Reported by: piners.

Residue: command-level cases such as ceiling enforcement in `piners/cmd.rs` and `lint/cmd.rs` still lack coverage.

## PRJ-051 - piners resources and cross-module reach

Reported by: piners.

Residue: the shared captured-process runner still buffers whole streams, and the corpus DB has no retention bound.

## PRJ-054 - Three inline-tag lists disagree

Reported by: small-benches.

Residue: the separate inline-tag list in the litehtml-rs repository is not reconciled.

## PRJ-055 - Visual and small-bench defaults, paths and rules duplicated

Reported by: small-benches.

Residue: measured-run dispatch is still repeated across project adapters, and the `approved.png` exemption in the git tree policy is separate from sluggrs' output path.

## PRJ-056 - litehtml/sluggrs configuration is untyped

Reported by: small-benches.

Residue: visual thresholds and offender limits are not configurable, and Puppeteer is not pinned.

## PRJ-059 - Visual/small-bench tests

Reported by: small-benches.

Residue: the litehtml smoke script is not registered as a script_check.

## PRJ-062 - Dead code in the small benches

Reported by: small-benches.

Residue: the `SluggrsSnapshot` dead-code allowance, `MeasureRequest.dataset`/`variant` and the `artifact_dir` compatibility import remain.
