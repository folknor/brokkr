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

## PRJ-001 - pbfhogg/nidhogg cargo package names restated

Reported by: pbfhogg-nidhogg.

`Project::cli_package()` owns `"pbfhogg-cli"` and `"nidhogg"`; the literal is restated at ~15 pbfhogg sites (`cmd.rs`, `dispatch.rs`, `verify.rs`, `bench_all.rs`, `bench_allocator.rs`, three times in `pbfhogg_mod/download_modes.rs`) and ~8 nidhogg sites (`dispatch.rs`, `cmd.rs`, `commands.rs::package`).

Enforcement proposed: textlint forbidding `"pbfhogg-cli"` outside `project.rs`, or `BenchContext::new` derives the package from `req.project`.

## PRJ-002 - pbfhogg/nidhogg fixture literals duplicated

Reported by: pbfhogg-nidhogg.

`commands.rs::GETID_BENCH_IDS` and `verify_getid_removeid.rs::IDS` identical; `INSPECT_ALL_TAGS_MIN_COUNT` is a constant but `bench_blob_filter.rs` spells `"999999999"`; `"Sort.Type_then_ID"` three times in `verify.rs`; the nidhogg highway filter list six times (`client.rs` ×5, `query.rs::FALLBACK_QUERY`); `"planet.openstreetmap.org"` three times in `download_core.rs`.

## PRJ-005 - bbox parsing re-implemented

Reported by: pbfhogg-nidhogg.

A shared `Bbox` type now owns parsing, strips, quarter shrinking and the pbfhogg and nidhogg renderings, and resolvers pass it typed. Residue: config (`Dataset.bbox`) and clap (`--bbox`) still carry the bbox as a string, parsed at resolution rather than at config load or by a clap value parser; coordinate range and min/max ordering are still not validated.

## PRJ-006 - Two compression validators that disagree

Reported by: pbfhogg-nidhogg, config-cli-bootstrap.

`cli/validation.rs::validate_compression`: zlib 1-9 only, zstd a non-negative `u32`, bare `zlib` rejected. `pbfhogg/mod.rs::parse_compressions`: any `i32`, bare `zlib`/`zstd` expand to defaults; its test `negative_level_is_valid_i32` asserts `zstd:-1` is accepted, which the other refuses. Defaults `zlib:6`/`zstd:3` also spelled in schema `default_value`s.

Enforcement proposed: a `Compression` type with `FromStr` used by clap.

## PRJ-007 - nidhogg server addresses and timeouts

Reported by: pbfhogg-nidhogg.

`.brokkr/nidhogg.pid` in both `server::serve` and `server::stop`; `http://localhost:{port}` in five URL builders while `health_check` builds its own; the 6s readiness timeout exists as `30 × 200ms` in `poll_for_ready` and a literal "6s" in the error; `ITERATIONS=5`, `STARTUP_TIMEOUT=30s` (`bench_tiles.rs`), stop 5s and ready 6s (`server.rs`), curl `--max-time 30`/`--connect-timeout 2` (`client.rs`) - no list, no injection. Six curl invocations with six flag policies (failures: BUG-083). Port resolution: BUG-088.

## PRJ-008 - Denmark-only fixtures on dataset-parameterized commands

Reported by: pbfhogg-nidhogg.

`SUITE_EXTRACT_BBOX` (Copenhagen, `bench_commands.rs`), `GEOCODE_TEST_QUERIES` (Danish cities), `query.rs::FALLBACK_QUERY` (Copenhagen bbox), the GETID IDs ("known to exist in Denmark PBFs"). On another dataset the extract suite measures near-empty work and getid verify diffs two empty outputs. `osc.rs`'s doc sizes input by "the Denmark daily diff" (PRJ-017).

Design hunt (map-projects): `TimeFilter` hard-codes `2024-01-01T00:00:00Z`, so as planet data ages the selected fraction changes and rows are comparable across time only by accident. Rows from different datasets under the same `cli_args` measure different work when GETID IDs are missing; recording the hit count as a metadata observation would make that visible.

## PRJ-009 - Dataset registry writers and naming conventions

Reported by: pbfhogg-nidhogg.

`download_core.rs` has seven `brokkr.toml` appenders and two line-based rewriters, each calling `project_root.join("brokkr.toml")` and hand-formatting TOML without escaping; the repo already has a comment-preserving `toml_edit` pattern (`guard.rs`, `piners/pins_write.rs`). `{key}-{date}.osm.pbf` and `-with-indexdata.osm.pbf` spelled at five sites (`raw_pbf_path`, `indexed_pbf_path`, `run_as_snapshot`, `run_refresh`, `promote_snapshot`). `download_core.rs` hand-rolls `days_to_civil`/`civil_to_days` and `SystemTime::now()` while `header.rs` uses libc `gmtime` for "what is today". Atomicity: BUG-081.

Design hunt (map-projects): three calendar implementations and four string formats - `download_core::today` + `days_to_civil` (Hinnant forward); `download_modes::civil_to_days`, `unix_to_yyyymmdd`, `iso_date_to_yyyymmdd`, `snapshot_key_to_iso_date`, `iso_date_today`, `iso_date_parse_unix`; `tools::parse_http_date` (an inline Hinnant inverse whose comment cites `pbfhogg::download::days_to_civil`); `header.rs` with libc `gmtime_r`. Proposed: a `CivilDate { y, m, d }` in one shared module (it is not pbfhogg-specific) with `from_unix`, `to_unix`, `yyyymmdd()`, `iso()`, `parse_yyyymmdd`, `parse_iso`, `parse_http`, so snapshot keys, `download_date` and Last-Modified comparisons stop each re-deriving the date. A day-range defect in one of the copies is filed in `bugs.md`.

