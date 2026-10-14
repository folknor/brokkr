# Design hunt: map-projects scope

Scope: `src/pbfhogg/`, `src/pbfhogg_mod/`, `src/elivagar/` (dispatch/commands/mod; the corpus/regress subtrees only touched in passing), `src/nidhogg/`, `src/osc.rs`, `src/pmtiles.rs`, `src/resolve.rs` + `src/resolve_parts/`, `src/preflight.rs`, `src/tools.rs`.

Ordered roughly by payoff. Each finding names the question, the sites, whether they already disagree, and where the owner should live.

---

## A. Bugs and live disagreements found along the way

These come first because several are the scattered decisions below, already drifted.

1. **`verify multi-extract` still has the JSON-splicing bug that `commands.rs` fixed.** `pbfhogg/commands.rs::multi_extract_plan` builds the extract config with `serde_json`. Its comment says the directory "used to be spliced in with `display()`, which broke on a `"` or `\`". `pbfhogg/verify_multi_extract.rs::run` still builds the same JSON with `format!` and `multi_dir.display()`. It also leaves out the `is_finite` check the bench path has, and it computes the longitude strips with its own copy of the arithmetic. These are two copies of one decision ("how a bbox is split into N strips and serialised as a pbfhogg extract config"), and they have already drifted. The same function also has a dead first assignment of `multi_args` (it is built, then overwritten at once).

2. **`cat --type` with a bad value silently benches unfiltered `cat`.** `cli_adapter.rs` does `CatTypeFilter::parse(s).ok()`, so a parse failure becomes `None`, and `None` means no filter. Today it is safe only because clap's `value_parser = ["way", "relation"]` in `cli/schema.rs` gives the same answer as `CatTypeFilter::parse`. That is two lists of allowed values with nothing linking them. `inspect --type` is worse: it travels as a raw `String` all the way into argv (`PbfhoggCommand::Inspect.type_filter`). Fix: make it `clap::ValueEnum` on the enum itself, as `DiffFormat` already is, so one list exists. The same applies to `tags-filter --input-kind` (a string compared with `== Some("osc")`), `ExtractStrategy` (`parse` from a string, plus `bench_extract::ALL_STRATEGIES` as a separate `&[&str]`) and `--index-type` (a `String` forwarded verbatim).

3. **Output dir naming disagrees for `MultiExtract`.** `output_kind()` says `ScratchDir("multi-extract")`, and `scratch_output_path` / `cleanup_output`'s generic arm turn that into `<scratch>/multi-extract-<dataset>`. The plan actually writes to `<scratch>/multi-extract`. `cleanup_output` gets around this with a hard-coded special case that runs before the generic match. So `OutputKind` is wrong for this variant, and the special case is what keeps cleanup working. `BuildGeocodeIndex` has the same problem: `build_body` builds `geocode-<dataset>` itself instead of calling `scratch_output_path`, which keeps the two spellings separate. `OutputKind::None => PathBuf::new() // Should not be used.` is a panic-free lie: `promote_artifact` calls `scratch_output_path` with no output-kind check, so `--as-snapshot` on a no-output command would try to promote `""`.

4. **Run mode records results for some commands.** `nidhogg/dispatch.rs` sends `MeasureMode::Run` for `Api`/`Tiles` straight into `cmd::bench_api`/`bench_tiles`, and `elivagar/dispatch.rs::run_elivagar_external` does the same for Planetiler/Tilemaker. Those are the bench paths: `HarnessContext::new(..., req.force, ...)` with the dirty-tree gate and, unless the harness keys off `req.mode`, a results row. Elsewhere the rule is "Run mode never stores results, so dirty tree is always fine" (stated in three dispatchers). Please verify whether the harness suppresses the row in Run mode. Either way, "is this run recorded?" is decided per dispatcher, not by the mode.

