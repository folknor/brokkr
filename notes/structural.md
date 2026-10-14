# Structural

Shape and placement, from the design hunt: axes that should be types, and moves,
splits and rewrites, each with the payoff it buys. Siblings: `consolidations.md`
(CON), `bugs.md` (BUG), and the hygiene documents (`hygiene-*.md`), where design
findings about mechanisms those documents already held were added to the
existing entries instead. Entries record what hunters reported; nothing has
been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

## STR-005 - `include!`-spliced fragments stand in for modules

Reported by: config-cli, measurement, map-projects.

`main.rs` `include!`s `main_parts/bootstrap.rs` and `main_parts/commands.rs`, and `cli.rs` `include!`s three files (which is why `visibility.rs` must use `//` comments). `harness.rs` is four `include!`s: `harness_mod/format_sidecar.rs` holds result-line formatting, `run_variants`, `hotpath_feature`, `elapsed_to_ms/us`, the hotpath capture runner and its thread-local parking slot, percentile maths, kv-stderr parsing and sidecar DB backup rotation, while `types_run.rs` holds the harness and all of its loops, and `foreign_loop.rs` is two hooks. `resolve.rs` splices `resolve_parts/schema.rs`, which contains no schema: it is the dataset entry resolver plus the PMTiles archive store, the nidhogg data dir and `file_size_mb`. It also splices `resolve_parts/runtime.rs`, which is three DB path functions, two of them piners-only, with `results_db_path` stranded at the bottom of `schema.rs`. `pbfhogg/download.rs` splices `pbfhogg_mod/*`, which is a directory of fragments rather than a module, holding generic pieces (dates, the snapshot promotion `dispatch.rs` calls) beside the download CLI. None of these has a privacy boundary, `use` hygiene or a module doc; everything shares one namespace. The ratatoskr shims are recorded in the hygiene projects document's ratatoskr module-shape entry.

Proposed splits: real modules for `main_parts` and `cli`. Measurement: `measure/wall.rs`, `measure/loop.rs`, `measure/record.rs` (row building, `prev.*`, result line), `measure/capture.rs` (hotpath child runner), `measure/kv.rs` (stderr counter protocol), `sidecar/backup.rs`, and `bench_variants.rs` (`run_variants` is a dispatch helper, not harness). Resolve: `datasets::resolve` (entry resolution, `SnapshotRef`, `select_one`, typed errors), `elivagar::archive_store`, DB paths next to their DBs, `nidhogg_data_dir_path` into `nidhogg`. Download: a real `pbfhogg::download` with submodules `source` (Geofabrik/planet, aliases, URLs), `layout` (`raw_pbf_path`, `osc_filename`), `snapshot` (promotion, collision preflight, toml writes) and `modes`.

## STR-008 - A lane's execution kind is five loose fields and a priority ladder

Reported by: check-plan.

`harness: Harness`, `doc_only: bool`, `parallel_budget: Option<u32>`, `process_isolation: bool` and `test_threads: Option<u32>` together encode one fact: how the lane executes. `prepare::lane_kind` recovers it with a priority ladder (nextest > doc_only > parallel > isolated > serial). Illegal combinations are refused in four different places, or not at all. `lane_kind` silently picks `Parallel` when both `parallel_budget` and `process_isolation` are set, so correctness depends on `phase.rs::reject_conflicting_lanes` having run first. The two docs already disagree: `profile.rs`'s doc on `parallel_budget` says the exclusion is "enforced at resolve time", while `reject_conflicting_lanes` says it is "refused here rather than at resolve time". `LaneKind` is defined in `accounting.rs`, derived in `prepare.rs`, and consumed by `phase.rs`, `continuation.rs` and the doctest predicate, so the type's dependency points accounting <- prepare and the derivation's points prepare <- accounting.

Proposed: `enum LaneExec { Serial { threads }, Parallel { budget }, Isolated, Nextest { threads }, DocOnly }`, built once at resolution, with the conflicts as construction errors and the value carried on the planned sweep. `LaneKind` becomes a projection of it.

## STR-029 - The guard's admission decision is not testable

Reported by: process-control.

Acquisition now moves through typed `Flocked -> Gated -> Drained -> Authorized` states that consume each other, with effects injectable per transition. Residue: the guard binary's `decide()` (`src/bin/rustc_guard.rs`) still reads the environment and `$HOME` directly, so the security-relevant half of the protocol has no test; it needs injected protocol paths and an env lookup.

## STR-034 - Measurement session residue

Reported by: measurement, map-projects.

`MeasurementSession` replaces the two contexts and the dead `BenchConfig` fallbacks are gone. Residue: the db tests still build full `RunRow` literals by hand (a `RunRow` fixture builder would remove the copies).

## STR-035 - The shared core owns pbfhogg's types, and pbfhogg's command inputs are checked at argv time

Reported by: measurement, map-projects.

`CommandContext` (pbf_path, osc_path, osc_paths, pbf_b_path, bbox) and `CommandParams` (direct_io, io_uring, compression, snapshot, osc_range, inject_prepass, force_altw, index_type, merged-cache observations, to_snapshot_file, as_snapshot, ...) live in `crate::measure`, the shared core. Their fields are pbfhogg's vocabulary, so the edge points the wrong way.

