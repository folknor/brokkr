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
  tilemaker, all), verify, compare-tiles, download-ocean, hotpath, regress,
  corpus (the `pmtiles-corpus` exec runner), ocean_build (`ocean-build`).

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

## ocean-build

`brokkr ocean-build` (`src/elivagar/ocean_build.rs`) wraps `elivagar
ocean-build` - one shot per shapefile release, building the world-ocean
pmtiles artifact that tilegen later consumes as an ocean input. The invocation
is derived **entirely** from `[<host>.tilegen.default].ocean`, the same block
tilegen reads: the shapefile entries (the zoom-banded `OceanSpec` variants)
become the `--ocean` specs, and the single `.pmtiles` entry (the `Artifact`
variant) becomes the `-o` output path. There are no override flags - to build
a different artifact, edit the block, the same philosophy as tilegen. The
builder and the consumer therefore read the same statement and cannot drift on
spelling, and the artifact key elivagar records is derived from the same
shapefiles every run re-hashes.

The block is partitioned by role: any number of shapefile specs plus exactly
one artifact. brokkr refuses two ways, both the spec's: **no `.pmtiles` entry**
(nowhere to write the output) and **no shapefile entries** (nothing to build
from); more than one artifact is refused too. Shapefile inputs are resolved
against the host data dir and checked for existence (a missing input fails with
a clear brokkr message rather than deep inside elivagar). `--dry-run` prints
the derived invocation and output path and validates the inputs without
building, matching tilegen. The `default` tilegen block is hardcoded and there
is no `--dataset` - the ocean artifact is per-host pipeline config (the world
ocean serves every extract), not a dataset property; a block selector can be
added the day a second tilegen block exists.

Rotating the artifact is an output-changing event: the next `pmtiles-corpus
check` refuses on the artifact key until the corpus is re-blessed. That
refusal is elivagar's job; brokkr just runs the commands.

## Read-only PMTiles inspection: pmtiles-inspect / diag / svg

`brokkr pmtiles-inspect`, `brokkr diag -z Z -x X -y Y`, and
`brokkr svg -z Z -x X -y Y [-W width] [-H height] [-l layers] [-o output]`
wrap elivagar's `inspect`/`diag`/`svg` subcommands (`src/elivagar/inspect.rs`,
`src/elivagar/diag.rs`, `src/elivagar/svg.rs`). `pmtiles-inspect` is named
that way (not `inspect`) because `brokkr inspect` is already pbfhogg's PBF
inspector - the two share one flat clap `Command` enum so names must be
unique.