5. **Elivagar tilegen hotpath cleans up a file nobody writes.** `run_elivagar_hotpath` removes `hotpath-{alloc-}output.pmtiles`, but `ElivagarCommand::build_args` always writes `bench-self-output.pmtiles`. The hotpath path also deletes the tilegen output where run/bench archive it via `rename_elivagar_output`. Deleting it may be intended, but nothing names the policy. On a non-zero exit, run mode deletes the scratch output, while bench mode (`run_external_ok` erroring) leaves it in place. The doc comment on `rename_elivagar_output` says outputs are "left in place on every failure path", which run mode contradicts.

6. **Two retention policies for the same archive group.** `elivagar/dispatch.rs::prune_output_dir` keeps `OUTPUT_RETENTION = 5` per (dataset, variant) after each tilegen. `main_parts/commands.rs::clean_archives` keeps `--keep` (default 2) for the same group, with its own read_dir + mtime sort + skip loop. Both share only `pmtiles_archive_matches`. So the newest-first ordering, the mtime fallback and the window are decided twice. The `OUTPUT_RETENTION` doc still says `<dataset>-<commit>.pmtiles` (variant missing, stale).

7. **`nidhogg::client::bbox_to_api` never checks that the components are numbers.** It swaps lon/lat by position on raw strings. `shrink_bbox` in the same file does parse them. So `default_api_query` can send malformed JSON to the server. See B1.

8. **`preflight::Check::DiskSpace` is dead and duplicates `disk_gate`** (its own `statvfs`). It is also marked `#[allow(dead_code)]` "for future preflight checks". Delete it, or route it through `disk_gate`.

---

## B. Axes that should be types

### B1. `Bbox` (highest value)
A bbox travels as `String`/`&str` from config (`Dataset.bbox: Option<String>`) through `resolve_bbox` (returns `String` after `validate_bbox`) into `CommandContext.bbox: Option<String>`. It is then re-parsed independently in at least five places:
- `resolve_parts/schema.rs::validate_bbox` (count + f64 check, no finiteness)
- `pbfhogg/commands.rs::multi_extract_plan` (count, f64, finite)
- `pbfhogg/verify_multi_extract.rs::run` (count, f64, no finiteness)
- `nidhogg/client.rs::shrink_bbox` (count, f64)
- `nidhogg/client.rs::bbox_to_api` (count only; lon/lat swap by index)

The axis order (`lon_min,lat_min,lon_max,lat_max`) is restated in comments at each site. A `Bbox { min_lon, min_lat, max_lon, max_lat }` parsed once at config load (and once for `--bbox` via a clap value parser) would make an unvalidated or swapped bbox impossible to build. It would own the methods `strips(n)`, `shrink_quarter()`, `to_nidhogg_api()` and `to_pbfhogg_arg()` (the `-b=<bbox>` spelling, which `commands.rs` and `bench_extract.rs` now keep in step by comment, see C3). Keep the original text for `cli_args` fidelity, or define one canonical render.

### B2. Dataset / variant / OSC seq / snapshot key
`resolve.rs` passes `dataset: &str, variant: &str, seq: &str` side by side in nearly every signature (`resolve_pbf_path(dataset, variant, ..)`, `resolve_osc_path(dataset, seq, ..)`, `pmtiles_archive_name(output_dir, dataset, variant, commit)`: four same-typed strings). Swapping any two compiles. Specifics:
- **OSC seq is numeric but stored and keyed as `String`.** `max_osc_seq` parses keys with `parse::<u64>().ok()` (silently dropping non-numeric keys), `resolve_osc_range` parses `LO..HI` and formats back to strings to look up, and `download_core` builds keys with `seq.to_string()`. An `OscSeq(u64)` map key, parsed at config load, removes the string round trip and makes a non-numeric seq a config error, not a silently ignored one.
- **`--osc-range` is a `String` carried in `CommandParams` and parsed late in `resolve_osc_range`.** It should be a `RangeInclusive<OscSeq>` from a clap value parser.
- **Snapshot key:** `SnapshotRef::Named(String)` is validated at parse, but config's `ds.snapshot` map is keyed by `String`, and `"base"` is spelled in `SnapshotRef::parse`, in `config::validate_snapshot_key` (reserved), and in `pbfhogg/dispatch.rs::resolve_merged_pbf` (the cache-key spelling `Base => "base"`). A `SnapshotKey` newtype, with `SnapshotRef::cache_token()` owning the "base" spelling, makes all three one.
- **PBF variant names `"raw"`/`"indexed"` are magic strings with meaning.** `promote_artifact` routes `degrade --strip-indexdata` to `"raw"`, everything else to `"indexed"`. `download_core::has_existing_pbf`/`raw_pbf_path`/`indexed_pbf_path` prefer `"raw"` and fall back to dated filenames. The snapshot error suggestion falls back to `"raw"`. A `PbfVariant` type with well-known constructors (`RAW`, `INDEXED`) and an open `Other(String)` would give these one home.