## PRJ-010 - pbfhogg parsers, labels and helpers with several owners

Reported by: pbfhogg-nidhogg.

- Inspect-output parsers: `verify.rs::parse_ordered`, `verify_multi_extract.rs::parse_inspect_counts`, `verify_renumber.rs::parse_total_relations`, with `strip_commas` duplicated in the last two; the format contract with pbfhogg's `inspect.rs` is recorded only in a test comment.
- `cmd.rs::verify_name` claims to "match the labels used by `verify all`", but `verify_all.rs` spells its own; it passes `5` regions to multi-extract, duplicating the CLI default `"5"`.
- UTF-8 path helpers: `pbfhogg::path_strs`, `commands::path_to_string`, the `CommandContext::*_str` family, `nidhogg::client::path_str`, inline `to_str().ok_or_else`; messages disagree, some omit the path.
- "MB": `file_size_mb` decimal; `nidhogg/ingest.rs` 1,048,576 and `verify_renumber::fmt_size` binary.

Enforcement proposed: one `InspectReport` parser with tests from the doc-comment samples.

## PRJ-011 - pbfhogg/nidhogg configuration used before it is validated

Reported by: pbfhogg-nidhogg.

`nidhogg serve` turns a data_dir resolution error into "tiles-only" via `.ok()`; `bench_all` finds its OSC via `get_default_osc_entry`, which returns `None` when more than one OSC is configured (then prints "skipped, no osc file") and bypasses the hash verification `resolve_*` performs (as does `get_pbf_entry` for raw); `cli_adapter.rs` `CatTypeFilter::parse(s).ok()` turns a bad value into an unfiltered `cat`; `tags-filter --input-kind` compared as a string (`== Some("osc")`); `Extract`/`MultiExtract` `--strategy` is a `String` compared to `"all"` and parsed late (`DiffFormat` shows the `ValueEnum` pattern).

Design hunt (map-projects, config-cli): the bad `cat --type` value is safe today only because clap's `value_parser = ["way", "relation"]` gives the same answer as `CatTypeFilter::parse` - two lists of allowed values with nothing linking them; `inspect --type` travels as a raw `String` all the way into argv (`PbfhoggCommand::Inspect.type_filter`); `--index-type` is a `String` forwarded verbatim. The `--strategy all` sentinel, `Suite.name` (`"pbfhogg" | "elivagar" | "nidhogg"` with a hand-written "expected:" list) and `Sync --gate` compared to `GATE_ALL` are the same pattern. Proposed: `clap::ValueEnum` on the enums themselves (as `DiffFormat` already is) and `StrategySel { All, One(ExtractStrategy) }`, parsed before anything is built.

## PRJ-012 - pbfhogg/nidhogg policy invented per call site

Reported by: pbfhogg-nidhogg.

- OSC resolution: `resolve_single_osc` (dispatch), `resolve_default_osc_path` (merge-bench, not snapshot-aware), `get_default_osc_entry` (suite, unverified), `resolve_verify_osc`.
- Scratch cleanup on error: pbfhogg run mode cleans up on failure, bench/hotpath return early via `?` before `cleanup_output`; nidhogg bench cleans the ingest dir every run, hotpath never between runs; `verify_renumber` deletes outputs on success, other verifies keep them.
- HTTP timing: `bench_api` uses curl `time_total` truncated to whole ms (BUG-048) while `verify_batch` wraps the curl spawn in `Instant`.
- Ambient dependencies: `SystemTime::now()` in `download_core::today`; env `PORT`; host tools `curl`, `which`, `pkill`, `chmod`, `osmium` on PATH.
- `nidhogg/bench_tiles.rs` calls `crate::ratatoskr::process::send_signal` (PLT-010).
- Lock labels: PLT-034. Merged-PBF cache key: BUG-080. `cat --type`: BUG-086.

Design hunt (map-projects): "exactly one -> pick it; zero -> error; many -> error naming the flag" is implemented in `resolve_default_entry_path` and again inline in `resolve_single_osc` (which needs the key back and copies the logic, its sorted "available" list and `where_clause` match - built twice in `resolve_osc_range` too); `get_default_osc_entry` is a third version returning `None` on ambiguity. The snapshot-not-found error with its sorted `base`-plus-keys list is built twice (`snapshot_osc_map`, `resolve_snapshot_pbf_path`). Almost every failure in the resolvers is `DevError::Config(String)` (unknown dataset, missing variant, missing file, ambiguous default) or `Preflight(vec![String])` (hash mismatch), so a caller cannot tell "not downloaded" from "misconfigured" from "corrupt". Proposed: one `select_one(map, key: Option<K>, ctx) -> Result<(K, &E), ResolveError>`, and `ResolveError::{UnknownDataset, MissingEntry { kind, key, available }, FileMissing { path }, HashMismatch { path, expected, actual, origin }, Ambiguous { kind, available, flag }}` with one renderer, letting `download` offer "missing file -> fetch it".

## PRJ-013 - pbfhogg/nidhogg errors and silent fallbacks

Reported by: pbfhogg-nidhogg.

Error variants chosen ad hoc (PLT-007). `build_diff_snapshots_context` swallows the file-size error; `bench_planetiler` defaults node/way/relation counts to 0; other nidhogg HTTP fallbacks: BUG-083. `lint deny` opt-outs: PLT-002.

## PRJ-014 - pbfhogg/nidhogg tests that prove nothing

Reported by: pbfhogg-nidhogg.

