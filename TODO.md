# TODO

## Won't fix

### Inconsistent path-to-string conversion
All paths are constructed from known UTF-8 components, so `.display().to_string()` won't corrupt in practice. 100+ occurrences across 30+ files — not worth the churn.

### Hand-rolled UUID via `/dev/urandom`
10 correct lines in `src/db.rs`. Not worth adding the `uuid` crate as a dependency.

### `#[allow(clippy::too_many_arguments)]` proliferation
Functions genuinely need many parameters. `BenchContext` covers the common case; remaining allows are the pragmatic choice.

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

### Hotpath JSON: emit raw numeric values

`parse_metric()` in `hotpath_fmt.rs` reverse-engineers formatted strings like `"59.2 MB"` and `"3.06 ms"` back into numbers to compute change %. Fragile — silently breaks if the hotpath crate changes formatting (new units, precision changes). The hotpath crate should emit raw numeric values alongside formatted strings in its JSON output so brokkr doesn't need to parse display text.

### Tests for comparison and metric parsing

The comparison pairing (`build_comparison_pairs`), metric parsing (`parse_metric`), and hotpath diff (`format_section_diff`) have no test coverage. `parse_metric` especially benefits from a table of test cases (various units, edge cases, unknown units).

### RTK double execution

Commands appear to run twice (two "Finished... Running..." blocks in output). The rtk PreToolUse hook may be executing the command in addition to the original — investigate hook configuration.

### `resolve_pbf_with_size` helper

`resolve_pbf_path()` and `file_size_mb()` are always called together (19 call sites in `main.rs`). Merge into a single `resolve_pbf_with_size()` returning `(PathBuf, f64)`.

### `HarnessContext` for no-build commands

7 handlers in `main.rs` manually expand `bootstrap + bootstrap_config + BenchHarness::new` because they don't need a cargo build (allocator, planetiler, bench-all). `BenchContext::new()` always builds. Add a lighter `HarnessContext` (or make the build step optional in `BenchContext`).

### Shared `alloc` mode constants

The `alloc` bool → feature/variant_suffix/label tri-state is repeated in all 3 `hotpath.rs` modules + `main.rs`. Extract to a small helper or constants.

### Shared dataset `data_dir` lookup

The 6-line `data_dir` resolution pattern (get dataset → get `data_dir` field → join with `paths.data_dir`) appears in 3 nidhogg commands (`cmd_serve`, `cmd_ingest`, `cmd_verify_readonly`). Extract to a helper.

### `Worktree` has no `Drop` impl

If the process panics or is killed (SIGKILL/SIGTERM) inside a `--commit` benchmark, the worktree at `.brokkr/worktree/<hash>` is left behind. Mitigated: `Worktree::create` cleans up stale worktrees at the same path before creating a new one. A `Drop` impl would require interior mutability or an `Option` wrapper — probably not worth the complexity.

### Nidhogg `.gitignore` ignores `results.db`

Nidhogg uses `.brokkr/` (ignores everything) instead of `.brokkr/*` + `!.brokkr/results.db` like pbfhogg and elivagar. This means `results.db` is not tracked in git for nidhogg.