### B3. Digests
xxh128 digests are `String`s compared with `eq_ignore_ascii_case` (`preflight::verify_file_hash`). The `FileEntry` trait yields `Option<&str>` for `xxhash`. `tools.rs` has a separate `sha256_file` -> `String`. An `Xxh128([u8;16])` parsed at config load (rejecting a malformed pin at load, not on first use) and a `Sha256` type would make "compared against the wrong algorithm" or "a truncated pin" impossible. The tree digest (`compute_xxh128_tree`) is also a `String`, and nothing at the type level says whether a digest is of a file or a tree.

### B4. Dates
There are three calendar implementations and four string formats:
- `download_core::today` + `days_to_civil` (Hinnant forward)
- `download_modes::civil_to_days`, `unix_to_yyyymmdd`, `iso_date_to_yyyymmdd`, `snapshot_key_to_iso_date`, `iso_date_today`, `iso_date_parse_unix`
- `tools::parse_http_date` (an inline Hinnant inverse, whose comment cites `pbfhogg::download::days_to_civil`)
- `header.rs` uses libc `gmtime_r` for the year.

A `CivilDate { y, m, d }` type with `from_unix`, `to_unix`, `yyyymmdd()`, `iso()`, `parse_yyyymmdd`, `parse_iso` and `parse_http`, in one shared module (it is not pbfhogg-specific), would remove three copies of the same arithmetic. It would also stop snapshot keys, `download_date` and Last-Modified comparisons from each re-deriving the date. The `snapshot_key_to_iso_date` check `(1..=31).contains(&d)` accepts Feb 31.

### B5. `PbfhoggCommand` field shapes
- `Inspect { nodes: bool, tags: bool, .. }`: the doc says the two are mutually exclusive. That should be `InspectMode::{Default{extended}, Nodes, Tags{type_filter}}`. As it stands, `extended` is a documented "no-op in nodes/tags", which means the enum can hold states it then ignores.
- `TagsFilter { input_kind_osc: bool, omit_referenced, invert_match, remove_tags }`: the doc says osc input "rejects -R, -i, -t". `build_body` silently drops them, and clap's `conflicts_with` rejects them, so that is two decisions. Use `TagsFilterInput::{Pbf{omit_referenced, invert, remove_tags}, Osc}`.
- `Diff`/`DiffSnapshots` each carry `DiffFormat`, and their `build_body` arms are identical apart from the source of the two inputs.
- `as_pbfhogg` returns the 5-tuple `(&ModeArgs, &PbfArgs, PbfhoggCommand, Option<&str>, CommandParams)` (`#[allow(clippy::type_complexity)]`). A struct would name the parts. More importantly, `osc_seq` should live inside the command's input spec and not beside it.