`commands.rs::supports_hotpath_includes_tool_commands` tests a function returning `true` unconditionally; `nidhogg/commands.rs` tests `package_none_for_api`, `api_needs_server_not_build`, `ingest_needs_build_not_server`, `tiles_needs_build_not_server`, `metadata_is_empty_post_v13` cover methods with no production caller or a constant `Vec::new()`; `verify_diff.rs::denmark_numbers_reconcile` restates the formula instead of calling `compare_diff_classes`; `client.rs::explicit_level_with_defaults_off_still_works` actually tests `query_url`; `bench_commands.rs::extract_variants_use_hardcoded_bbox` asserts only the strategy mapping; no tests for `verify_check_refs.rs` parsers or `bench_tiles::is_ready_line`. Scratch allocation in `download_modes.rs`: PLT-038.

## PRJ-015 - Cross-repo output contracts are unenforced

Reported by: pbfhogg-nidhogg.

`bench_tiles::is_ready_line` keys on the substring "listening" in nidhogg's stderr; pbfhogg `inspect` and `diff` text formats and nidhogg's shutdown kv are parsed without a pinned contract. The hunter says these can't be enforced mechanically from here; they need a structured output mode in those tools or a pinned fixture test on each side.

## PRJ-016 - Stale claims in pbfhogg/nidhogg docs and comments

Reported by: pbfhogg-nidhogg, elivagar.

- `docs/projects/pbfhogg.md` names `build_hotpath_args()` and `result_variant()` (don't exist); says CLI fields go in `src/cli.rs` (they're in `src/cli/schema.rs`); verify counts (PLT-041).
- `docs/projects/nidhogg.md`: status is "PID file under the host's scratch dir" (it's an HTTP health check; the pid file is `.brokkr/nidhogg.pid`).
- `docs/projects/pbfhogg-vs-elivagar.md`: pbfhogg has one build kind, a uniform `run_external_ok` path, no external baselines, "stderr kv → results.db: never" - all false (`run_external_ok` scrapes stderr kv; read/write/merge use `run_external_with_kv`; osmpbf and planetiler are self-reported via `run_internal`; the suite runs osmium, osmpbf and planetiler baselines). Elivagar half: PRJ-029.
- `VerifyPbfArgs` help: OSC "still resolve[s] from the dataset's primary chain" (`resolve_verify_osc` is snapshot-aware).
- `dispatch.rs::run_wallclock_core` claims argv construction is "centralised here"; build_args + io + compression assembly repeats in dry-run, run and hotpath.
- `commands.rs`: Hotpath mode "prepends the binary path (matching the format expected by `run_hotpath_capture`)"; dispatch slices it off (`hotpath_args[1..]`) and dry-run needs a fake binary for it. Headers reference removed `bench_build_geocode_index.rs` and `hotpath.rs`.
- `bench_tiles::ChildGuard::new` says the server is "spawned in its own process group"; `spawn_server` and `Drop` say it isn't.
- `bench_all.rs::parse_stderr_blocks` claims planetiler shares it (planetiler has its own parser with a different unknown-key policy).
- `nidhogg/dispatch.rs`: ingest follows "the standard build+run_external pattern (like pbfhogg)" (uses `run_internal`).
- `verify_merge.rs` "4-tool comparison": osmosis and osmconvert outputs are only printed.
- `CommandParams.jobs` doc: diff/diff-snapshots only (inspect, tags-filter, apply-changes use it); `osc_range` doc says `LO-HI`, the validator requires `LO..HI`.

## PRJ-017 - pbfhogg/nidhogg legacy argv and unbounded input

Reported by: pbfhogg-nidhogg.

"Hotpath legacy" in `commands.rs`: `inspect tags` drops `--type`/`--min-count` in hotpath mode and `apply-changes` forces `--compression zlib` "so hotpath result rows don't shift", so bench and hotpath measure different workloads under one command name. `osc.rs` reads the whole decompressed OSC into one `String`; planet or europe daily diffs are large.

## PRJ-018 - Dead code in pbfhogg/nidhogg

Reported by: pbfhogg-nidhogg.

`nidhogg/commands.rs` `package()`, `needs_build()`, `needs_server()`, `build_args()`, `scratch_output_dir()` have no production caller, under `#[allow(dead_code)] // methods prepared for future dispatch unification`, while dispatch hand-writes `["ingest", pbf, out]` three times; `metadata()` always empty; `result_command()` (`"ingest"`) and `id()` (`"nid-ingest"`) name one thing twice. `PbfhoggCommand::supports_hotpath()` is `true` for every variant (refusal branches in `run_pbfhogg_hotpath` and dry-run dead). `bench_commands::run`: `index_type` always `None`, `osc_path`/`scratch_dir` always `Some`. `pbfhogg/cmd.rs::verify(_project)` and nidhogg `status/query/geocode(_project_root)` take unused args. `verify_multi_extract.rs` builds `multi_args` and immediately overwrites it under a misleading comment.

## PRJ-019 - elivagar tilegen argv and the second bench implementation

Reported by: elivagar.

Tilegen argv is built in `commands.rs::build_args` and `bench_self.rs::run`; `PipelineOpts` assembly (`resolve_tilegen` + `input_assertions`) in the `bootstrap.rs` Tilegen arm and `cmd.rs::bench_all`; `compression_level` is added outside `push_args`, so a third caller would drop it. Example names `bench_pmtiles`/`bench_node_store` appear in `build_config()`, `example()` and both bench modules. `bench_self`, `bench_pmtiles`, `bench_node_store` survive only as `bench all`'s second implementation (BUG-057, BUG-058). Hunter's view: one `ElivagarCommand`-driven path with `bench all` and the `bench_*` modules deleted.

## PRJ-020 - elivagar scratch and archive names

Reported by: elivagar.

`"tilegen_tmp"` in `commands.rs`, `bench_self.rs`, `dispatch.rs` (wipe guard) and comments (clean mislabel: BUG-035); `"bench-self-output.pmtiles"` three times; the hotpath cleanup deletes `hotpath-{alloc-}output.pmtiles`, a name no argv builder produces (dead). Retention pruners: PLT-020.

Design hunt (map-projects): `run_elivagar_hotpath` removes `hotpath-{alloc-}output.pmtiles` while `ElivagarCommand::build_args` always writes `bench-self-output.pmtiles`, and the hotpath path also deletes the tilegen output where run and bench archive it via `rename_elivagar_output` - deleting may be intended, but nothing names the policy. `<data>/tilegen_tmp` is built independently in `elivagar/commands.rs::build_args`, `elivagar/dispatch.rs::rename_elivagar_output` (the "output dir coincides with tmp" guard) and `main_parts/commands.rs` (clean), and documented in `config_parts/schema.rs`; `bench-self-output.pmtiles` is spelled twice inside `commands.rs` (`output_files`, `build_args`). Proposed: one `elivagar::paths::tilegen_tmp(data_dir)`, ideally on elivagar's `ResolvedPaths`.

## PRJ-023 - elivagar magic numbers, copied values and scattered defaults

Reported by: elivagar.

- MVT field/wire numbers and geometry types 1/2/3 across `canonical.rs`, `render.rs`, `mutate.rs`, `compare_tiles.rs`; canvas 4096, the 500-ring clamp (×4), `#ff00ff` (×4), `format_guard`'s tile type 1/compression 2, bucket zoom 7, manifest max zoom 14; PMTiles header byte offsets in both `pmtiles.rs` and `mutate.rs`.
- Copied from elivagar with nothing keeping them in step: `OVERLAY_BACKGROUND = "#f2efe9"`, layer name `"ocean"` in `compare_detail_layer`, ocean shapefile directory names (`download_ocean.rs` constants, literals in `bench_tilemaker.rs`, typed by users in `[tilegen].ocean`).
- CLI defaults: `default_value = "raw"` ~13×, `"denmark"` ~7× (PLT-027); `max_examples` 20 in CLI and `RegressConfig::default`; `overlay_max` 32 and `sample` 200 hidden as `unwrap_or`; `bench_all` spells `50`/`500_000` again.
- `peak_rss_kb` (VmHWM parse) and `elapsed_ms` duplicate `sidecar.rs` and `duration_ms`.

## PRJ-024 - elivagar configuration and tunables

Reported by: elivagar.

Tilegen string knobs (`tile_format`, `tile_compression`, `compress_sort_chunks`, budgets, `compression_level` 0-10) aren't validated at load; a typo surfaces inside elivagar after a release build. `style.toml` and `manifest.toml` lack `deny_unknown_fields` (a typo'd paint key or `layer` for `layers` is ignored yet changes the style hash). `OUTPUT_RETENTION`, `DIFF_CAP = 100`, `RESIDUAL_CANDIDATES = 8`, `KD_TREE_THRESHOLD`, the overlay panel limit 24 and the Planetiler heap formula have no config surface or index. The docs recommend sibling `[<host>.tilegen.*]` blocks for A/B arms, but `DEFAULT_TILEGEN` has no selector, so a sibling block is config nothing can run.

