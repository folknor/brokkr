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

## tilegen: the contract lives in brokkr.toml

`brokkr tilegen`'s CLI surface is the *input* axis only - `--dataset`,
`--variant`, the measurement mode, and `--skip-to` (a per-invocation resume
point, not part of the contract). Everything that configures the pipeline -
ocean inputs, tile format/compression, memory budgets, geometry, threads -
comes from `[<host>.tilegen.default]` in brokkr.toml. There are no override
flags: either it is explicit in the block, or it is not set. See
`docs/brokkr.toml.md` for the full key list and the ocean partition rules.

`resolve_tilegen()` / `input_assertions()` (`src/elivagar/mod.rs`) resolve the
block and the per-variant input assertions off `DevConfig`, mirroring
`config::host_features` in resolving the hostname themselves.
`PipelineOpts::push_args()` expands the block into argv; the maps are
`BTreeMap`, so identical config yields byte-identical `cli_args`.

This replaced `detect_ocean()`/`push_ocean_args()`, which stat'd `data/` for
the two shapefiles and passed whichever existed (and never passed the
`.pmtiles` artifact at all - elivagar auto-detected that itself). A run's
meaning therefore lived in the filesystem rather than the invocation: two runs
of the same binary on the same PBF produced different ocean geometry with
nothing in `cli_args` saying which, so no bench row could be classified after
the fact as artifact-active or computed. On 2026-07-14 a denmark archive was
built, verified and blessed as the regress baseline while
`data/ocean-tiles.pmtiles` was missing; it took the computed path throughout
and every gate passed. `bench all`'s self arm shared the same path and the
same defect.

An A/B arm is a sibling block, not a flag - drop the `.pmtiles` line and
`brokkr results --grep-v ocean-tiles` selects the computed arm off `cli_args`,
which is the "arm defined by an absent flag" case `--grep-v` exists for.

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
`<output_dir>/<dataset>-<commit>.pmtiles` (the durable output store, default
`data/tilegen`, NOT scratch), matching the naming convention
`rename_elivagar_output()` (`src/elivagar/dispatch.rs`) uses after `tilegen`:
`git rev-parse --short HEAD` collected from the *build root* (the worktree's
HEAD under `tilegen --commit <hash>`, else the main tree), so the archive name
always names the commit whose code produced the tiles. `--commit` defaults to
current HEAD. The durable store survives a routine `brokkr clean`; only the
deep clean (`brokkr clean --worktrees`) reclaims it. These
subcommands only read the file - the current release binary can inspect
output built by any commit, so `--commit` picks which file to open, not
which binary to build (no historical worktree rebuild, unlike `verify
--commit`). All three acquire the brokkr lock (non-blocking
`acquire_cmd_lock`, like `regress`/`bless`) so an inspection can't read an
archive a concurrent `tilegen` run is mid-write - it refuses instead.

`brokkr verify pmtiles --geometry-stats` forwards `--geometry-stats` to
`elivagar verify` (per-zoom ocean ring geometry statistics).

## Output regression: regress (tier-3 attribution)

`brokkr regress` (`src/elivagar/regress.rs`) is a thin passthrough over
`elivagar regress <current> --against <comparand>`. **Both sides are
explicit** and there is no default baseline, ever: the CURRENT archive comes
from `--commit`/`--file`, the COMPARAND from `--against-commit`/`--against`,
each resolved through the same `resolve_pmtiles_by_commit()` used by
`pmtiles-inspect`/`diag`/`svg` (durable output dir `data/tilegen`). A required
clap `ArgGroup` over the two `--against*` flags means a missing comparand is a
usage error at clap's exit **2** - never colliding with regress's own verdict
codes. Flags `--tol`/`--max-moved`/`--max-examples`/`--overlay`/`--overlay-max`/
`--json` pass straight through. The wrapper streams the report live and
propagates elivagar's exit code verbatim (0 = no accountable diff, 1 =
regression / budget overrun). Like the inspection subcommands, it takes the
non-blocking brokkr lock first.

**There is deliberately no comparability gate and no baseline registry.**
`regress` is the attribution instrument, and reads no provenance contract by
design: its legitimate uses include cross-contract diffs (adjudicating
artifact-active vs computed output, pricing an intended config change), which
a brokkr-side refusal would block, pushing people back to the raw binary.
Comparability is the caller's responsibility - the help text points at `brokkr
pmtiles-inspect` for reading the provenance blocks and warns that cross-variant
comparisons report six-figure diffs on two correct builds. This replaced the
old `src/elivagar/provenance.rs` comparability gate (and `brokkr bless` / the
`[<host>.datasets.<D>.blessed]` config entry), removed on 2026-07-24 when
elivagar retired the blessed-pmtiles-archive machinery in favour of a
git-committed output corpus. The corpus is the only baseline mechanism now;
see the pmtiles-corpus section below.

Oracle (`scripts/validate/earcut-oracle.mjs`, a Node script, not a Rust
subcommand) has no brokkr wrapper yet - deferred, since it needs a
Node-subprocess invocation pattern brokkr doesn't have today.
