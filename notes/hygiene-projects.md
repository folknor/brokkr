# Hygiene - per-project layers

Hygiene findings from the hunt for the per-project command layers: pbfhogg/nidhogg (`src/pbfhogg/`, `src/pbfhogg_mod/`, `src/nidhogg/`, `src/osc.rs`), elivagar (`src/elivagar/` incl. `corpus/`, `regress/`, `src/pmtiles.rs`), ratatoskr (`src/ratatoskr/`, `src/ratatoskr_sync/`), piners (`src/piners/` incl. `lint/`, `corpus_db/`), and litehtml/sluggrs/dellingr/mogwai (+ `scripts/litehtml-prepare/`). Themes that recur across projects (state root, output channel, spawning, errors, clean, tests on `/tmp`, …) are filed once in `hygiene-platform.md` (PLT) and referenced here. Siblings: `hygiene-validation.md` (VAL), `hygiene-measurement.md` (MEA), `bugs.md` (BUG). Entries record what hunters reported; nothing has been verified.

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

## PRJ-003 - Multi-extract geometry and scratch output names duplicated

Reported by: pbfhogg-nidhogg.

Strip geometry and config JSON duplicated verbatim between `commands.rs` (`MultiExtract`) and `verify_multi_extract.rs`. `"multi-extract"` and `geocode-{dataset}` are each spelled in `output_kind`, `build_body` and `cleanup_output`; `OutputKind::ScratchDir("multi-extract")` claims a `-{dataset}` suffix never used, forcing a special case in `cleanup_output`. nidhogg spells `bench-`, `run-`, `hotpath-ingest-output` inline while the dead `scratch_output_dir()` lists only the first.

## PRJ-004 - Extract strategy → flags mapping has four copies

Reported by: pbfhogg-nidhogg.

`commands.rs` (`Extract` and `MultiExtract`), `bench_extract.rs::strategy_args`, `verify_extract.rs`. Diverged: `commands.rs` emits `-b=<bbox>`, `bench_extract` `-b <bbox>`, so rows under the same `extract` command never pair (BUG-049). `bench_extract.rs` also duplicates the `extract-*` presets in `bench_commands` (hard-coded bbox) with different argv; `suite pbfhogg` runs both, producing two extract row families that can never pair.

## PRJ-005 - bbox parsing re-implemented

Reported by: pbfhogg-nidhogg.

`resolve_bbox`/`validate_bbox` own validation; four other places re-parse: `commands.rs` MultiExtract, `verify_multi_extract.rs`, `nidhogg/client.rs::shrink_bbox`, `bbox_to_api` (no numeric check). `bench_all.rs` reads `ds.bbox` raw, unvalidated.

## PRJ-006 - Two compression validators that disagree

Reported by: pbfhogg-nidhogg, config-cli-bootstrap.

`cli/validation.rs::validate_compression`: zlib 1-9 only, zstd a non-negative `u32`, bare `zlib` rejected. `pbfhogg/mod.rs::parse_compressions`: any `i32`, bare `zlib`/`zstd` expand to defaults; its test `negative_level_is_valid_i32` asserts `zstd:-1` is accepted, which the other refuses. Defaults `zlib:6`/`zstd:3` also spelled in schema `default_value`s.

Enforcement proposed: a `Compression` type with `FromStr` used by clap.

## PRJ-007 - nidhogg server addresses and timeouts

Reported by: pbfhogg-nidhogg.

`.brokkr/nidhogg.pid` in both `server::serve` and `server::stop`; `http://localhost:{port}` in five URL builders while `health_check` builds its own; the 6s readiness timeout exists as `30 × 200ms` in `poll_for_ready` and a literal "6s" in the error; `ITERATIONS=5`, `STARTUP_TIMEOUT=30s` (`bench_tiles.rs`), stop 5s and ready 6s (`server.rs`), curl `--max-time 30`/`--connect-timeout 2` (`client.rs`) — no list, no injection. Six curl invocations with six flag policies (failures: BUG-083). Port resolution: BUG-088.

## PRJ-008 - Denmark-only fixtures on dataset-parameterized commands

Reported by: pbfhogg-nidhogg.

