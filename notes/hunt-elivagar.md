I found about 70 findings in scope, grouped below under the eight questions. Nothing was edited or built. The first seven are the most consequential: all are wrong today, not predictions.

**Wrong today**
- **`bench all` probably fails at its first arm.** `src/elivagar/bench_self.rs` still runs tilegen through `run_external_with_kv_raw`, which errors when stderr has no `elapsed_ms=` line (`parse_kv_stderr`). The docs and `dispatch.rs` both say tilegen stopped printing that at elivagar 54f9b07. I can't see the elivagar side, so this is inferred rather than run.
- **`bench all`'s pmtiles and node-store arms run a second copy of the same benchmark.** `bench_pmtiles.rs` / `bench_node_store.rs` pass `--runs N`, print the child's output, and record `tiles` / `internal_runs` values. The regular path (`dispatch.rs::run_elivagar_internal`) passes `--runs 1`, ignores `req.runs()` while printing "N run(s)", and records no values. So `--runs` is silently ignored for `pmtiles-writer` / `node-store`, and the two paths file rows of different shapes under the same command name.
- **`pmtiles-corpus` defaults can never succeed.** Every subcommand defaults to `--variant raw`, but `bless` refuses any archive that isn't locations-generated (`corpus/mod.rs::bless`). A default `check` against a locations corpus hits a contract mismatch.
- **The style file is found by two different rules.** `cmd.rs` defaults it to `build_root/corpus/style.toml`; `corpus_style_path` uses the corpus dir's parent. With `--corpus` pointed elsewhere, `render-manifest` records one style's hash and `check` compares against another, so it reports stale forever (exit 3).
- **Exit 1 is still used for failures that aren't a content mismatch.** `baseline_material` only covers step 1. In step 4, a missing `style.toml`, a bad `manifest.toml`, or bad `contract.json` JSON escapes as `DevError::Io`, which exits 1 ("the archive regressed"). So do bless's `parse_baseline(...)?` path, and every resolve, lock or bootstrap error in `cmd::corpus` and `cmd::regress`. Separately, corpus uses 2 for "archive refused", which collides with clap's usage-error 2; regress deliberately avoided that. The mapping from outcome to exit code is written out separately in `corpus::Outcome::exit_code`, `regress::failed`/`run`, and `bootstrap.rs` (`Err(_) => 1`).
- **The run-mode example path compiles without the lock.** `dispatch.rs::run_elivagar_run` (`BuildKind::Example`) calls `cargo_build` with no lock (its own comment admits it), and `run_measured` doesn't take one. A guarded host should refuse the build; an unguarded one builds concurrently.
- **`docs/brokkr.toml.datasets.md` still documents `[<host>.datasets.<D>.blessed]`** ("written by `brokkr bless`, read by `regress`"). Both were removed; with `deny_unknown_fields`, following the doc produces a config error.