Design hunt (map-projects, config-cli): these are forwarded verbatim, consistent with "brokkr interprets none of this", but the budget strings are sizes, so a typo surfaces only when elivagar rejects the argv after a build; a validated `ByteSize` with a `Display` round trip costs nothing and gives a config-load error, and `tile_format`/`tile_compression` are closed sets. The hunter marks this borderline - a deliberate choice to challenge rather than a defect.

## PRJ-025 - elivagar output

Reported by: elivagar.

Capture-then-print commands and `compare_tiles`/`pmtiles::run` printing: PLT-004. `verify.rs` discards the child's stderr on success. `emit_corpus` prints "changed run(s)" in buckets mode, where the number is buckets.

## PRJ-026 - elivagar errors

Reported by: elivagar.

`create_dir_all(...).ok()` in the mutate default; `render_archive_tile` renders an absent manifest tile as a blank SVG with no warning; `DevError::Verify` for plain IO in `compare_tiles`; production `expect`/`unreachable!` (PLT-009). Swallowed failures with user-visible consequences: BUG-062, BUG-063, BUG-065.

## PRJ-028 - elivagar's decoding seam and command shape are claimed, not held

Reported by: elivagar.

`src/pmtiles.rs` now delegates directory reading and tile addressing to the linked elivagar reader through `eliv.rs`; its own header, varint, directory and Hilbert decoders are gone. Residue:
- `canonical.rs`/`render.rs` import `protohoggr` directly and the canonical tests import `elivagar::tile_detail`, so `eliv.rs` is not the only seam its header claims; `corpus/mutate.rs` keeps a third `read_varint`. Enforcement proposed: textlint `\b(elivagar|protohoggr)::` over `src/**/*.rs` (`region = "code"`) excluding `eliv.rs`.
- The rationale for locking inspect/diag/svg/verify ("can't read a mid-write archive") doesn't match: tilegen writes to scratch and renames; the lock actually serialises `cargo build`. `compare-tiles` and `pmtiles-stats` take no lock.
- Build feature sets differ for the same binary across commands (`verify` host features, inspect/diag/svg/ocean-build none, bench all, dispatch), so alternating commands forces rebuilds.
- `svg`/`diag` (shelling out to elivagar) overlap the native `pmtiles-corpus render`/`rings`; keeping both is the user's call.

## PRJ-029 - Stale claims in elivagar docs and help

Reported by: elivagar, config-cli-bootstrap.