`SUITE_EXTRACT_BBOX` (Copenhagen, `bench_commands.rs`), `GEOCODE_TEST_QUERIES` (Danish cities), `query.rs::FALLBACK_QUERY` (Copenhagen bbox), the GETID IDs ("known to exist in Denmark PBFs"). On another dataset the extract suite measures near-empty work and getid verify diffs two empty outputs. `osc.rs`'s doc sizes input by "the Denmark daily diff" (PRJ-017).

## PRJ-009 - Dataset registry writers and naming conventions

Reported by: pbfhogg-nidhogg.

`download_core.rs` has seven `brokkr.toml` appenders and two line-based rewriters, each calling `project_root.join("brokkr.toml")` and hand-formatting TOML without escaping; the repo already has a comment-preserving `toml_edit` pattern (`guard.rs`, `piners/pins_write.rs`). `{key}-{date}.osm.pbf` and `-with-indexdata.osm.pbf` spelled at five sites (`raw_pbf_path`, `indexed_pbf_path`, `run_as_snapshot`, `run_refresh`, `promote_snapshot`). `download_core.rs` hand-rolls `days_to_civil`/`civil_to_days` and `SystemTime::now()` while `header.rs` uses libc `gmtime` for "what is today". Atomicity: BUG-081.

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

## PRJ-012 - pbfhogg/nidhogg policy invented per call site

Reported by: pbfhogg-nidhogg.

- OSC resolution: `resolve_single_osc` (dispatch), `resolve_default_osc_path` (merge-bench, not snapshot-aware), `get_default_osc_entry` (suite, unverified), `resolve_verify_osc`.
- Scratch cleanup on error: pbfhogg run mode cleans up on failure, bench/hotpath return early via `?` before `cleanup_output`; nidhogg bench cleans the ingest dir every run, hotpath never between runs; `verify_renumber` deletes outputs on success, other verifies keep them.
- HTTP timing: `bench_api` uses curl `time_total` truncated to whole ms (BUG-048) while `verify_batch` wraps the curl spawn in `Instant`.
- Ambient dependencies: `SystemTime::now()` in `download_core::today`; env `PORT`; host tools `curl`, `which`, `pkill`, `chmod`, `osmium` on PATH.
- `nidhogg/bench_tiles.rs` calls `crate::ratatoskr::process::send_signal` (PLT-010).
- Lock labels: PLT-034. Merged-PBF cache key: BUG-080. `cat --type`: BUG-086.

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
- `docs/projects/pbfhogg-vs-elivagar.md`: pbfhogg has one build kind, a uniform `run_external_ok` path, no external baselines, "stderr kv → results.db: never" — all false (`run_external_ok` scrapes stderr kv; read/write/merge use `run_external_with_kv`; osmpbf and planetiler are self-reported via `run_internal`; the suite runs osmium, osmpbf and planetiler baselines). Elivagar half: PRJ-029.
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

## PRJ-021 - elivagar corpus layout and vocabulary are strings

Reported by: elivagar.

Corpus file names: `"digest"` ×5, `"contract.json"` ×5, `"manifest.toml"` ×4, `"leaves"` ×4, `"tiles"`, `build_root.join("corpus")` ×3. `DigestMode` spellings `"leaves"`/`"buckets"` in `digest_text`, `parse_baseline_text`, `cmd.rs::parse_mode` and the bless message; `MutationOp` spellings only in `parse`, CLI takes a `String`, help lists them by hand. Header strings `elivagar-corpus-digest v1`/`-leaves v1` written by writer and parser separately. Tile SVG names `z{z}-x{x}-y{y}` built in `manifest::file_name` and `overlay::dump_overlays`.

Enforcement proposed: a `CorpusLayout` type; clap `ValueEnum` for mode/op; a corpus verdict type owning the exit mapping (BUG-061).

## PRJ-022 - elivagar hashing and outcome accounting duplicated

Reported by: elivagar.

`CanonSink`, `HashSink`, `write_len`, `write_string`, `write_detail_attr` (attr tag bytes 1..7) and `detail_attrs_hash` are duplicated verbatim in `corpus/canonical.rs` and `regress/prepared.rs`; `compare_prepared_component_slices` restates `compare_detail_component_slices`. The eight outcome classes are listed in `DiffTotals`, `LayerCounters`, `ContentCounts`, `add_event_counters`, `DetailOutcome::record`, `passed()`, `counters_are_zero`, `print_text`, `to_json`; `passed()` is a hand-written field list, so a new class wouldn't be gated. The same per-blob semantic hash runs sequentially in `corpus::compute` and in parallel in `regress::fingerprint_blobs`.