**1. One value, one owner**
- `"tilegen_tmp"` is written in `commands.rs`, `bench_self.rs`, `dispatch.rs` (the wipe guard) and several comments. `clean_scratch` wipes `paths.scratch_dir` and *labels* it "tilegen_tmp". `data/tilegen_tmp` itself is only cleaned if a host happens to set scratch to it, so the `dispatch.rs` dry-run comment ("a routine `brokkr clean` reclaims `tilegen_tmp`") holds only on such hosts.
- `"bench-self-output.pmtiles"` is spelled 3 times. The hotpath cleanup deletes `hotpath-{alloc-}output.pmtiles`, a name no argv builder produces, so that cleanup is dead.
- Tilegen argv construction exists twice: `commands.rs::build_args` and `bench_self.rs::run`. So does the `PipelineOpts` assembly (`resolve_tilegen` + `input_assertions`): `bootstrap.rs` Tilegen arm and `cmd.rs::bench_all`. `compression_level` is added outside `push_args`, so any third caller silently drops it.
- Example names `bench_pmtiles` / `bench_node_store` appear in `build_config()`, `example()`, and both bench modules.
- "Keep the newest N by mtime" exists twice: `dispatch.rs::prune_output_dir` (`OUTPUT_RETENTION = 5`, not configurable, unlike `worktree_keep`) and `clean_archives` (`--keep`). The deep clean `clean_elivagar_outputs` deletes *every* `*.pmtiles` in the output dir. That breaks the "can't name it by construction, don't delete it" rule its sibling documents, and would take `ocean-tiles.pmtiles` if a host set `output = "data"`.
- `clean` wipes `data/ocean-build_tmp`, a name brokkr never passes to elivagar (`ocean_build.rs` has no tmp flag). It is elivagar's default, copied into brokkr, so the "brokkr-designated" claim is false.
- Corpus file names are scattered: `"digest"` ×5, `"contract.json"` ×5, `"manifest.toml"` ×4, `"leaves"` ×4, `"tiles"`, and `build_root.join("corpus")` ×3. A `CorpusLayout` type would own them.
- `DigestMode` spellings `"leaves"`/`"buckets"` appear in `digest_text`, `parse_baseline_text`, `cmd.rs::parse_mode` and the bless message. `MutationOp` spellings live only in `parse`, the CLI takes a `String`, and help lists them by hand.
- Header strings `elivagar-corpus-digest v1` / `-leaves v1` are written once by the writer and once by the parser.
- `CanonSink`, `HashSink`, `write_len`, `write_string`, `write_detail_attr` (the attr tag bytes 1..7) and `detail_attrs_hash` are duplicated verbatim in `corpus/canonical.rs` and `regress/prepared.rs`. `compare_prepared_component_slices` restates `compare_detail_component_slices`.
- The 8 outcome classes are listed out in `DiffTotals`, `LayerCounters`, `ContentCounts`, `add_event_counters`, `DetailOutcome::record`, `passed()`, `counters_are_zero`, `print_text` and `to_json`. `passed()` is a hand-written field list, so a new class wouldn't be gated. A `[u64; N]` indexed by `OutcomeClass` would make that unrepresentable.
- Magic numbers:
  - MVT field and wire numbers and geometry types 1/2/3 recur across `canonical.rs`, `render.rs`, `mutate.rs` and `compare_tiles.rs`.
  - Canvas size 4096, the 500-ring clamp (×4), `#ff00ff` (×4) and `format_guard`'s tile type 1 / compression 2 are all bare literals.
  - PMTiles header byte offsets appear in both `pmtiles.rs` and `mutate.rs`.
  - The bucket zoom 7 and manifest max zoom 14 are also literals.
- Tile SVG file names: `z{z}-x{x}-y{y}` is built in both `manifest::file_name` and `overlay::dump_overlays`.
- Values copied from elivagar with nothing keeping them in step: `OVERLAY_BACKGROUND = "#f2efe9"`, the layer name `"ocean"` in `compare_detail_layer`, and the ocean shapefile directory names (`download_ocean.rs` constants plus literals in `bench_tilemaker.rs`, also typed by users in `[tilegen].ocean`).
- CLI defaults are scattered:
  - `default_value = "raw"` about 13×, `"denmark"` about 7×.
  - `bootstrap.rs` hardcodes `"denmark","raw"` for the synthetic benches.
  - `max_examples` 20 is set in both the CLI and `RegressConfig::default`.
  - `overlay_max` 32 and `sample` 200 are hidden in code as `unwrap_or`.
  - `bench_all` spells `50` / `500_000` again.
- The `pmtiles-stats` project set is spelled in `cmd_pmtiles_stats` and in `visibility.rs::TABLE`. Its test re-lists the set by hand instead of reading `TABLE`.
- `--commit` is not normalised: archive names use `git rev-parse --short` (whose length grows over time), and a user-supplied hash of a different length just fails with "no build".
- `check_unzip` ×2 and `check_ogr2ogr` use a local `which` pattern that `tools.rs`/`preflight.rs` already implement.
- `peak_rss_kb` (VmHWM parse) and `elapsed_ms` duplicate `sidecar.rs` and `duration_ms`.
- **Enforceable:** yes, by types (clap `ValueEnum` for mode/op/variant defaults, a layout struct, a shared canon-sink module, class-indexed counters) plus a test that compares against `TABLE`.