All three take `--dataset`/`--variant`/`--commit`/`--file`, resolved by
`resolve_pmtiles_by_commit()` in `src/resolve_parts/schema.rs`: `--file`
skips resolution; otherwise the path is
`<output_dir>/<dataset>-<variant>-<commit>.pmtiles` (the durable output store,
default `data/tilegen`, NOT scratch), constructed by the single
`resolve::pmtiles_archive_name()` helper that `rename_elivagar_output()`
(`src/elivagar/dispatch.rs`) also uses after `tilegen`, so the resolver and the
writer can never drift on spelling. The archive content is a function of
`(dataset, variant, commit)`, and the name carries all three - the variant is
load-bearing, because without it `--against-commit H` (or a plain re-open)
resolves to whichever variant happened to be built last at `H`, the
meaning-lives-in-the-filesystem trap the contract-free `regress` cannot catch.
The name is **constructed, never parsed back** (dataset names carry hyphens,
e.g. `north-america`, so splitting is ambiguous but construction is not).
`--variant` defaults to `raw` (matching `tilegen`); `--commit` defaults to
current HEAD; the commit is `git rev-parse --short HEAD` from the *build root*
(the worktree's HEAD under `tilegen --commit <hash>`, else the main tree), so
the name always names the commit whose code produced the tiles. The durable
store survives a routine `brokkr clean`; only the deep clean (`brokkr clean
--worktrees`) reclaims it. These subcommands only read the file - the current
release binary can inspect output built by any commit, so `--commit` picks
which file to open, not which binary to build (no historical worktree rebuild,
unlike `verify --commit`). All three acquire the brokkr lock (non-blocking
`acquire_cmd_lock`, like `regress`) so an inspection can't read an archive a
concurrent `tilegen` run is mid-write - it refuses instead.

`brokkr verify pmtiles --geometry-stats` forwards `--geometry-stats` to
`elivagar verify` (per-zoom ocean ring geometry statistics).

## Output regression: regress (tier-3 attribution)

`brokkr regress` (`src/elivagar/regress.rs`) is a thin passthrough over
`elivagar regress <current> --against <comparand>`. **Both sides are
explicit** and there is no default baseline, ever: the CURRENT archive comes
from `--variant`/`--commit`/`--file`, the COMPARAND from
`--against-variant`/`--against-commit`/`--against`, each resolved through the
same `resolve_pmtiles_by_commit()` used by `pmtiles-inspect`/`diag`/`svg`
(durable output dir `data/tilegen`). The comparand's variant is addressed
**independently** (`--against-variant`, defaulting to `raw` like `--variant`):
a cross-variant diff is a legitimate regress use - it is the attribution
instrument, and adjudicating artifact-active vs computed output or pricing a
config change means diffing two deliberately different contracts. A required
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

## The pmtiles corpus: `brokkr pmtiles-corpus <sub>`

`brokkr pmtiles-corpus` (`src/elivagar/corpus.rs` for the exec runner,
`cmd::corpus` for the dispatch) is a namespace mirroring elivagar's `corpus`
subcommands - the standing baseline mechanism that replaced the blessed
archive. It is named `pmtiles-corpus`, not `corpus`, because `corpus` is
already piners' parity-corpus runner and brokkr's command names share one flat
clap namespace (the same reason `inspect` became `pmtiles-inspect`).

| brokkr | wraps | brokkr resolves |
|---|---|---|
| `pmtiles-corpus check [--dataset D] [--variant V] [--commit H \| --file P] [--corpus DIR]` | `elivagar corpus check <archive> --corpus <dir>` | archive, corpus dir |
| `pmtiles-corpus bless [... ] [--corpus DIR] [--rotate] [--mode M]` | `elivagar corpus bless` | archive, corpus dir |
| `pmtiles-corpus render-manifest [...] [--corpus DIR] [--style P]` | `elivagar corpus render-manifest` | archive, corpus dir |
| `pmtiles-corpus render [...] -z Z -x X -y Y [--layers L] [--style P] [-o OUT]` | `elivagar corpus render` | archive only |
| `pmtiles-corpus rings [...] -o OUT` | `elivagar corpus rings` | archive only |
| `pmtiles-corpus mutate [...] [-o OUT] --op OP [--tile z/x/y]` | `elivagar corpus mutate` | input archive only |

`mutate`'s `-o` is optional: omitted, it writes a calibrand to
`data/corpus-calibrands/<dataset>-<variant>-<op>.pmtiles`, a brokkr-designated
scratch dir a routine `brokkr clean` clears wholesale. An explicit `-o`
elsewhere is the user's file and clean never touches it.

Every subcommand resolves the archive through the SAME
`resolve_pmtiles_by_commit()` as `pmtiles-inspect`/`diag`/`svg`
(`[--dataset D] [--variant V] [--commit H | --file P]`, variant default
`raw`), so default-commit/variant semantics never diverge. The standing gate is
therefore symmetric: `brokkr tilegen --dataset denmark --variant locations`
then `brokkr pmtiles-corpus check --dataset denmark --variant locations`; a
wrong variant fails loudly at resolution (`no locations build for <hash>`)
before the archive even opens. `--corpus` defaults to `corpus/<dataset>` under the **build root**
(where the git-committed corpus lives, alongside the code - NOT the
config/`data/` dir), and is overridable. Every other flag passes through
verbatim; elivagar owns the value sets (`--mode`, `--op`), so brokkr carries
them as strings and never re-validates.

The wrapper is convenience, never safety: the corpus machinery enforces its
own guards (contract refusal, dirty-build refusal, `--rotate` protection), so
brokkr adds no baseline registry, no default comparand, and no filesystem
inference. There is **no clean-tree gate and no tilegen lock** - a check is
read-only on the archive and never touches tilegen scratch, and bless writes
only into the corpus dir (committed with the landing, so a dirty tree is the
normal state). Exit codes pass through unchanged: **0** pass, **1** content
mismatch, **2** refusal (missing baseline, invalid archive, contract
mismatch). The 0/1/2 distinction is load-bearing for the caller, and brokkr
adds no interpretation of the verdict. Each wrapper still takes the
non-blocking brokkr lock and rebuilds the elivagar release binary
(cargo up-to-date check, sub-second) so a stale binary can never silently
serve the standing gate.
