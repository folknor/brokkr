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

Residue: bboxes are still strings in config and clap, with ranges and min/max ordering unvalidated.

## PRJ-006 - Two compression validators that disagree

Reported by: pbfhogg-nidhogg, config-cli-bootstrap.

Residue: clap's `validate_compression` is still a separate validator (rejecting bare `zlib`/`zstd`) and schema default literals are not centralized.

## PRJ-008 - Denmark-only fixtures on dataset-parameterized commands

Reported by: pbfhogg-nidhogg.

Residue: Danish geocode fixtures and default term and the fixed `TimeFilter` date remain, and GETID hit counts are still not recorded.

## PRJ-009 - Dataset registry writers and naming conventions

Reported by: pbfhogg-nidhogg.

Residue: no shared `CivilDate` for `tools::parse_http_date` and `header`.

## PRJ-011 - pbfhogg/nidhogg configuration used before it is validated

Reported by: pbfhogg-nidhogg.

Residue: CLI enum integration and typed resolver errors remain undone.

## PRJ-012 - pbfhogg/nidhogg policy invented per call site

Reported by: pbfhogg-nidhogg.

Residue: no `ResolveError` or generic `select_one` in `src/resolve_parts/**`.

## PRJ-013 - pbfhogg/nidhogg errors and silent fallbacks

Reported by: pbfhogg-nidhogg.

Residue: resolver errors are still `DevError`.

## PRJ-015 - Cross-repo output contracts are unenforced

Reported by: pbfhogg-nidhogg.

`bench_tiles::is_ready_line` keys on the substring "listening" in nidhogg's stderr; pbfhogg `inspect` and `diff` text formats and nidhogg's shutdown kv are parsed without a pinned contract. The hunter says these can't be enforced mechanically from here; they need a structured output mode in those tools or a pinned fixture test on each side.

## PRJ-018 - Dead code in pbfhogg/nidhogg

Reported by: pbfhogg-nidhogg.

Residue: unused context parameters still reach the pbfhogg/nidhogg handlers from `verify_cmd.rs` and `bootstrap.rs`.

## PRJ-024 - elivagar configuration and tunables

Reported by: elivagar.

Residue: `TilegenConfig` still deserializes format and budget values as strings and validates them at planning time.

## PRJ-031 - Dead code in elivagar

Reported by: elivagar.

Residue: the trait-required `package()` remains.

## PRJ-032 - The ratatoskr protocol list is spelled in eight places

Reported by: ratatoskr.

Residue: `RatatoskrConfig` still has nine `test_endpoint_env_*` fields instead of a protocol-keyed map.

## PRJ-034 - ratatoskr configuration is checked at use, and frontmatter typos are ignored

Reported by: ratatoskr.

Residue: the hard-coded saehrimnir binary path lives in the external ratatoskr checkout, which nothing in this repository can change.

## PRJ-035 - ratatoskr timing residue

Reported by: ratatoskr.

Residue: the generic child runner's copied wait constants in `output/child.rs` are still not injectable (the `"3"` literal is fixed).

## PRJ-037 - ratatoskr errors

Reported by: ratatoskr.

Residue: `output/child.rs::verdict` still classifies `Ending::Deadline` as `DevError::Config`.

## PRJ-039 - ratatoskr gate rules that can't fail

Reported by: ratatoskr.

Residue: load-time rejection exists (`validate_gates` -> `GateRules::compile`), but `MetricRule` still deserializes seven independent optional predicates, and no code-site note records the adjudication.

## PRJ-044 - piners registry residue

Reported by: piners.

Residue: the duplicate TOML quoter in `rustflags.rs` remains beside `piners/toml_writer.rs::toml_string`.

## PRJ-051 - piners resources and cross-module reach

Reported by: piners.

Residue: the shared captured-process runner still buffers whole streams (`CaptureCap::UNBOUNDED`).

## PRJ-055 - Visual and small-bench defaults, paths and rules duplicated

Reported by: small-benches.

Residue: the `approved.png` exemption in `git/tree_state.rs` still restates sluggrs' output path separately.

## PRJ-056 - litehtml/sluggrs configuration is untyped

Reported by: small-benches.

Residue: visual thresholds and offender limits are still not configurable (Puppeteer is pinned, with an unverified hand-written lockfile).

## PRJ-059 - Visual/small-bench tests

Reported by: small-benches.

Residue: the litehtml smoke script is still not registered as a `[[script_check]]`.