Enforcement proposed: a shared canon-sink module; counters as `[u64; N]` indexed by `OutcomeClass`.

## PRJ-023 - elivagar magic numbers, copied values and scattered defaults

Reported by: elivagar.

- MVT field/wire numbers and geometry types 1/2/3 across `canonical.rs`, `render.rs`, `mutate.rs`, `compare_tiles.rs`; canvas 4096, the 500-ring clamp (×4), `#ff00ff` (×4), `format_guard`'s tile type 1/compression 2, bucket zoom 7, manifest max zoom 14; PMTiles header byte offsets in both `pmtiles.rs` and `mutate.rs`.
- Copied from elivagar with nothing keeping them in step: `OVERLAY_BACKGROUND = "#f2efe9"`, layer name `"ocean"` in `compare_detail_layer`, ocean shapefile directory names (`download_ocean.rs` constants, literals in `bench_tilemaker.rs`, typed by users in `[tilegen].ocean`).
- CLI defaults: `default_value = "raw"` ~13×, `"denmark"` ~7× (PLT-027); `max_examples` 20 in CLI and `RegressConfig::default`; `overlay_max` 32 and `sample` 200 hidden as `unwrap_or`; `bench_all` spells `50`/`500_000` again.
- `peak_rss_kb` (VmHWM parse) and `elapsed_ms` duplicate `sidecar.rs` and `duration_ms`.

## PRJ-024 - elivagar configuration and tunables

Reported by: elivagar.

Tilegen string knobs (`tile_format`, `tile_compression`, `compress_sort_chunks`, budgets, `compression_level` 0-10) aren't validated at load; a typo surfaces inside elivagar after a release build. `style.toml` and `manifest.toml` lack `deny_unknown_fields` (a typo'd paint key or `layer` for `layers` is ignored yet changes the style hash). `OUTPUT_RETENTION`, `DIFF_CAP = 100`, `RESIDUAL_CANDIDATES = 8`, `KD_TREE_THRESHOLD`, the overlay panel limit 24 and the Planetiler heap formula have no config surface or index. The docs recommend sibling `[<host>.tilegen.*]` blocks for A/B arms, but `DEFAULT_TILEGEN` has no selector, so a sibling block is config nothing can run.

## PRJ-025 - elivagar output

Reported by: elivagar.

Capture-then-print commands and `compare_tiles`/`pmtiles::run` printing: PLT-004. `verify.rs` discards the child's stderr on success. `emit_corpus` prints "changed run(s)" in buckets mode, where the number is buckets.

## PRJ-026 - elivagar errors

Reported by: elivagar.

`create_dir_all(...).ok()` in the mutate default; `render_archive_tile` renders an absent manifest tile as a blank SVG with no warning; `DevError::Verify` for plain IO in `compare_tiles`; production `expect`/`unreachable!` (PLT-009). Swallowed failures with user-visible consequences: BUG-062, BUG-063, BUG-065.

## PRJ-027 - elivagar hash tests pin the wrong function

Reported by: elivagar.

The equivalence tests pin the streaming hash against `detail_tile_hash`, which production never calls (`#[allow(dead_code)]`, "until that port lands"); regress's pass 3 uses `prepared.rs` digests, so "digest and regress can never silently disagree" is guarded against a function not in the path. There is no golden-value test for the "frozen" canonical hash or the digest domains, so a quiet edit to both hash paths passes. `fixture.rs::TestDir`: PLT-038.

## PRJ-028 - elivagar's decoding seam and command shape are claimed, not held

Reported by: elivagar.