**2. Values nobody can find, change, or trust**
- Tilegen string knobs (`tile_format`, `tile_compression`, `compress_sort_chunks`, budgets, `compression_level` 0-10) are not validated at load. A typo surfaces inside elivagar after a release build. Serde enums or validated newtypes would fix this.
- `style.toml` and `manifest.toml` lack `deny_unknown_fields`. A typo'd paint key or `layer` for `layers` is silently ignored, yet still changes the style hash.
- `OUTPUT_RETENTION`, `DIFF_CAP = 100`, `RESIDUAL_CANDIDATES = 8`, `KD_TREE_THRESHOLD`, the overlay panel limit of 24 and the Planetiler heap formula have no config surface and no single index.
- The docs recommend sibling `[<host>.tilegen.*]` blocks for A/B arms, but `DEFAULT_TILEGEN` has no selector, so a sibling block is config nothing can run.

**3. One channel, one implementation**
- `inspect.rs`, `diag.rs`, `svg.rs`, `bench_pmtiles.rs` and `bench_node_store.rs` each hand-roll "capture, then `print!`/`eprint!`, then `check_success`". The inspect/diag/svg headers claim output is "streamed directly"; it is buffered until the child exits.
- `compare_tiles` prints census lines through `bench_msg` (`[bench]`) mixed with raw `println!`. `pmtiles::run` prints errors with `println!`.
- `verify.rs` discards the child's stderr on success.

**4. Errors**
- Swallowed errors:
  - `pmtiles::run` prints read failures and returns `Ok`, so `pmtiles-stats` on a missing or corrupt file exits 0.
  - `rename_elivagar_output` logs rename, create-dir and coinciding-dir failures but still returns success, so a tilegen "passes" with no durable archive.
  - A git failure names the archive `…-unknown.pmtiles`, which the resolver never finds.
  - `create_dir_all(...).ok()` in the mutate default.
  - `bench_all` downgrades planetiler/tilemaker failures to "skipped".
  - `render_archive_tile` renders an absent manifest tile as a blank SVG with no warning.
- Panics on input an operator or committed file controls:
  - `pmtiles.rs::read_varint` indexes out of bounds on truncated data, and `decode_directory`'s `val - 1` can underflow.
  - `vec![0; length]` allocates whatever a corrupt header says.
  - Committed `leaves`/`digest` z/x/y and run lengths go unchecked into `xy_to_tile_id` and per-tile loops.
- Internal `expect`/`unreachable!` in production: `engine.rs`, `compare.rs`, `overlay.rs`, `pairing.rs`, `mutate.rs`, `dispatch.rs`.
- `DevError::Verify` is used for plain IO failures in `compare_tiles`.

**5. Tests that prove nothing**
- The equivalence tests pin the streaming hash against `detail_tile_hash`, which production never calls (`#[allow(dead_code)]`, "until that port lands"). Regress's pass 3 uses `prepared.rs` digests, so the claim "digest and regress can never silently disagree" is guarded against a function that isn't in the path.
- There is no golden-value test for the "frozen" canonical hash or the digest domains. A quiet edit to both hash paths still passes the equivalence tests.
- `corpus/fixture.rs::TestDir` is a second scratch allocator beside `src/test_scratch.rs` (which is documented as the only one). Its per-process counter restarts in every nextest process, it keeps leftovers from earlier runs, and it writes to `CARGO_MANIFEST_DIR/target` regardless of the configured target dir. A textlint rule could forbid `CARGO_MANIFEST_DIR` outside `test_scratch.rs`.

**6. Claims that no longer hold**
- The `PmtilesCorpusCommand` doc comment and its `--help` texts still say "thin wrapper over `elivagar corpus`", "elivagar owns the value sets and exit codes (0/1/2)", "passed through to elivagar", "elivagar refuses to overwrite". All false: this is why mode and op are still `String`s.
- The `regress --overlay` help says "worst structural diffs"; the code emits the *first* differing tiles.
- `eliv.rs` says "brokkr does not decode PMTiles itself" and that it re-exports the protohoggr primitives. In fact:
  - `canonical.rs` and `render.rs` import `protohoggr` directly, and the canonical tests import `elivagar::tile_detail` directly.
  - `pmtiles.rs` is a complete second PMTiles reader.
  - `compare_tiles.rs` says there is "one decoder in the system rather than three". There are at least four MVT/protobuf walkers.
  - **Enforceable:** a textlint rule `\b(elivagar|protohoggr)::` over `src/**/*.rs` with `region = "code"`, excluding `eliv.rs`. `dependency_rule` can't do it because the crate is single-package.
