# elivagar project notes

`project = "elivagar"` in `brokkr.toml`.

## Module layout

- `src/elivagar/commands.rs` - `ElivagarCommand` enum (Tilegen, PmtilesWriter,
  NodeStore, Planetiler, Tilemaker) with `build_args()`, `build_config()`,
  `needs_pbf()`, `output_files()`, `metadata()`.
- `src/elivagar/dispatch.rs` - exposes `run_command()`. Routes through
  run/bench/hotpath/alloc based on command enum + mode. Uses `BenchContext`
  for build+harness.
- `src/elivagar/...` - benchmarks (self, node-store, pmtiles, planetiler,
  tilemaker, all), verify, compare-tiles, download-ocean, hotpath.

## Variant defaults

- `--variant <name>` defaults to `raw` (vs pbfhogg's `indexed`).
- `--tiles <variant>` selects the `pmtiles.<variant>` entry; auto-selects if
  exactly one is configured.

See `docs/brokkr.toml.md` for the dataset structure and shared variant flags.

## download-ocean

Fetches the ocean polygon dataset used by tile generation. Follows a similar
pattern to pbfhogg's `download` but is elivagar-specific.

## Read-only PMTiles inspection: pmtiles-inspect / diag / svg

`brokkr pmtiles-inspect`, `brokkr diag -z Z -x X -y Y`, and
`brokkr svg -z Z -x X -y Y [-W width] [-H height] [-l layers] [-o output]`
wrap elivagar's `inspect`/`diag`/`svg` subcommands (`src/elivagar/inspect.rs`,
`src/elivagar/diag.rs`, `src/elivagar/svg.rs`). `pmtiles-inspect` is named
that way (not `inspect`) because `brokkr inspect` is already pbfhogg's PBF
inspector - the two share one flat clap `Command` enum so names must be
unique.

All three take `--dataset`/`--commit`/`--file`, resolved by
`resolve_pmtiles_by_commit()` in `src/resolve_parts/schema.rs`: `--file`
skips resolution; otherwise the path is
`<scratch_dir>/<dataset>-<commit>.pmtiles`, matching the naming convention
`rename_elivagar_output()` (`src/elivagar/dispatch.rs`) uses after `tilegen`
via `git rev-parse --short HEAD`. `--commit` defaults to current HEAD. These
subcommands only read the file - the current release binary can inspect
output built by any commit, so `--commit` picks which file to open, not
which binary to build (no historical worktree rebuild, unlike `verify
--commit`).

`brokkr verify pmtiles --geometry-stats` forwards `--geometry-stats` to
`elivagar verify` (per-zoom ocean ring geometry statistics).

Oracle (`scripts/validate/earcut-oracle.mjs`, a Node script, not a Rust
subcommand) has no brokkr wrapper yet - deferred, since it needs a
Node-subprocess invocation pattern brokkr doesn't have today.