### B6. `CommandContext` should be shaped by `InputKind`
`CommandContext` holds `pbf_path`, `osc_path: Option`, `osc_paths: Vec`, `pbf_b_path: Option` and `bbox: Option<String>`. `build_body` unwraps them with `ctx.osc_str()?` / `ctx.pbf_b_str()?` / "extract requires a bbox". So the invariant "the inputs this command needs are present" is checked at argv build time by `Result`, not by construction. `InputKind` already lists the shapes (`Pbf`, `PbfAndOsc`, `PbfAndBbox`, `PbfAndMerged`, `OscOnly`), but it is not the type of anything. Make the resolved inputs an enum mirroring `InputKind` (`ResolvedInputs::PbfAndOsc { pbf, osc }`, `OscRange { oscs }`, `TwoPbfs { a, b, origin: Merged | Snapshot }`, ...), so `build_body` pattern-matches and cannot ask for a missing input.

`InputKind` is also wrong for `DiffSnapshots`: it says `Pbf` and needs two, and dispatch short-circuits it ("handles the snapshot resolution outside the InputKind dispatch"). `needs_osc()` returns true for `PbfAndMerged` because the merge needs one, which mixes "what the command consumes" with "what setup needs". `OscOnly` is used by `merge-changes`, yet `run_wallclock_core` still calls `file_size_mb(&cmd_ctx.pbf_path)` and records the PBF as `input_file` for a command that never reads it. That puts the wrong `input_file`/`input_mb` on merge-changes rows.

### B7. `CommandParams` is a bag of `Option<String>` side-channel state
`merged_cache_state: Option<String>` ("hit"/"miss"), `merged_cache_age_s: Option<String>`, `to_snapshot_file_mb: Option<String>` (a formatted float). Dispatch writes these into a cloned `params` so `PbfhoggCommand::metadata` can read them back. That routes runtime observations through the input-parameter struct. `MergedCacheState` already exists as an enum and is stringified only to fit the bag. Return observations as a typed value from resolution (`ResolvedInputs` carries `MergedCacheState` / `SnapshotSide { file, mb }`), and have `metadata()` take that, not `&CommandContext`.

### B8. Bool-pair and positional-bool APIs
- `elivagar::input_assertions` returns `(bool, bool)` = `(locations_on_ways, force_sorted)`, so the two can be swapped silently. It should return a struct, or the `PbfEntry` itself.
- `BenchContext::new(..., true, &label, true|req.force, stop)` has two positional bools (the second is "dirty tree allowed"). Every dispatcher passes literals, and the source formatting `true,        req.stop_marker...` shows these lines were machine-edited. This belongs to the measurement core (sibling scope), but every map-project call site pays for it. An `enum DirtyPolicy { Allow, Gate { force } }` would make "run mode never stores" structural (ties to A4).
- `resolve_io_flags` returns `(Vec<&str>, Vec<&str>)` = `(features, args)`. That pair is "the I/O mode" and should be `IoMode { direct_io, io_uring }` with `features()` / `args()`.

### B9. Tilegen config values are strings
`TilegenConfig.tile_format`, `tile_compression`, `compress_sort_chunks`, `sort_budget`, `way_budget` and `assemble_budget` are `Option<String>` forwarded verbatim. The budget strings in particular are sizes, so a typo surfaces only when elivagar rejects the argv after a build. This is consistent with "brokkr interprets none of this", but a validated `ByteSize` with `Display` round-trip costs nothing and gives a config-load error. Borderline: a deliberate choice, so challenge it rather than flag it as a defect.

### B10. Typed failures
Almost every failure in scope is `DevError::Config(String)`: unknown dataset, missing variant, missing file, ambiguous default, hash mismatch (`Preflight(vec![String])`). A machine caller (e.g. `--json` or a script driving `brokkr download` then a bench) cannot tell "not downloaded" from "misconfigured" from "corrupt". A `ResolveError::{UnknownDataset, MissingEntry{kind, key, available}, FileMissing{path}, HashMismatch{path, expected, actual, origin}, Ambiguous{kind, available, flag}}` would let `download` offer "missing file -> fetch it", and would make the five hand-built "available: a, b" messages one renderer.

---

## C. Decisions made in more than one place