- The rationale for locking inspect/diag/svg/verify ("can't read a mid-write archive") doesn't match the code: tilegen writes into scratch and renames into the store. The lock actually serialises the `cargo build`. `compare-tiles` and `pmtiles-stats` take no lock at all.
- `pbfhogg-vs-elivagar.md` is stale in four places:
  - "no stderr kv into results.db": false for `bench_self` and `bench_all`.
  - "`meta.locations_on_ways_detected` dropped": `bench_self` still stamps it.
  - "same run mode, no DB": false, since `run_elivagar_external` sends Run mode to the recording bench.
  - It still describes the store as `<dataset>-<commit>` and names `bless`.
- `output-channels.md` says example benches' "stderr … discarded": false on the `bench all` path.
- Other stale comments:
  - `commands.rs` Tilegen doc ("parses kv from stderr").
  - `resolve_pmtiles_by_commit` and `OUTPUT_RETENTION` docs (`<dataset>-<commit>`).
  - `bench_*` headers mention a `run_hotpath()` that doesn't exist.
  - `corpus/mod.rs` map says render is a "scaffold".
  - `contract_diff` mentions an `effective` block that `ContractDoc` doesn't have.
  - `emit_corpus` prints "changed run(s)" in buckets mode, where the number is buckets.
  - Several headers cite `brokkr.md` / `elivagar.md`, which don't exist in this repo.
  - `Cargo.toml` description lists 3 projects.
- The 2026-07-14 incident is retold in 5 places.

**7. Policy invented per call site**
- The per-tile budget is materialised per site, with no bound: `diff::expand`, `fold_leaves` pair vectors, `compare_tiles::addressed_tiles` and `dump_ring_grouping` (decodes the blob once per *tile* and buffers all lines). On the world artifact (~212M tiles) that is several GB.
- `render_archive_tile` re-reads the whole directory per manifest tile.
- The same per-blob semantic hash runs sequentially in `corpus::compute` and in parallel in `regress::fingerprint_blobs`.
- Cleanup on failure differs by path: Run mode deletes outputs, while bench and hotpath leave them in scratch.
- Build feature sets differ for the same binary: `verify` uses host features, inspect/diag/svg/ocean-build none, `bench_all` `paths.features`, dispatch `req.features`. Alternating commands forces rebuilds.
- `bootstrap(None)` vs `bootstrap(Some(build_root))` is chosen per command. `project::require` runs *after* the lock is taken.
- External tools:
  - Planetiler keeps `--download` inside the timed runs, so network time lands in the measurement.
  - Its priming check (`data_dir/sources`) likely looks in the wrong place, because the child runs from `project_root`.
  - Tilemaker's `exists()` check fails on a dangling symlink.
  - Downloads get no hash check, and a partial unzip counts as "present".
- Overlays accumulate stale SVGs from earlier runs in the same directory.

**8. No longer load-bearing**
- Dead today:
  - `needs_data_dir` and `needs_scratch` (`#[allow(dead_code)]`).
  - `package()` and `metadata()`, which always return None/empty.
  - `ContractState::Unavailable`, never constructed.
  - `detail_tile_hash`, only reachable from tests.
  - The unused `_lock` parameter in `cmd::corpus`, `_base` in `svg_staleness`, `_data_dir`/`_scratch_dir` in `bench_all::run`.
  - The hotpath output cleanup.
- Stale `#[allow(dead_code)]` on the live `compare_detail_features`/`compare_detail_components` and on `TilegenConfig`.
- `bench_self`, `bench_pmtiles` and `bench_node_store` survive only as `bench all`'s second implementation. They should be deleted in favour of dispatching `ElivagarCommand`.
- `svg`/`diag` (shelling out to elivagar) now overlap the native `pmtiles-corpus render`/`rings`. Whether to keep both is a decision for you.

**Where to spend effort**
My view is that it goes into four things:
- One `ElivagarCommand`-driven path, with `bench all` and the `bench_*` modules deleted.
- A `Corpus` layout/verdict type that owns the file names, the exit-code mapping (clap's 2 kept reserved) and the `ValueEnum` mode/op types.
- A shared MVT/canonical-sink module behind the `eliv.rs` seam, held by the textlint rule, with `pmtiles.rs` replaced by elivagar's reader.
- Golden-hash tests.