`build_body` unwraps the context with `ctx.osc_str()?`, `ctx.pbf_b_str()?` and "extract requires a bbox", so the invariant "this command's inputs are present" is checked at argv-build time by `Result`, not by construction. `InputKind` already lists the shapes (`Pbf`, `PbfAndOsc`, `PbfAndBbox`, `PbfAndMerged`, `OscOnly`) but is not the type of anything. It is also wrong for `DiffSnapshots`, which it labels `Pbf` though the command needs two PBFs; dispatch short-circuits it. `needs_osc()` returns true for `PbfAndMerged` because the merge needs one, which mixes "what the command consumes" with "what setup needs".

`CommandParams` doubles as a side channel. `merged_cache_state: Option<String>` ("hit"/"miss", though `MergedCacheState` exists as an enum), `merged_cache_age_s: Option<String>` and `to_snapshot_file_mb: Option<String>` (a float formatted early, so the results DB gets a rounded text value where `input_mb` is a float) are written by dispatch into a cloned `params` so that `PbfhoggCommand::metadata` can read them back.

`PbfhoggCommand` field shapes admit states their docs forbid. `Inspect { nodes: bool, tags: bool, .. }` has two flags documented as mutually exclusive, and `extended` is a documented no-op in nodes and tags mode. `TagsFilter { input_kind_osc: bool, omit_referenced, invert_match, remove_tags }`: osc input "rejects -R, -i, -t", which `build_body` enforces by silently dropping them and clap enforces with `conflicts_with`, two decisions. `Diff` and `DiffSnapshots` have identical `build_body` arms apart from where the two inputs come from. `resolve_io_flags` returns `(Vec<&str>, Vec<&str>)` = `(features, args)`.

`commands.rs`'s header says every measurable command "is defined here exactly once". In practice a command's facts are spread across `commands.rs` (argv), `cli/schema.rs` (allowed values, conflicts), `cli_adapter.rs` (a 400-line identity match from CLI to enum), `main.rs` (strategy fan-out), `bench_commands.rs` (suite presets by string name, `preset_to_command`), `bench_extract.rs` (a second extract argv) and `dispatch.rs` (input special cases for `DiffSnapshots`, output special cases for `MultiExtract`, promotion routing for `Degrade`). The `commands.rs` tests assert argv token by token for each flag combination, mirroring the implementation, while the decisions that actually drifted (the two extract argv builders, bench against verify multi-extract, output kind against the actual dir) have no test, because each copy is tested on its own.

Proposed: both types move into `src/pbfhogg/`, and `measure.rs` keeps only `MeasureMode`/`MeasureRequest`. Resolved inputs become an enum mirroring `InputKind` (`ResolvedInputs::PbfAndOsc { pbf, osc }`, `OscRange { oscs }`, `TwoPbfs { a, b, origin: Merged | Snapshot }`, ...), so `build_body` pattern-matches and cannot ask for a missing input. Resolution returns runtime observations as typed values (`MergedCacheState`, `SnapshotSide { file, mb }`), and `metadata()` takes those rather than `&CommandContext`. The command fields become `InspectMode::{Default { extended }, Nodes, Tags { type_filter }}`, `TagsFilterInput::{Pbf { omit_referenced, invert, remove_tags }, Osc}` and `IoMode { direct_io, io_uring }` with `features()`/`args()`. Clap derive goes on the command enum itself (the variants are mostly the CLI already), so `cli_adapter.rs` disappears and allowed values live on the types. A per-variant descriptor holds promotion variant, `OutputKind` and io_uring support next to argv. The valuable test is a property over a catalogue of every variant: `build_args` is pure, every path it names is in the plan's artefact list, cleanup removes exactly those paths, and `supports_io_uring` agrees with the error text.

## STR-037 - Remaining dependency edges pointing up

Reported by: measurement, process-control, deps-misc.

`RunLog` moved to `src/run_log.rs`, the cancellable pipe reader to `src/fs_probe.rs`, and the gate store under ratatoskr (`ratatoskr/gate_store.rs`). Residue: `cargo_env.rs` still calls `check_cmd::current_phase()` (implemented in `check_cmd/watchdog.rs`); `sidecar.rs` still uses the test runner's pipe reader; the `"pbfhogg"` default project is still hidden by name in `db/format/details.rs` and `db/format/single.rs`; `disk_gate`/`env` storage probing should sit below both.

## STR-042 - Stored commit fields are still strings

Reported by: deps-misc.

`Commit` and `ShortCommit` own validation and the one abbreviation rule, and git, history, worktrees, bench, piners and the elivagar archive store use them. Residue: `RunRow`, `StoredRow` and `RunInfo` still hold commits as `String`; typing them as `ShortCommit` validated on read needs a decision on what a malformed legacy commit in an existing row does.

## STR-043 - Dataset status still lives in the env report

Reported by: deps-misc.

Host facts are typed libraries under `src/host/`, consumed by `env`, `disk_gate`, history, `preflight` and the harness. Residue: `DatasetStatus` and the dataset hash-check helpers still live in `env.rs`; they belong next to dataset resolution (a `ResolvedPaths::dataset_status`), with `env` only rendering them.
