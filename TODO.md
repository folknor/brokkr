# TODO

Gaps found by auditing the implementation against CLI.md problem statements.

Core infrastructure is complete: lockfile, harness, SQLite, preflight, env, build, output, project gating. pbfhogg subcommands are complete. nidhogg subcommands are complete. Gaps are concentrated in elivagar.

---

## 1. Smart elivagar `dev run`

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

---

## 2. `bench tilemaker`

`src/elivagar/bench_tilemaker.rs` is a stub that returns an error. Requires new infrastructure in `tools.rs` plus the benchmark itself.

**tools.rs additions:**
- `ensure_tilemaker(data_dir)` — git clone tilemaker source, cmake build, cache in `{data_dir}/tilemaker/`, version file. Preflight: check cmake, make, g++ are available.
- Shortbread-tilemaker config download — the lua/json config files tilemaker needs for Shortbread schema tilegen.
- EPSG:4326 ocean shapefiles — separate from elivagar's 3857 shapefiles. Download from osmdata.openstreetmap.de, extract.
- `ogr2ogr` reprojection — the 4326 shapefiles may need reprojection. Preflight: check ogr2ogr binary.

**bench_tilemaker.rs implementation:**
- Build tilemaker (via tools.rs), resolve config + ocean shapefiles, run tilemaker tilegen against the dataset PBF, time with harness `run_external()`, emit `[result]` lines.
- Should mirror `bench_planetiler.rs` structure — external baseline comparison benchmark.

---

## 3. Dataset SHA256 verification

**config.rs**: Add `sha256_pbf: Option<String>` and `sha256_osc: Option<String>` to the `Dataset` struct.

**dev.toml**: Add hash fields to dataset entries:
```toml
[datasets.denmark]
pbf = "denmark-latest.osm.pbf"
sha256_pbf = "a1b2c3..."
```

**preflight.rs**: Add a `Check::Sha256 { path, expected, description }` variant. When a benchmark subcommand resolves its dataset, if the dataset has a hash field, add a SHA256 check to preflight. Compute hash by reading the file in chunks (these are multi-GB files — can't read into memory). Use `std::io::Read` + a rolling hash. No new dependencies needed if we implement SHA256 manually, or add `sha2` crate.

Hashes are optional — datasets without them skip the check. This keeps dev.toml backwards-compatible and lets you add hashes incrementally.

---

## 4. `pmtiles-stats`

Rust rewrite of elivagar's `scripts/pmtiles-stats.py` (181 lines). New subcommand `dev pmtiles-stats <file>`.

**PMTiles v3 format parsing:**
- 127-byte fixed header: magic bytes, version, root directory offset/length, metadata offset/length, leaf directories offset/length, tile data offset/length, addressed/tile/entry counts, tile type, tile compression.
- Directory entries: varint-encoded (tile_id delta, run_length, length, offset delta). Varints are LEB128-style unsigned integers.
- 4 compression formats for tile data: none, gzip, brotli, zstd. The stats tool reads the header to report compression type, doesn't need to decompress.

**Output**: tile count, total size, min/max/avg tile size, zoom level distribution, compression type, metadata summary. Match the Python tool's output format.

**Dependencies**: None beyond std. PMTiles header is a fixed struct, varints are trivial to decode, and we're just computing stats not decompressing tiles.

---

## Done

- ~~Server readiness polls log file, not HTTP~~ — Fixed: `serve()` now polls HTTP health endpoint via `status()`.
- ~~`dev stop` doesn't SIGKILL on timeout~~ — Fixed: SIGTERM → poll 5s → SIGKILL escalation.
- ~~Lockfile doesn't report the holder's command~~ — Fixed: reads `/proc/{pid}/cmdline` on conflict.
