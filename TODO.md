# TODO

## Not addressed

### Inconsistent `check_clean` misses untracked files
`src/git.rs:61-84` — uses `git diff --quiet HEAD` for unstaged and `git diff --quiet --cached HEAD` for staged, but neither detects untracked files. A new file in the tree would not flag the tree as dirty.

### Inconsistent path-to-string conversion
Codebase-wide — `.display().to_string()` (lossy, replaces invalid UTF-8 with replacement char) mixed with `.to_str().ok_or_else(...)` (strict, returns error). Even within single functions. For subprocess args, `.to_str()` is safer (clear error vs silent corruption).

### Inconsistent `run_*` argument types
`src/output.rs` — `run_captured` takes `args: &[&str]` and `program: &str`; `run_passthrough` takes `args: &[String]` and `program: &Path`. Forces unnecessary conversions at call sites.

### Duplicated `basename` + `pbf_str` extraction
The pattern `pbf_path.file_name().and_then(|n| n.to_str()).unwrap_or_default()` + `pbf_path.to_str().ok_or_else(...)` appears in 8+ bench modules. A `fn pbf_strs(path: &Path) -> Result<(String, &str), DevError>` would eliminate this.

### `bench_node_store.rs` and `bench_pmtiles.rs` are near-identical
`src/elivagar/` — ~60 lines of duplicated logic (build example binary, run with env, check exit, parse elapsed, build result). Only differences: example name, CLI flag names, extra JSON keys. Extract a shared `run_example_bench` helper.

### Duplicate `path_to_cstring` helpers
`src/lockfile.rs` and `src/preflight.rs` — both have their own `path_to_cstring` with different signatures and error handling. Unify or move to a shared utility.

### `parse_compressions` returns redundant tuple
`src/pbfhogg/mod.rs` — `(label, cli_arg)` always contains the same value for both fields. A single `Vec<String>` would suffice.

### Hand-rolled UUID via `/dev/urandom`
`src/db.rs` — reads 16 bytes, manually sets version/variant bits. The `uuid` crate does this correctly with less code. Not a dependency currently.

### `#[allow(clippy::too_many_arguments)]` proliferation
Several pbfhogg bench/hotpath modules suppress this lint. BenchContext covers some cases but not all.

### `RunRow` vs `StoredRow` nullable field mismatch
`src/db.rs` — `RunRow` uses `Option<String>` for `variant`, `input_file`, `cargo_features`, etc. but `StoredRow` uses plain `String` via `unwrap_or_default()` on read-back. Loses the ability to distinguish "no variant" from "empty string variant". Could cause subtle filtering bugs.

### `run_distribution` percentile truncates instead of interpolating
`src/harness.rs` — percentile function uses integer division `(pct * (len - 1)) / 100`. With small sample counts, p95 on 10 samples maps to p88.9 by nearest-rank. Systematically underestimates high percentiles.

### `pbfhogg/download.rs` doesn't validate partial downloads
Skips files that `exists()` but doesn't check completeness. A partially downloaded file from an interrupted run passes the check and causes silent failures downstream. A size check or checksum would help.

### Elivagar hotpath/profile missing `--no-ocean` flag
`bench_self` accepts `no_ocean: bool` but `hotpath.rs` and `profile.rs` always add ocean flags with no opt-out. Inconsistent with bench_self.

---

## Backlog

### `bench tilemaker` stub

`src/elivagar/bench_tilemaker.rs` is 18 lines that immediately return an error.

Requires new infrastructure in `tools.rs` (tilemaker build, shortbread config, EPSG:4326 ocean shapefiles, ogr2ogr reprojection).

### `pmtiles-stats`

Rust rewrite of elivagar's `scripts/pmtiles-stats.py` (181 lines). New subcommand `brokkr pmtiles-stats <file>`.

**PMTiles v3 format parsing:**
- 127-byte fixed header: magic bytes, version, root directory offset/length, metadata offset/length, leaf directories offset/length, tile data offset/length, addressed/tile/entry counts, tile type, tile compression.
- Directory entries: varint-encoded (tile_id delta, run_length, length, offset delta). Varints are LEB128-style unsigned integers.
- 4 compression formats for tile data: none, gzip, brotli, zstd. The stats tool reads the header to report compression type, doesn't need to decompress.

**Output**: tile count, total size, min/max/avg tile size, zoom level distribution, compression type, metadata summary. Match the Python tool's output format.

**Dependencies**: None beyond std. PMTiles header is a fixed struct, varints are trivial to decode, and we're just computing stats not decompressing tiles.
