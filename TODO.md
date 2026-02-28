# TODO

## Not addressed

### Inconsistent path-to-string conversion
Codebase-wide — `.display().to_string()` (lossy, replaces invalid UTF-8 with replacement char) mixed with `.to_str().ok_or_else(...)` (strict, returns error). Even within single functions. For subprocess args, `.to_str()` is safer (clear error vs silent corruption).

### Inconsistent `run_*` argument types
`src/output.rs` — `run_captured` takes `args: &[&str]` and `program: &str`; `run_passthrough` takes `args: &[String]` and `program: &Path`. Forces unnecessary conversions at call sites.

### Hand-rolled UUID via `/dev/urandom`
`src/db.rs` — reads 16 bytes, manually sets version/variant bits. The `uuid` crate does this correctly with less code. Not a dependency currently.

### `#[allow(clippy::too_many_arguments)]` proliferation
Several pbfhogg bench/hotpath modules suppress this lint. BenchContext covers some cases but not all.


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
