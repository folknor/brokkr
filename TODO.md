# TODO

Gaps found by auditing the implementation. Items marked ~~strikethrough~~ are done.

---

## HIGH — Halfway-implemented (same class as the hotpath bug)

### ~~1. `profile.rs` in all 3 projects — no harness, no DB, no lockfile~~ — Done

pbfhogg profile now uses BenchHarness and delegates to hotpath::run() for both timing and alloc passes, storing structured JSON results in DB. elivagar and nidhogg profile now acquire the exclusive lockfile.

### ~~2. `elivagar/bench_node_store.rs` and `bench_pmtiles.rs` — completely bypass harness~~ — Done

Both now use BenchHarness with lockfile, git/env context, and SQLite storage. Added `example` field to BuildConfig for `--example` support.

### 3. `preflight.rs` — entire check system is dead code

`run_preflight()` and the `Check` enum (`Binary`, `File`, `DiskSpace`, `KernelParam`) were designed as a pre-benchmark validation framework but **never called from anywhere**. The module is `#[allow(dead_code)]` in main.rs (line 15). Only `verify_file_hash()` and `cached_sha256()` are actually used.

Meanwhile, ad-hoc preflight checks are scattered:
- `pbfhogg/bench_merge.rs`: `check_uring_preflight()` (manual RLIMIT_MEMLOCK check)
- `elivagar/profile.rs`: `check_perf_paranoid()`, `check_tool_installed()`
- `nidhogg/profile.rs`: identical copies of the above

All of these should use the `Check` system in `preflight.rs`, but they don't.

### ~~4. `bench all` missing benchmarks~~ — Done

pbfhogg bench_all now runs extract (if bbox configured), allocator, and blob-filter (if pbf_raw configured) alongside the original 7 benchmarks. elivagar bench_all now includes node-store and pmtiles micro-benchmarks. Nidhogg still has no bench all (only 2 benchmarks: api and ingest).

---

## MEDIUM — Inconsistencies and partial implementations

### 5. Smart elivagar `dev run`

`cmd_run()` is a project-agnostic build-and-exec passthrough. When project is elivagar, it should do what `bench_self.rs` and `hotpath.rs` already do for their subcommands:

**Ocean shapefile detection** (logic already exists in `bench_self.rs`):
- Full resolution: `{data_dir}/water-polygons-split-3857/water_polygons.shp`
- Simplified: `{data_dir}/simplified-water-polygons-split-3857/simplified_water_polygons.shp`
- If found and `--no-ocean` not passed, add `--ocean {path}` and `--ocean-simplified {path}` to args.

**`--tmp-dir` injection**: Auto-set to `{scratch_dir}/tilegen_tmp` from config so temp files go to the right drive. Currently `bench_self.rs` uses `{data_dir}/tilegen_tmp` — should use scratch instead.

**`HOTPATH_METRICS_SERVER_OFF=true`**: Already set in `hotpath.rs` via `run_captured_with_env()`. Needs to be set for `dev run` too.

**`--mem` cgroup wrapping**: Wrap the subprocess with `systemd-run --scope -p MemoryMax={value}` to prevent OOM on planet-scale runs. New flag, not in any existing module.

**Elivagar-specific passthrough flags**: `--skip-to`, `--no-ocean`, `--compression-level` should be recognized by `dev run` (not just `bench self`). These are elivagar binary flags that `dev run` forwards after injecting the auto-detected ones.

The Run command in main.rs needs elivagar-specific args added to its CLI definition (currently just `args: Vec<String>`), and `cmd_run()` needs an elivagar branch that loads config, detects ocean, injects flags, optionally wraps with systemd-run.

### 6. `bench tilemaker` stub

`src/elivagar/bench_tilemaker.rs` is 18 lines that immediately return an error. The CLI defines `dataset`, `pbf`, `runs` parameters that are all bound to `_` and silently ignored. A user running `brokkr bench tilemaker --dataset japan --runs 10` gets an error with no indication the params were ignored.

Requires new infrastructure in `tools.rs` (tilemaker build, shortbread config, EPSG:4326 ocean shapefiles, ogr2ogr reprojection).

### ~~7. `elivagar/download_ocean.rs` — only downloads full-res ocean~~ — Done

`download-ocean` now downloads both full-resolution (~765 MB) and simplified (~13 MB) ocean shapefiles. Idempotent per variant.

### ~~8. `nidhogg/bench_api.rs` — BenchConfig missing `input_file` and `input_mb`~~ — Done

Added `--dataset` flag to `bench api` CLI (defaults to "denmark"). PBF filename and size are now resolved from dataset config and recorded in BenchConfig.

### ~~9. `nidhogg/hotpath.rs` — unused `_data_dir` parameter~~ — Done

Removed the unused `_data_dir` parameter and its resolution in `main.rs`. Nidhogg hotpath doesn't need data_dir (unlike elivagar which uses it for ocean shapefiles).