- `PmtilesCorpusCommand` doc and `--help`: "thin wrapper over `elivagar corpus`", "elivagar owns the value sets and exit codes (0/1/2)", "passed through to elivagar", "elivagar refuses to overwrite" - all false (and why mode/op are still `String`s); two different exit-code lists in one file.
- `regress --overlay` help: "worst structural diffs"; the code emits the first differing tiles.
- `docs/brokkr.toml.datasets.md` documents `[<host>.datasets.<D>.blessed]` ("written by `brokkr bless`, read by `regress`"); both removed, and `deny_unknown_fields` makes following the doc a config error.
- `pbfhogg-vs-elivagar.md`: "no stderr kv into results.db" (false for `bench_self`/`bench_all`); "`meta.locations_on_ways_detected` dropped" (`bench_self` stamps it); "same run mode, no DB" (`run_elivagar_external` sends Run mode to the recording bench); store as `<dataset>-<commit>`; names `bless`.
- `commands.rs` Tilegen doc ("parses kv from stderr"); `resolve_pmtiles_by_commit` and `OUTPUT_RETENTION` docs (`<dataset>-<commit>`, "per dataset" - now dataset and variant); `bench_*` headers mention a nonexistent `run_hotpath()`; `corpus/mod.rs` map calls render a "scaffold"; `contract_diff` mentions an `effective` block `ContractDoc` lacks.

## PRJ-030 - elivagar resources and leftovers

Reported by: elivagar.

Per-tile work is materialised with no bound: `diff::expand`, `fold_leaves` pair vectors, `compare_tiles::addressed_tiles`, `dump_ring_grouping` (decodes the blob once per tile and buffers all lines); on the world artifact (~212M tiles) several GB. `render_archive_tile` re-reads the whole directory per manifest tile. Overlays accumulate stale SVGs from earlier runs. Cleanup on failure differs: Run mode deletes outputs, bench/hotpath leave them in scratch.

## PRJ-031 - Dead code in elivagar

Reported by: elivagar.

`needs_data_dir`, `needs_scratch` (`#[allow(dead_code)]`); `package()`/`metadata()` always None/empty; `ContractState::Unavailable` never constructed; `detail_tile_hash` test-only; unused `_lock` in `cmd::corpus`, `_base` in `svg_staleness`, `_data_dir`/`_scratch_dir` in `bench_all::run`; the hotpath output cleanup (PRJ-020). Stale `#[allow(dead_code)]` on live `compare_detail_features`/`compare_detail_components` and on `TilegenConfig`.

## PRJ-032 - The ratatoskr protocol list is spelled in eight places

Reported by: ratatoskr.