### C1. "Is this command external / buildable / hotpath-capable?" (elivagar)
`ElivagarCommand` answers this three times: `is_external()`, `build_config() -> BuildKind::NoBuild`, and `supports_hotpath()` (false exactly for the externals). Dispatch then checks `is_external()` before the mode match, and `BuildKind::NoBuild` arms in `run_elivagar_run`/`run_elivagar_bench`/`run_elivagar_hotpath` repeat the "not supported" error, unreachably. In `run_elivagar_external`, `_ => unreachable!()` exists because the type does not encode it. Owner: split the enum, `ElivagarCommand::{Built(BuiltCommand), External(ExternalTool)}`, and `BuildKind` becomes the discriminant of `BuiltCommand`, not a method.

### C2. "What is this command called?" (all three projects)
Each command enum has both `id()` and `result_command()`, and they diverge: elivagar `tilegen`/`self`, `pmtiles-writer`/`pmtiles`; nidhogg `nid-ingest`/`ingest`; pbfhogg's `result_command()` is `id().to_owned()`. `bench_self.rs` and `bench_all` also file rows, so the results DB name is chosen in several places for the same work (a test, `cat_variants_share_result_command`, guards one case pairwise). Owner: one `CommandIdentity { cli: &str, results: &str }` per variant, or rename the results label to match the CLI id with a results-DB migration, which removes the axis entirely. The legacy names exist only because historical rows used them. Normalise them in the DB once and delete `result_command`.

### C3. pbfhogg argv built in more than one place
- `extract`: `PbfhoggCommand::Extract` in `commands.rs` and `bench_extract::strategy_args` both build `extract <pbf> [--simple|--smart] -b=<bbox> -o <out>`, held in step by a comment on each side ("must stay spelled identically"). The strategy-to-flag mapping is also in `MultiExtract`'s arm, and in `verify_multi_extract` (always `--simple`). Owner: `ExtractStrategy::flag() -> Option<&str>` plus `Bbox::to_pbfhogg_arg()`, and `bench_extract` should build `PbfhoggCommand::Extract` values instead of its own argv (the "strategy all" fan-out then lives in one place; today `main.rs` fans out both `extract` and `multi-extract`, and `bench_extract::run` fans out again by string).
- `ensure_merged_pbf` spells `["apply-changes", pbf, osc, "-o", out]` separately from `PbfhoggCommand::ApplyChanges`. That drops `--locations-on-ways`, `-j`, `--compression` and the I/O flags, so the merged B-side for `diff` is built under different settings from an `apply-changes` bench. That may be intended, but it is a second argv for the same subcommand.
- The `--io-uring` support list exists as `supports_io_uring()` and again as the error prose in `resolve_io_flags` ("only apply-changes, sort, cat-dedupe, diff-osc, repack, degrade"). Neither is tied to pbfhogg's clap. Derive the prose from the method, by iterating a variant catalogue.
- Appending the io args plus `--compression` is copy-pasted in four places (`run_pbfhogg_dry_run`, `run_pbfhogg_run`, `run_wallclock_core`, `run_pbfhogg_hotpath`). It belongs in `build_args`, with `IoMode` and compression in the context. Then dry-run would show the real argv by construction. Today dry-run shows bench argv even under `--hotpath`, and the hotpath "legacy" `--compression zlib` default is decided inside `build_body` by peeking at `ctx.params.compression`, because the append happens elsewhere.

### C4. "Does build_args touch the filesystem?"
pbfhogg says no: `build_args` is pure because dry-run validates with it, and `prepare_scratch` materialises separately. Elivagar says yes: `ElivagarCommand::build_args` does `create_dir_all` on scratch and `tilegen_tmp`, and the dispatch doc excuses that for dry-run. Nidhogg has `--dry-run` "not yet supported". This is one policy question (may dry-run write?) with three answers. Owner: a shared `Plan` step (pure argv + list of paths to create) that every project's command type implements, with materialisation done by the shared runner.