- `eliv.rs` says "brokkr does not decode PMTiles itself" and that it re-exports the protohoggr primitives; `canonical.rs`/`render.rs` import `protohoggr` directly, the canonical tests import `elivagar::tile_detail` directly, `pmtiles.rs` is a complete second PMTiles reader, and `compare_tiles.rs` claims "one decoder in the system rather than three" while there are at least four MVT/protobuf walkers. Enforcement proposed: textlint `\b(elivagar|protohoggr)::` over `src/**/*.rs` (`region = "code"`) excluding `eliv.rs`; replace `pmtiles.rs` with elivagar's reader.
- The rationale for locking inspect/diag/svg/verify ("can't read a mid-write archive") doesn't match: tilegen writes to scratch and renames; the lock actually serialises `cargo build`. `compare-tiles` and `pmtiles-stats` take no lock.
- Build feature sets differ for the same binary: `verify` host features, inspect/diag/svg/ocean-build none, `bench_all` `paths.features`, dispatch `req.features`; alternating commands forces rebuilds.
- `bootstrap(None)` vs `bootstrap(Some(build_root))` chosen per command; `project::require` runs after the lock is taken.
- `svg`/`diag` (shelling out to elivagar) overlap the native `pmtiles-corpus render`/`rings`; the hunter leaves keeping both to the user.

## PRJ-029 - Stale claims in elivagar docs and help

Reported by: elivagar, config-cli-bootstrap.

- `PmtilesCorpusCommand` doc and `--help`: "thin wrapper over `elivagar corpus`", "elivagar owns the value sets and exit codes (0/1/2)", "passed through to elivagar", "elivagar refuses to overwrite" — all false (and why mode/op are still `String`s); two different exit-code lists in one file.
- `regress --overlay` help: "worst structural diffs"; the code emits the first differing tiles.
- `docs/brokkr.toml.datasets.md` documents `[<host>.datasets.<D>.blessed]` ("written by `brokkr bless`, read by `regress`"); both removed, and `deny_unknown_fields` makes following the doc a config error.
- `pbfhogg-vs-elivagar.md`: "no stderr kv into results.db" (false for `bench_self`/`bench_all`); "`meta.locations_on_ways_detected` dropped" (`bench_self` stamps it); "same run mode, no DB" (`run_elivagar_external` sends Run mode to the recording bench); store as `<dataset>-<commit>`; names `bless`.
- `commands.rs` Tilegen doc ("parses kv from stderr"); `resolve_pmtiles_by_commit` and `OUTPUT_RETENTION` docs (`<dataset>-<commit>`, "per dataset" — now dataset and variant); `bench_*` headers mention a nonexistent `run_hotpath()`; `corpus/mod.rs` map calls render a "scaffold"; `contract_diff` mentions an `effective` block `ContractDoc` lacks.

## PRJ-030 - elivagar resources and leftovers

Reported by: elivagar.

Per-tile work is materialised with no bound: `diff::expand`, `fold_leaves` pair vectors, `compare_tiles::addressed_tiles`, `dump_ring_grouping` (decodes the blob once per tile and buffers all lines); on the world artifact (~212M tiles) several GB. `render_archive_tile` re-reads the whole directory per manifest tile. Overlays accumulate stale SVGs from earlier runs. Cleanup on failure differs: Run mode deletes outputs, bench/hotpath leave them in scratch.

## PRJ-031 - Dead code in elivagar

Reported by: elivagar.

`needs_data_dir`, `needs_scratch` (`#[allow(dead_code)]`); `package()`/`metadata()` always None/empty; `ContractState::Unavailable` never constructed; `detail_tile_hash` test-only; unused `_lock` in `cmd::corpus`, `_base` in `svg_staleness`, `_data_dir`/`_scratch_dir` in `bench_all::run`; the hotpath output cleanup (PRJ-020). Stale `#[allow(dead_code)]` on live `compare_detail_features`/`compare_detail_components` and on `TilegenConfig`.

## PRJ-032 - The ratatoskr protocol list is spelled in eight places

Reported by: ratatoskr.

Nine protocols in: `Endpoints` fields, `parse_sentinel`'s nine locals, its match arms, error text and missing-list, `endpoint_env_pairs`, `print_endpoints`, the one-line format in `FixtureSession::start`, and nine `test_endpoint_env_*` config fields. The HTTP-vs-`host:port` rule is written twice (`endpoint_env_pairs`, `print_endpoints`); `127.0.0.1` appears 18 times; endpoints render twice (table and one line). Already diverged: comments and docs say "five" (`READINESS_BUDGET` doc, `ScriptInfo.protocol`, service.md, sync.md, ratatoskr.md: "five-line sentinel"); service.md lists only jmap/graph/gmail as HTTP. Endpoint tests (in `bench_gate.rs`, testing `saehrimnir.rs`) check the scheme for only jmap, gmail, imap.