### ~~10. `config.rs` — `Dataset.ocean_shp` field defined but never read~~ — Done

Removed the dead `ocean_shp` field from `Dataset`. Ocean shapefiles are shared across datasets and detected by directory scanning in `detect_ocean()`, not per-dataset config.

### ~~11. Inconsistent `cargo_features` recording in BenchConfig~~ — Done

All pbfhogg benchmarks built via `BuildConfig::release(Some("pbfhogg-cli"))` now record `cargo_features: Some("zlib-ng")`. Fixed bench_commands, bench_extract, bench_blob_filter.

### ~~12. `pbfhogg/verify_check_refs.rs` and `verify_diff.rs` — never assert PASS/FAIL~~ — Done

All three modules now compare outputs and return `Err` on mismatch: verify_check_refs compares pbfhogg vs osmium text, verify_diff compares line counts, verify_derive_changes fails on roundtrip differences.

### 13. DB stores fields never displayed to the user

The harness stores `kernel`, `cpu_governor`, `avail_memory_mb`, `storage_notes`, `cargo_features`, `cargo_profile`, `hostname`, `subject` — but `brokkr results` only shows `uuid`, `timestamp`, `commit`, `command`, `variant`, `elapsed`, `input`. The 8 hidden fields are only accessible by manually querying SQLite.

### ~~14. Nidhogg/elivagar dataset rename: remove `denmark-latest` fallbacks~~ — Done

Removed hardcoded `"denmark-latest"` fallback in `main.rs` nidhogg profile. Falls back to `data_dir` instead of a stale dataset-specific path. Disk rename is a manual step outside brokkr.

---

## LOW — Code quality, duplication, stale annotations

### ~~15. Stale `#[allow(dead_code)]` annotations~~ — Done

Removed stale annotations from `harness::run_distribution`, `harness::percentile`, `output::CapturedOutput::elapsed`. Blanket `#[allow(dead_code)]` on `Dataset`/`HostConfig`/`StoredRow` still masks some dead fields.

### ~~16. Duplicated code across projects~~ — Done

- `elapsed_to_ms` → pub in `harness.rs`, 5 copies deleted
- `check_perf_paranoid` + `check_tool_installed` → moved to `preflight.rs`
- `url_encode` → moved to `nidhogg/mod.rs`
- `which_exists` → `bench_all.rs` imports from `verify.rs`
- `parse_compressions` → shared in `pbfhogg/mod.rs` with `add_default_levels` parameter

### ~~17. `pbfhogg/hotpath.rs` — two report extraction methods~~ — Done

Removed dead `run_hotpath_command()` and `extract_hotpath_block()`. Profile now uses the same JSON approach as hotpath.

### 18. Minor inconsistencies

- `nidhogg/profile.rs` takes `data_dir: &str`, elivagar takes `data_dir: &Path` — inconsistent parameter types
- `bench_planetiler.rs` sets `cargo_profile: "release"` in BenchConfig for a Java benchmark — meaningless metadata
- `bench_planetiler.rs` hardcodes `runs: 1` in BenchConfig despite accepting a `runs` parameter (Java handles its own repetition internally)
- `bench_node_store.rs` hardcodes `--features hotpath` in its cargo build but `bench_pmtiles.rs` does not

---

## Backlog

### `pmtiles-stats`

Rust rewrite of elivagar's `scripts/pmtiles-stats.py` (181 lines). New subcommand `brokkr pmtiles-stats <file>`.

**PMTiles v3 format parsing:**
- 127-byte fixed header: magic bytes, version, root directory offset/length, metadata offset/length, leaf directories offset/length, tile data offset/length, addressed/tile/entry counts, tile type, tile compression.
- Directory entries: varint-encoded (tile_id delta, run_length, length, offset delta). Varints are LEB128-style unsigned integers.
- 4 compression formats for tile data: none, gzip, brotli, zstd. The stats tool reads the header to report compression type, doesn't need to decompress.

**Output**: tile count, total size, min/max/avg tile size, zoom level distribution, compression type, metadata summary. Match the Python tool's output format.

**Dependencies**: None beyond std. PMTiles header is a fixed struct, varints are trivial to decode, and we're just computing stats not decompressing tiles.

---

## Done

- ~~Dataset SHA256 verification~~ — `sha256_pbf` and `sha256_osc` fields in config, verification in preflight.
- ~~Server readiness polls log file, not HTTP~~ — `serve()` now polls HTTP health endpoint via `status()`.
- ~~`dev stop` doesn't SIGKILL on timeout~~ — SIGTERM → poll 5s → SIGKILL escalation.
- ~~Lockfile doesn't report the holder's command~~ — reads `/proc/{pid}/cmdline` on conflict.
- ~~Hotpath data not stored in DB~~ — All 3 projects now capture JSON via `HOTPATH_OUTPUT_FORMAT=json` and store in `extra` column.
- ~~Hotpath results not displayed~~ — `brokkr results <uuid>` pretty-prints hotpath tables from stored JSON.