### C5. `tilegen_tmp`
The `<data>/tilegen_tmp` path is built independently in `elivagar/commands.rs::build_args`, `elivagar/dispatch.rs::rename_elivagar_output` (the "output dir coincides with tmp" guard), `main_parts/commands.rs` (clean) and documented in `config_parts/schema.rs`. Owner: one `elivagar::paths::tilegen_tmp(data_dir)`, ideally on `ResolvedPaths` for elivagar. The same applies to `bench-self-output.pmtiles`, spelled twice inside `commands.rs` (`output_files` and `build_args`).

### C6. The PMTiles archive store
`resolve.rs` owns `pmtiles_archive_name`, `pmtiles_archive_prefix`, `pmtiles_archive_matches`, `legacy_archive_token` and `resolve_pmtiles_by_commit`. The rename into the store lives in `elivagar/dispatch.rs`, retention in two places (A6), and `clean --archives` in `main_parts`. The comment on `pmtiles_archive_name` says it is "the single place this name is constructed", and that holds for the name, but retention, enumeration (prefix-match + hyphen-free remainder + legacy hex tokens) and the "wiped dir" guard are spread across three modules. Owner: an `elivagar::archive_store::ArchiveStore { dir }` type with `path_for(ArchiveKey{dataset, variant, commit})`, `resolve(dataset, variant, rev)`, `admit(scratch_file, key)`, `prune(group, keep)` and `groups()`. `resolve.rs` is the wrong home: it is the generic dataset-entry resolver.

### C7. Default-entry auto-selection
"Exactly one -> pick it; zero -> error; many -> error naming the flag" is implemented in `resolve_default_entry_path` and again inline in `resolve_single_osc`, which needs the key back and so copies the logic, including its own sorted "available" list and `where_clause` match (built twice in `resolve_osc_range` too). `get_default_osc_entry` is a third version that returns `None` on ambiguity rather than an error. The snapshot-not-found error with its sorted `base`-plus-keys list is built twice (`snapshot_osc_map` and `resolve_snapshot_pbf_path`). Owner: one `select_one(map, key: Option<K>, ctx) -> Result<(K, &E), ResolveError>`.

### C8. Dataset lookup paths
There are two routes to a dataset entry. One is `ResolvedPaths.datasets` (`resolve.rs::get_dataset`, used by every resolver). The other is `elivagar::input_assertions`, which goes back to `dev_config.hosts[config::hostname()].datasets[..].pbf[variant]` and swallows every failure into `(false, false)`, as does `resolve_tilegen` re-resolving the hostname. If `ResolvedPaths` ever differs from the raw host table (include composition, a defaults layer, user layer), the PBF resolved and the PBF whose `locations_on_ways` is asserted can differ silently. Owner: resolution returns the `PbfEntry` alongside its path (`ResolvedPbf { path, entry }`), and `PipelineOpts` reads the assertions from that.

### C9. Bbox parsing and strip splitting: see B1 and A1.

### C10. Curl
`tools::run_curl` (with `CURL_STALL_ARGS`) and `nidhogg::client::run_curl_body` (`-sS --fail-with-body --max-time 30`) are two curl spawners with different error rendering and timeout policy. `health_check` builds a third command line. These differ deliberately in timeouts, but "how a curl failure becomes a `DevError`" is decided twice. Owner: one `http` module with request options. `tools.rs` also owns `head_url` / `HeadResponse` / `download_file`, which are generic and not "tools".

### C11. Two PMTiles readers
`src/pmtiles.rs` hand-parses headers, varints, directories and Hilbert tile IDs (`tile_id_to_zxy`), while `elivagar/eliv.rs` re-exports the linked elivagar crate's `pmtiles_reader` and `tile_id_to_zxy`. `corpus/mutate.rs` has a third `read_varint`. If elivagar is always linked, `pmtiles-stats` and nidhogg's `sample_tile_coords` should read through `eliv`, and `pmtiles.rs` (868 lines) can mostly go. If the elivagar link is meant to be optional, that boundary should be stated and `eliv` should be the only consumer of the crate. Either way, decoding the spec in two places is a drift hazard (compression-code naming, run-length handling, leaf directories).

---

## D. Structure

