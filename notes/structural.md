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

## STR-037 - Remaining dependency edges pointing up

Reported by: measurement, process-control, deps-misc.

`RunLog` moved to `src/run_log.rs`, the cancellable pipe reader to `src/fs_probe.rs`, and the gate store under ratatoskr (`ratatoskr/gate_store.rs`). Residue: `cargo_env.rs` still calls `check_cmd::current_phase()` (implemented in `check_cmd/watchdog.rs`); `sidecar.rs` still uses the test runner's pipe reader; the `"pbfhogg"` default project is still hidden by name in `db/format/details.rs` and `db/format/single.rs`; `disk_gate`/`env` storage probing should sit below both.