Enforcement proposed: a `Protocol` enum carrying name, scheme and config key; `Endpoints` as `[u16; N]`; config as a map keyed by protocol; exhaustive matches.

## PRJ-033 - ratatoskr paths and harness env have several owners

Reported by: ratatoskr, piners.

`.brokkr/ratatoskr` root hard-coded in `service_test.rs` (`ARTEFACT_PARENT`), `saehrimnir.rs` (`MOCK_DIR`), `list_smoke.rs` (`SYNC_ARTEFACT_PARENT`), `bench_gate.rs` (gate.db path and error text), `main_parts/commands.rs` (clean). The relative-path rule ("absolute kept, relative joins project root") is implemented four times (`require_path`, `sync_script_dir`, gate script in `run_gate_cohort` and `run_gate_hook`). `BROKKR_HARNESS_ARTEFACT_DIR`/`BROKKR_TEST_BIN_DIR`/`--test-harness` spelled as literal pairs at three ratatoskr sites and two piners sites (`cmd.rs`, `measured.rs`). Namespace collision: BUG-071.

Enforcement proposed: a `RatatoskrLayout` type (PLT-001, PLT-020) and one `harness_env(artefact_dir, bin_dir)` constructor.

## PRJ-034 - ratatoskr configuration is checked at use, and frontmatter typos are ignored

Reported by: ratatoskr.