### D1. Three project dispatchers that should be one runner over a trait
`pbfhogg/dispatch.rs`, `elivagar/dispatch.rs` and `nidhogg/dispatch.rs` each re-implement the same skeleton: project gate, dry-run, then a match over `Run | Bench | Hotpath/Alloc`, then `BenchContext::new` (9 positional args), resolve input, `build_args`, `BenchConfig {...}` (the same 11-field literal about eight times), run, cleanup. The per-command enums each expose the same informal interface (`id`, `result_command`, `supports_hotpath`, `metadata`, `package`, `build_args`), several of whose methods are dead or constant:
- `PbfhoggCommand::supports_hotpath` is always `true`, and its dispatch check is unreachable.
- `ElivagarCommand::package` is always `None`, `metadata` always empty, `needs_data_dir`/`needs_scratch` are `#[allow(dead_code)]` "prepared for future ... unification".
- `NidhoggCommand` has an `#[allow(dead_code)]` impl block: `build_args`, `scratch_output_dir`, `package`, `needs_build` and `needs_server` are unused, while dispatch hard-codes `Some("nidhogg")`, `"bench-ingest-output"` and the `["ingest", pbf, out]` argv. Its `commands.rs` therefore describes the commands, and the code that runs them ignores the description (a second decision for every fact in it).

The real seam is: a command produces a **plan** (build spec: package/bin/example + features; inputs to resolve; argv given resolved inputs; scratch artefacts; ok exit codes; observations -> metadata; post-success action such as archive or snapshot promotion), and a shared **runner** executes that plan under a mode. Write a `MeasuredCommand` trait (or a `CommandPlan` data struct) in the measurement core, and implement it per project. The three dispatchers then collapse into one, and dry-run, the run-mode-records question (A4), the cleanup-on-failure policy (A5) and the io-flag appending (C3) are each decided once. Nidhogg's Api/Tiles and elivagar's externals become plan kinds (`ServerLifecycle`, `ExternalTool`), not escape hatches in the dispatch match. This is the big rewrite worth doing in this scope.

### D2. `resolve.rs` is three unrelated modules glued by `include!`
- `resolve_parts/schema.rs` contains no schema. It is the dataset entry resolver plus the PMTiles archive store (C6), the nidhogg data dir and `file_size_mb`.
- `resolve_parts/runtime.rs` is three DB path functions, two of them piners-only (`corpus_runs_db_path`, `lint_runs_db_path`), with `results_db_path` stranded at the bottom of `schema.rs`.
- The `include!` pattern means these are not modules: no privacy boundary, one namespace.

Proposed split: `datasets::resolve` (entry resolution, `SnapshotRef`, `select_one`, typed `ResolveError`), `elivagar::archive_store`, the DB paths next to their DBs (`db::paths` / `piners::corpus_db`), and `nidhogg_data_dir_path` into `nidhogg`. The same `include!` concern applies to `pbfhogg/download.rs` -> `pbfhogg_mod/*`. `pbfhogg_mod` is not a module, only a directory of fragments, and it holds generic pieces (dates, the snapshot promotion that `dispatch.rs` calls) alongside the download CLI. Make it a real `pbfhogg::download` module with submodules `source` (Geofabrik/planet, aliases, URLs), `layout` (file naming: `raw_pbf_path`, `osc_filename`), `snapshot` (promotion, collision preflight, toml writes) and `modes`.

### D3. `preflight.rs` is two subsystems
The first half is host-capability checks (io_uring sysctls, rlimit, a binary-on-PATH check via spawning `which`). The second half is content integrity: the xxh128 hash cache, tree digests and the racy-window stamp logic. Hash verification is used by every dataset resolver, by dellingr and by mogwai, so it is not "preflight". Split it into `digest` (types from B3, cache, tree fold) and `host_checks` (io_uring set). `check_binary` shelling out to `which` should be a PATH walk.