Sentinel parsing, endpoint formatting, env selection and status output in `src/ratatoskr/` now read one protocol registry with indexed endpoints, and the "five protocols" claims are corrected. Residue: the config side still has nine `RatatoskrConfig.test_endpoint_env_*` fields in `config_parts/schema.rs` (adding a protocol means a new field, parser line and consumer branch; proposed `BTreeMap<Protocol, EnvName>` keyed by the registry's protocol type), and the endpoint scheme tests still cover only jmap, gmail and imap.

## PRJ-033 - ratatoskr paths and harness env have several owners

Reported by: ratatoskr, piners.

`.brokkr/ratatoskr` root hard-coded in `service_test.rs` (`ARTEFACT_PARENT`), `saehrimnir.rs` (`MOCK_DIR`), `list_smoke.rs` (`SYNC_ARTEFACT_PARENT`), `bench_gate.rs` (gate.db path and error text), `main_parts/commands.rs` (clean). The relative-path rule ("absolute kept, relative joins project root") is implemented four times (`require_path`, `sync_script_dir`, gate script in `run_gate_cohort` and `run_gate_hook`). `BROKKR_HARNESS_ARTEFACT_DIR`/`BROKKR_TEST_BIN_DIR`/`--test-harness` spelled as literal pairs at three ratatoskr sites and two piners sites (`cmd.rs`, `measured.rs`). Namespace collision: BUG-071.

Enforcement proposed: a `RatatoskrLayout` type (PLT-001, PLT-020) and one `harness_env(artefact_dir, bin_dir)` constructor.

## PRJ-034 - ratatoskr configuration is checked at use, and frontmatter typos are ignored

Reported by: ratatoskr.

- Messages: "no `[ratatoskr]` section" three copies; "no `[ratatoskr.harness]`" four, worded two ways; "sæhrimnir binary not found" four copies with three remedies (`cargo build --release` in sæhrimnir's repo, while the real config points at `~/.cargo/bin/saehrimnir`, a `cargo install` layout). `run_sync_bench` repeats `validate_sync_config` inline.
- Checked at use: a gate named `all` refused only in `run_sync_bench`, once per gate in a sweep; a `--gate <typo>` checked after `with_worktree` cut the `--commit` worktree; `fixtures_dir`/`mock_server_binary` existence per command; service cohorts check keys exist but not fixture names (a typo'd fixture fails mid-soak after the build); `service <SCRIPT>` doesn't check mock config before building though `service --all` does; sync --all validates per script after the build; the gate sweep validates up front. Hunter: all belongs in one pass at `parse_ratatoskr`.
- Build roots differ: `sync --bench` builds in the cwd when `brokkr.toml` is one level up; smoke/all and service build in `project_root`.
- The config in use hard-codes `mock_server_binary = "/home/folk/.cargo/bin/saehrimnir"`.

Enforcement proposed: reject unknown/unparseable frontmatter keys; parse-time refusals for rule shape, a gate named `all`, and baseline UUID format.

Design hunt (other-projects): `ScriptInfo` is one struct for two script kinds - `fixture`/`protocol` are "sync-only, service scripts leave None" - so a sync script with no fixture gets all the way to `resolve_info` before it is refused; proposed `ScriptInfo { common, kind: Service | Sync { fixture, protocol } }`. `list_smoke::select_cohort` filters `s.expected.as_str() != "ignored"` on a value that is already the `Expected` enum, while `service_suite` compares the enum; `Ignored` only excludes from selection, so the variant is really `Skip`. Frontmatter typo handling is filed in `bugs.md`.

## PRJ-035 - ratatoskr timing residue

Reported by: ratatoskr.

Budgets, polls, duration parsing and formatting, and the stderr tail now live in `ratatoskr/timing.rs`. Residue: `output.rs` still owns its own copies of the poll interval and the SIGTERM forward budget; the default `--bench 3` is a clap string in `cli/schema.rs`; nothing is injectable, so tests still sleep; and there is no round-trip test between the duration parser and formatter.

## PRJ-036 - ratatoskr output

Reported by: ratatoskr.

`ratatoskr_msg`, fake `[warn]` tags, `(s)` hedges, mixed prefixes and duration formats: PLT-004, PLT-005. Multi-line messages lose the prefix on every line after the first (the `--as-baseline` paste block, the suite soak summary). The comment on `feature_summary` calls it "the `[ratatoskr] building ...` log line"; the line is `[harness]`.

## PRJ-037 - ratatoskr errors

Reported by: ratatoskr.

Runtime failures as `Config` and service's `ExitCode(1)`: PLT-007. `run_gate_hook` canonicalize `.unwrap_or(configured_script.clone())`; `MockOutcome` maps a wait I/O error to `killed_after_budget: true` and counts any SIGKILL (e.g. `--hard`) as budget overrun. Gate blob round-trip and baseline queries: MEA-013.

## PRJ-038 - ratatoskr tests

Reported by: ratatoskr.

`capturing_true_succeeds`/`capturing_false_reports_nonzero_code` (`service_suite.rs`) test `std::process::Command` and coreutils, not brokkr code, while their comment claims they exercise "success vs failure routing through the artefact dir". Three `process.rs` tests test `wait_for_sentinel`, which production never calls. Host dependence: PLT-039. Endpoint coverage: PRJ-032. `open_mem`: MEA-017.

## PRJ-039 - ratatoskr gate rules that can't fail

Reported by: ratatoskr.

`ratatoskr/gate.rs` now compiles rules into `GateRules` (selectors and predicates) before the harness build and returns a `GateVerdict`; `predicate_count` is gone and `equal` values are checked early. The `exit_code`/`success` bare metrics are documented as constants for recorded runs in `ratatoskr-gate.md`. Residue: `MetricRule` in `config_parts/schema.rs` still deserializes seven independent `Option`s, compiled afterwards rather than refused at deserialization; whether the gate configs in the ratatoskr checkout carry constant `success`/`exit_code` rules, and how many gates there are (the guide's counts), needs that checkout to check.

## PRJ-040 - Stale claims in ratatoskr code and docs

Reported by: ratatoskr.

- Help (`service` in `cli/schema.rs`), `service_test` doc, `ratatoskr/mod.rs` and service.md say the build goes via the `[[check]]` sweep (decoupled); sync.md says "harness sweep" and lists "sweep" as a `run.toml` field.
- `--force` described three incompatible ways: `SyncBenchRequest.force` and sync.md say rows "land under the dirty alias"; the harness refuses to store them (`harness_mod/types_run.rs`), which the CLI help says.
- Gate docs: "Every `--gate` invocation writes a row"/"gate rows are always written so a failure stays inspectable" (a harness failure never reaches the hook); missing keys "never silently treated as zero" (`sidecar_to_json` writes 0 for io/cs/fault counters with no samples); "Numeric scalars only" (bools accepted); `--as-baseline` with `--gate all` refused "at parse time" (at dispatch, `validate_gate_selection`).
- service.md: single-script mock artefacts under `.brokkr/ratatoskr/<test>/mock/` (code: `.brokkr/ratatoskr/mock/<fixture>/`).
- `BenchConfig` comment "no best-of-N loop" (it is one); `Expected` doc says it can flip Fail to "expected failure" (only used to skip); `ScriptInfo.fixture` "service scripts leave it None" (service uses it); `RatatoskrConfig.test_endpoint_env_*` "consumed by `sync`" (service too).
- `saehrimnir.rs` header: "existing `wait_for_sentinel` waits for presence" (unused); `process.rs` header: `wait_for_sentinel` "required for manual-matrix items 4 and 5" (nothing uses it).
- `gate.rs` says plumbing lives in `src/ratatoskr/sync.rs` and sync.md says the helpers do; it's an `include!` shim, code in `src/ratatoskr_sync/`.
- Config docs are circular: service/sync/gate docs → `docs/brokkr.toml.md` → `docs/projects/ratatoskr.md` → rustdoc; no doc lists `mock_server_binary`, `fixtures_dir`, `test_endpoint_env_*`, `sync_script_dir` together.
- Doc counts: PLT-041. SIGTERM claim: BUG-069.

## PRJ-041 - ratatoskr lifecycle policy differs per shape

Reported by: ratatoskr.

Artefact placement: sync gets a per-run `mock/`, service shares `.brokkr/ratatoskr/mock/<fixture>`. Mock-pid bookkeeping: `remove` then `clear_mock_pids` in service, `clear` only in sync. Process-group decision: `true` in smoke/service, `false` in bench, each with a paragraph-long justification. Interrupt policy and validate-before-build: PLT-012, PRJ-034. `unix_now()`, `Instant::now` everywhere, the hostname in `run_gate_hook`, and a cwd-relative `canonicalize` of the script are ambient dependencies.

## PRJ-044 - piners registry residue

Reported by: piners.

Selection, registry file loading and dangling-key checks, reseed carry-forward, TOML writer primitives, the table formatter, the run-mode model and the run verdict are shared (`piners/selector.rs`, `run_model.rs`, `toml_writer.rs`, `table.rs`); the remaining differences between the corpus and lint registries are deliberate schema and bless-policy differences. Residue: `lint/cmd.rs` still copies four build fields from `LintConfig` into a synthesized `HarnessConfig` (embedding the common config needs `config_parts/schema.rs`), and `rustflags.rs::toml_string` is a third TOML string quoter.

## PRJ-047 - piners logging gaps

Reported by: piners.

Output channel: PLT-004/PLT-005. Reseed re-hashes every feed group even under `--probe`, and its summary doesn't report whether any feed hash changed. `RunMeta.stderr` is always `""` although its doc says it holds captured validator stderr. `report.rs` drops malformed dense-na sites and unparsable lines with only a stdout warning (intended), never recorded in `runs.db`.

## PRJ-048 - piners errors

Reported by: piners.

Spawn-failure mislabel and `clear_child_pid` omission: PLT-007. `explicit_run_id` returns a `Result` that can never be `Err` (per its comment).

## PRJ-049 - piners tests

Reported by: piners.

`/tmp` usage: PLT-038. The bless tests never read the file they write (they delete the dir, then assert on the in-memory registry). `lint/db.rs::record_run_roundtrips_and_renders` has assertions that can't fail (`runs.contains("diverge_one") || runs.contains("bracket")`, where the first is a probe name never in the runs table; `!runs.is_empty()` where an empty table renders `"(none)"`). The hand-rolled `now_rfc3339`/`civil_from_days` has no tests and `reanchor` reads the clock directly. Stale test comments: `ingest.rs` says `runtime_ms` is "store-only… not yet on any canned query row" and `ProbeLine.runtime_ms` "not yet rendered" (`--runtimes` renders it). `cmd.rs` and `lint/cmd.rs` have no tests (fail-reason mapping, selector JSON, bless on failure, ceiling enforcement).

## PRJ-050 - Stale claims in piners help and docs

Reported by: piners.

- `corpus` help: "parity tiers … do not fail the run yet (baseline work is deferred)" (the gate fails the run).
- `--runtimes` help "shares the pre-run ceiling's per-probe estimate", the `RUNTIME_CEILING_MS` doc, and the `cmd.rs` comment ("sum of each probe's most recent recorded runtime"): the ceiling uses the superset wall now.
- `docs/projects/piners.md`: "The one command is `brokkr corpus`" (`lint-corpus`/`lint-results` exist). `docs/brokkr.toml.piners.md` doesn't document `[piners.lint]`; the `feeds` key: PLT-042.
- `lint-corpus.md`: `path` "relative to the registry's snippet tree" (relative to `corpus_root`); the advisory reads "agree but TV-divergent, anchored Nd ago" (no age; not limited to agreeing probes); TV "times out at 10s" (`Duration::MAX`); the store has an `outcome` column (it doesn't); "`brokkr clean` spares it" (clean never looks at it).
- `lfs.rs` header: "every path that hashes a pinned file first calls `ensure_materialized`" (lint `verify_probe` and lint reseed don't).
- `migrate.rs::run_migrations` doc: a fresh DB gets "the v1 tables" (it gets the current ones).
- The reseed exclusion `path == registry_dir` fails open when the configured path is spelled differently (symlinks, `..`).

## PRJ-051 - piners resources and cross-module reach

Reported by: piners.

Harness stdout is buffered whole in memory; `runs.db` is append-only with full stderr and `trade_diff` rows and no retention. piners and lint reach into `crate::ratatoskr::build` (PLT-010, PLT-019).

## PRJ-052 - Dead code in piners

Reported by: piners.

`RunMeta.stderr` and the lint `run.stderr` column always `""` (one call site); lint `run_migrations` scaffolding has no migrations; the duplicated `project::require` in `measured.rs`. Compatibility paths whose continued need can't be checked from this repo: `report.rs` skipping a legacy `summary` line ("the harness no longer emits one") and `fmt_selector` accepting the legacy string `probe` shape - depends on the piners harness and existing `runs.db` rows.

## PRJ-054 - Three inline-tag lists disagree

Reported by: small-benches.

`compare.rs::is_inline_tag`, `prepare.js` `INLINE_ELEMENTS`, and litehtml-rs `is_inline_tag` (cross-repo, "keep in sync", unenforced). The JS list lacks `font`, `big`, `del`, `ins`, `output` (BUG-102). Enforcement proposed: generate the JS list from one source or a Rust test parsing `prepare.js`; the cross-repo copy needs vendoring or a script check.

## PRJ-055 - Visual and small-bench defaults, paths and rules duplicated

Reported by: small-benches.

- Fallback aspect ratio 2.0 in `cmd.rs` (`unwrap_or(2.0)`) and `prepare.js` (`|| 2.0`, which also turns a configured 0 or NaN into 2.0 silently).
- Outline depth 4 three times: clap `default_value`, the doc comment, `prepare.js` `opts.depth` (unreachable).
- Viewport: default 800 in `CAPTURE_JS` and config; height 600 and the 30s timeout only in the embedded JS.
- `REGRESSION_TOLERANCE` 0.5 re-typed as `- 0.5` for "(improved)" in both `format_status_columns`; the 0.05 delta-display cutoff duplicated.
- Scratch: `.brokkr/dellingr` in `dellingr/cmd.rs::SCRATCH_REL` and `clean_artefact_trees`; `.brokkr/mogwai` copied the pattern but clean never learned it (PLT-020); sluggrs hotpath uses `ctx.paths.scratch_dir` (`data/scratch`), which dellingr's comment says to avoid.
- `snapshots/*/approved.png` built in `sluggrs/cmd.rs` and excluded by literal pathspec in `git.rs::check_clean` (PLT-017).
- The mode-to-features rule (`uses_hotpath` + `if uses_hotpath { hotpath_features } else { features }`) is copied into sluggrs, dellingr and mogwai; the dellingr doc says restating feature names "would only create a way for them to disagree", but the selection logic is restated; only mogwai dedups (`hotpath,hotpath`), and recorded `cargo_features` order differs between mogwai and the others. Proposed: `MeasureRequest::build_features(registered)`.

Design hunt (other-projects): `piners/measured.rs` re-derives `alloc` from a match on `req.mode`, ignores `req.features` (CLI `-F`), pushing only the hotpath feature onto `[piners.harness] features`, and uses `hotpath_feature(alloc)` alone rather than `hotpath_features()`; whether that is intended is stated nowhere. Each of the four files then chooses `run_hotpath` + `run_hotpath_capture` or `run_external`, the `harness::hotpath_feature(...)` label, `hotpath_alloc_note` and a scratch dir - the same ~40-line decision four times. Proposed: the mode question on `MeasureMode` (`fn instruments(&self)`) and a core `run_measured_target(ctx, config, argv, env)` the four adapters call.

## PRJ-056 - litehtml/sluggrs configuration is untyped

Reported by: small-benches.

Duplicate and path-unsafe ids are refused and `mode`/`expected` are validated, both schemas are documented, and the Node scripts are bundled and installed into an XDG cache keyed by package, lockfile and script contents. Residue:
- `LitehtmlConfig.mode` and `LitehtmlFixture.expected` are still `String`s in `config_parts/schema.rs`, validated at command entry rather than deserialized as enums.
- `FUZZ_THRESHOLD`, `POS_TOLERANCE`, `SIZE_TOLERANCE`, `ZERO_HEIGHT`, `REGRESSION_TOLERANCE` (in `litehtml/compare.rs`) and `OFFENDER_PRINT_LIMIT` (in `cmd.rs`) are not injectable.
- Capture still resolves puppeteer from global npm, so the Chrome behind reference captures is pinned by no lockfile.
- `scripts/litehtml-prepare/package.json` asks for cheerio `^1.2.0` while the lockfile records `^1.0.0`, so the cache install cannot be frozen.

## PRJ-058 - Visual failures don't say why

Reported by: small-benches.

Sluggrs render failure prints only the first stderr line; pixel-compare errors print nothing (BUG-100).

## PRJ-059 - Visual/small-bench tests

Reported by: small-benches.

Nothing tests litehtml or sluggrs `cmd`/`db` (approve, status columns, prefix resolution, the shared-file schema interaction; BUG-101). `smoke.js` runs nowhere (needs node and `node_modules`; no `[[script_check]]` or test runs it), cites `HARNESS-IMPROVEMENTS.md` (not in this repo), writes `smoke-tmp/` into the source tree (not gitignored), yet `visual.md` presents it as the assertion of the fidelity rules. Capture depends on a globally installed, unpinned puppeteer found via `npm root -g`, spawned per fixture; `chrome.meta.json` exists only to explain drift afterwards (adding puppeteer to `package.json` and the lockfile proposed). The `FUZZ_THRESHOLD` const-asserts pin the literal 13 rather than computing "5% of 255". The `br_detected_by_tag_or_path` comment mentions a "`br[` prefix probe" while `is_br` compares tags for equality. `head_paths_filtered` never tests `thead` (BUG-098).

## PRJ-060 - Stale claims in small-bench docs

Reported by: small-benches.

`mogwai.md` "Datasets" says an entry records "whether the bytes moved under a recorded row", but the mogwai bench path never reads datasets and rows carry no digest. `visual.md` describes an `expected_fail` flag (the field is `expected = "fail"`) and says both projects compare against Chrome (sluggrs compares against `approved.png`); its approve clean-tree rationale is unenforced: `approve` pins HEAD to the latest result/`output.png`, which may come from a dirty run or older commit, and `mechanical_runs.dirty/commit` is never consulted. `dellingr.md` pair key and FIFO location: MEA-018. `visibility.rs` claims: PLT-023, BUG-101.

## PRJ-061 - `prepare.js` caches grow without bound

Reported by: small-benches.

The image cache and permanent `.miss` negative entries (a transient failure is cached forever) never shrink; `fetchUrl` has no response size cap; its option parser ignores unknown flags.

## PRJ-062 - Dead code in the small benches

Reported by: small-benches.

`latest_run` and `RunSummary` in litehtml `#[allow(dead_code)]` with no callers; sluggrs `latest_run` uncalled. Behind blanket allows: `PixelDiffResult.total_pixels`/`diff_pixels` unused; `ElementMatchResult.total_elements`/`passing_elements` test-only; `SnapshotMeta.backend` and `LitehtmlFixture.notes` unused. Stale allows on sluggrs `map_run_summary` (used) and `SluggrsSnapshot` (every field read). Litehtml's `artifact_dir` migration is a compatibility path and the only reason litehtml writes `user_version` (BUG-101). The `(Some, None) | (None, Some)` arm in `dellingr::workload::resolve` is unreachable (the parser rejects it) and carries a second wording of the parser's error. `MeasureRequest.dataset`/`variant`: PLT-027. The `print!` in `outline` is legitimate data output (noted only as the one raw stdout write in `litehtml/cmd.rs`).