- Messages: "no `[ratatoskr]` section" three copies; "no `[ratatoskr.harness]`" four, worded two ways; "sæhrimnir binary not found" four copies with three remedies (`cargo build --release` in sæhrimnir's repo, while the real config points at `~/.cargo/bin/saehrimnir`, a `cargo install` layout). `run_sync_bench` repeats `validate_sync_config` inline.
- Checked at use: a gate named `all` refused only in `run_sync_bench`, once per gate in a sweep; a `--gate <typo>` checked after `with_worktree` cut the `--commit` worktree; `fixtures_dir`/`mock_server_binary` existence per command; service cohorts check keys exist but not fixture names (a typo'd fixture fails mid-soak after the build); `service <SCRIPT>` doesn't check mock config before building though `service --all` does; sync --all validates per script after the build; the gate sweep validates up front. Hunter: all belongs in one pass at `parse_ratatoskr`.
- Frontmatter (`discover.rs`): `expected: ignore` → Pass; `ceiling: 5y` → 60s; `preserve_data_dir: maybe` → default; unknown `fixtures:` ignored (runs with no mock, no warning). The test `malformed_ceiling_falls_back_to_default` locks this in.
- Build roots differ: `sync --bench` builds in the cwd when `brokkr.toml` is one level up; smoke/all and service build in `project_root`.
- The config in use hard-codes `mock_server_binary = "/home/folk/.cargo/bin/saehrimnir"`.

Enforcement proposed: reject unknown/unparseable frontmatter keys; parse-time refusals for rule shape, a gate named `all`, and baseline UUID format.

## PRJ-035 - ratatoskr timing constants and small helpers duplicated

Reported by: ratatoskr.

`SIGTERM_FORWARD_BUDGET` (`output.rs`) copies `SHUTDOWN_BUDGET` "in spirit" at 1500 ms; the "50 ms, matching `ServiceClient`" poll is defined in `process.rs` and `output.rs` and is a bare literal twice more in `saehrimnir.rs` beside a 25 ms literal. `READINESS_BUDGET` (10s), `SHUTDOWN_BUDGET`, `DEFAULT_CEILING` (60s), both polls, the 5-line stderr tail and the default `--bench 3` (a clap string) are spread over five files with no injection (hence real `sleep`s in tests). `discover::parse_duration` and `service_test::format_duration` are inverse halves of one grammar in two files, no round-trip test. `summary_to_kv` drops bools, `meta_to_json` keeps them, and `meta_to_json`'s doc ("Same scalar-only rule as `summary_to_kv`: bools pass through") contradicts itself. Gate JSON helpers: MEA-021.

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

Every one of the 17 gates in ratatoskr's config has `success equal = 1` and `exit_code equal = 0` (34 rules); the gate hook runs only after every iteration succeeded and the row hard-codes `exit_code: 0, success: true`, so these bare metrics are constants. `MetricRule.equal_to_baseline: Option<bool>` — `false` means nothing. Empty-predicate rules: BUG-072. Profile check: BUG-068.

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

## PRJ-042 - Dead code and module shape in ratatoskr

Reported by: ratatoskr.

`pid_is_alive`, `wait_for_sentinel`, `SentinelOutcome` are test-only, hidden by `#[allow(dead_code)]`; `send_signal`, `send_signal_pgrp`, `snapshot_proc` carry the same stale allow though used. `write_run_toml`'s `mock_dir` is an unused placeholder (`let _mock_dir_anchor = mock_dir; // future`). `cmd.rs` and `sync.rs` are `include!` shims; `service_suite.rs` depends on imports declared in `service_test.rs` and can't carry `//!` docs; file names (`service_test`, `service_suite`, `list_smoke`, `bench_gate`) are the retired command names; `src/ratatoskr_sync/` isn't a module. Request structs would remove the `too_many_arguments`/`too_many_lines` waivers (PLT-002).

## PRJ-043 - piners registry names, paths and vocabulary have several owners

Reported by: piners.

- `PINS_FILE` in `registry.rs` and `reseed.rs`, bare `"pins.toml"` in `cmd.rs`; `LINTS_FILE` in `lint/registry.rs` and `lint/reseed.rs`, bare `"lints.toml"` in `lint/cmd.rs`; marker names `strategy.pine`/`tv_trades.csv` constants in `reseed.rs` but literals in `registry::verify_probe`; `lint/registry.rs` copies the `"strategy.pine"` label onto lint snippets, where it is wrong.
- `.brokkr/piners/corpus` has three owners (PLT-001); the lint tree is separate again.
- Selector JSON: written by `cmd::selector_json` and `lint/cmd.rs::selector_json`, read by `format::fmt_selector`, `measured::selector_label`, `query::selection_covered`; `selector_label` claims to mirror `fmt_selector` but drops keywords when `--probe` is also given; lint's run table prints raw JSON with every id (what `fmt_selector` was written to fix).
- Disposition labels as strings: corpus `DISPOSITION_LABELS`, match arms and fields in `report.rs::summarize`/`Summary`, `format_summary`; lint `DISPOSITION_LABELS`, literals in `diff::classify`, `== "piners_error"` twice in `lint/cmd.rs`. `const _: () = assert!(len == 8)` checks only the count.
- The "no `[piners.harness]`" message is duplicated word for word in `cmd.rs` and `measured.rs`, as is the whole load → lint → select → verify → feeds pipeline.

Enforcement proposed: text rules for file names and env literals; a serde enum for dispositions.

## PRJ-044 - piners has two registry stacks copied almost line for line

Reported by: piners.

`Registry::load`/`lint` vs `LintRegistry::load`/`lint`; `select.rs` vs `lint/select.rs` ("mirrors"); `pins_write.rs` vs `lints_write.rs` (`set_value`, `sync_opt`, `sort_fields`, `rank`, `set_block_prefix`, `pin_value`, `parse_value`, `toml_str` — a third copy is `rustflags::toml_string`); reseed diff/carry-forward; `grid` in `corpus_db/format.rs` and `lint/db.rs`. Bless is implemented twice with different rules (BUG-090). Directory walks differ: corpus reseed `file_type()` (no symlink follow), lint reseed `is_dir()` (follows; a symlink loop recurses). `lint/cmd.rs` rebuilds a `HarnessConfig` by hand from `LintConfig`'s package/binary/features/debug fields; the config could embed `HarnessConfig`. Hunter's recommendation: a single generic registry, selector, writer and run store with a per-corpus pin type — a rewrite with real payoff.

## PRJ-045 - piners query commands diverge

Reported by: piners.

Run-id precedence: `corpus_query` uses `q.run.or(q.run_id)`, lint `query` uses `q.run_id.or(q.run)`. Lint re-defaults `limit == 0` to 20, so `-n 0` means 20 rows there and 0 in `corpus-results`. Timestamps: MEA-005. Flag combinations silently ignored or widened: `--verify-only --keyword x` widens to the whole universe; `--reseed` ignores `--no-gate`, `--force`, `--release`; in `corpus-results`, `--where` without `--diffs`, `--over` without `--runtimes`, and `--sql`/`--trend` with other flags are ignored while `--columns` without `--diffs` is rejected.

## PRJ-046 - piners configuration and tunables

Reported by: piners.

`RUNTIME_CEILING_MS` is a const with no config key and no injection; its refusal path, `--force` bypass and no-database path are untested (only `estimated_wall_ms` is). The harness exit-code contract (0/1/2) is only a `match` and prose (PLT-028). Checked late: a missing `[piners.harness]` is noticed after full hashing and the ceiling check; a missing `pine_lint_bin` shows as `lint_error` on every probe (which bless then pins, BUG-090); nothing stops `[piners] registry_dir` and `[piners.lint] registry_dir` being the same directory, in which case each loads the other's file as a keyword file and fails to parse. An unknown `TvDiag` severity is dropped by `filter_map` in `tv_anchor` and `LintRegistry::lint` never validates it (serde enum proposed). `measured.rs` debug handling: BUG-047.

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

`RunMeta.stderr` and the lint `run.stderr` column always `""` (one call site); lint `run_migrations` scaffolding has no migrations; the duplicated `project::require` in `measured.rs`. Compatibility paths whose continued need can't be checked from this repo: `report.rs` skipping a legacy `summary` line ("the harness no longer emits one") and `fmt_selector` accepting the legacy string `probe` shape — depends on the piners harness and existing `runs.db` rows.

## PRJ-053 - litehtml and sluggrs visual layers are near-clones that have diverged

Reported by: small-benches.

`open_db`, `format_pct`, `print_table_header`, prefix-matching `resolve_fixture`/`resolve_snapshot`, `print_run_summary`, `format_status_columns` and the approve clean-tree check are cloned; the two `db.rs` differ only in column names. Sluggrs fixed the `FAIL_THRESHOLD (FAIL_THRESHOLD)` double status (its comment describes it); litehtml `format_status_columns` still has `s => format!(" ({s})")`. `sluggrs_msg` wraps nothing. Hunter's view: one `visual/` module generic over subject, with a structural rule that `sluggrs` doesn't import `crate::litehtml`.

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

## PRJ-056 - litehtml/sluggrs configuration is untyped and undocumented

Reported by: small-benches.

`LitehtmlConfig.mode` and `LitehtmlFixture.expected` are free `String`s compared at use (`"ahem"`, `"fail"`), so `"Ahem"`/`"failure"` silently select the other behaviour (serde enums proposed). No config-time check for duplicate fixture/snapshot ids (`fixture_by_id` takes the first) or ids containing `/` or `..` (joined into `fixtures/<id>`). `FUZZ_THRESHOLD`, `POS_TOLERANCE`, `SIZE_TOLERANCE`, `ZERO_HEIGHT`, `REGRESSION_TOLERANCE`, `OFFENDER_PRINT_LIMIT` are scattered with no injection. `[litehtml]` and `[sluggrs]` are documented nowhere: `brokkr.toml.md` points `[litehtml]` to `litehtml.md`, which points back ("See `docs/brokkr.toml.md` for full schema"); `waive_element_threshold`, per-fixture `mode`/`viewport_width`/thresholds, `notes` and the whole `[sluggrs]` schema lack docs. `scripts_dir()` uses `env!("CARGO_MANIFEST_DIR")`, so the installed binary depends on the source checkout existing where it was built; `pnpm install` (not `--frozen-lockfile`) writes into brokkr's tree from any litehtml project, and deps install only when `node_modules` is missing, so a `package.json` bump never re-installs.

## PRJ-057 - Visual status and exit policy invented per project

Reported by: small-benches.

Exit policy lives in positional `counts: [u32; 4]` arrays with different index meanings per project; `NoBaseline` is a failure in litehtml (the `_` arm) and not in sluggrs (`Status::is_failure()` proposed). Status is stored as TEXT and matched back as literals (`"PASS"`, `"NO_BASELINE"`) with no `FromStr`; an unknown value falls through silently. `determine_status` takes seven positional args including two adjacent `Option<f64>` approvals that can be swapped silently (a struct proposed). The child cwd differs: sluggrs hotpath `build_root`, dellingr and mogwai `project_root`. `latest_result_for_*` orders by `datetime('now')` (one-second resolution; same-second runs ambiguous).

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