### D4. `tools.rs` mixes four jobs
External-tool provisioning (JDK, Planetiler, Osmosis, Tilemaker, shortbread), HTTP primitives (`run_curl`, `download_file`, `head_url`, `parse_http_date`), checksum (`sha256_file`, `fetch_verified`) and build-tool probes. Move the HTTP and date pieces to shared modules (C10, B4). The provisioning is project-specific (planetiler/tilemaker are elivagar and pbfhogg baselines) and could live under a `baselines/` module the two projects share.

### D5. Ownership of `CommandContext`, `CommandParams`, `InputKind`, `ArgMode`
`CommandContext`/`CommandParams` live in `crate::measure` (the shared core), yet their fields are pbfhogg-specific (`pbf_b_path`, `osc_paths`, `index_type`, `inject_prepass`, `force_altw`, `merged_cache_state`, `to_snapshot_file`, `as_snapshot`, ...). That dependency edge points the wrong way: the core knows pbfhogg's vocabulary. Move them into `pbfhogg`, and let the core see only the plan/trait from D1.

### D6. `commands.rs` as the "single source of truth" claim
`pbfhogg/commands.rs`'s header says every measurable command "is defined here exactly once". In practice the facts about a command are spread across `commands.rs` (argv), `cli/schema.rs` (allowed values, conflicts), `cli_adapter.rs` (CLI-to-enum mapping, a 400-line identity match), `main.rs` (strategy fan-out), `bench_commands.rs` (suite presets by string name, `preset_to_command`), `bench_extract.rs` (a second extract argv) and `dispatch.rs` (input special cases for `DiffSnapshots`, output special cases for `MultiExtract`, promotion routing for `Degrade`). With clap derive on the command enum itself (the variants are mostly the CLI already), `cli_adapter.rs` disappears and allowed values live on the types. A per-variant descriptor would also hold promotion variant, OutputKind and io_uring support next to argv.

### D7. Test layout
`commands.rs` tests assert argv token by token for each flag combination, which mirrors the implementation. The decisions that drifted (A1, A3, C3: two extract argv builders, bench vs verify multi-extract, output kind vs actual dir) have no test, because each copy is tested on its own. Once D1/C3 land, the valuable test is a property over a catalogue of every variant: `build_args` is pure, every path it names is in the plan's artefact list, cleanup removes exactly those paths, and `supports_io_uring` agrees with the error text.

---

## E. Smaller observations

- `resolve_pmtiles_by_commit`: an unresolvable revision falls back to the typed string as the hash token (`Err(_) => c.to_owned()`), so a typo resolves to a "no build for <typo>" error, not "unknown revision". That is deliberate per its comment, but the error then tells the operator to run tilegen for a commit that does not exist.
- `resolve_pmtiles_by_commit` with `--file` skips hash verification and dataset scoping entirely. That is fine, but it does not record that the input was unpinned.
- `file_size_mb` is decimal MB, "consistent with bench scripts". The unit lives in the function name and an `f64`. `to_snapshot_file_mb` is formatted `{:.0}` into a string, so the results DB gets a rounded text value where `input_mb` gets a float.
- `get_pbf_entry`/`get_default_osc_entry` return `Option` and hide unknown-dataset vs missing-entry. They are used by bench-all to skip silently, which is how a misconfigured dataset becomes a bench-all run with fewer rows and no error.
- `GETID_BENCH_IDS` contains IDs that "may not exist in very small test datasets". Rows from different datasets therefore measure different work under the same `cli_args`. Recording the hit count as a metadata observation would make that visible.
- `TimeFilter` hard-codes `2024-01-01T00:00:00Z`. As planet data ages, the selected fraction changes, so rows are comparable across time only by accident.
- `elivagar::detect_locations_on_ways_stderr` is the "source of truth" for the LoW path but runs only in hotpath mode. Bench rows (FIFO counters only) carry no such observation, so the two modes record different facts for the same run.
- `osc.rs` was not reviewed in depth. It is a self-contained reader with a `String` error type in `parse_osc_text` (`Result<OscDiff, String>`), mapped to `DevError` at one call site. Fine as is, apart from B10.
