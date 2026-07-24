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
  tilemaker, all), verify, download-ocean, hotpath, ocean_build
  (`ocean-build`).
- `src/elivagar/eliv.rs` - the linked-crate seam. The one place that names the
  elivagar API the adjudication code depends on (reader, `tile_detail` decoder,
  writer, PMTiles addressing), so `corpus/` and `regress/` import from here
  rather than reaching into `elivagar::` directly.
- `src/elivagar/corpus/` - the native corpus gate (`pmtiles-corpus`). Owns the
  frozen canonical tile hash, the digest fold, the contract gating policy, the
  SVG render core, and every verdict.
- `src/elivagar/regress/` - the native two-archive semantic diff. See the
  regress section below.
- `src/elivagar/compare_tiles.rs` - the lenient per-layer sampling census, also
  native.

## Variant defaults

- `--variant <name>` defaults to `raw` (vs pbfhogg's `indexed`).
- The shared `--tiles <variant>` flag (the `pmtiles.<variant>` config entry)
  has **no elivagar consumer**. Elivagar produces archives rather than reading
  configured ones, so every archive-consuming command addresses the durable
  output store through `--dataset`/`--variant`/`--commit`/`--file` instead -
  see the resolver section below. `--tiles` survives on nidhogg's `serve`.

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

## Read-only PMTiles inspection: pmtiles-inspect / diag / svg / verify pmtiles

`brokkr pmtiles-inspect`, `brokkr diag -z Z -x X -y Y`,
`brokkr svg -z Z -x X -y Y [-W width] [-H height] [-l layers] [-o output]`, and
`brokkr verify pmtiles` wrap elivagar's `inspect`/`diag`/`svg`/`verify`
subcommands (`src/elivagar/inspect.rs`, `src/elivagar/diag.rs`,
`src/elivagar/svg.rs`, `src/elivagar/verify.rs`). `pmtiles-inspect` is named
that way (not `inspect`) because `brokkr inspect` is already pbfhogg's PBF
inspector - the two share one flat clap `Command` enum so names must be
unique.

All four take `--dataset`/`--variant`/`--commit`/`--file`, resolved by
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
unlike `brokkr verify --commit` on the pbfhogg cross-validators). All four
acquire the brokkr lock (non-blocking `acquire_cmd_lock`, like `regress`) so an
inspection can't read an archive a concurrent `tilegen` run is mid-write - it
refuses instead.

### verify pmtiles

```
brokkr verify pmtiles [--dataset D --variant V --commit H | --file P]
                      [--geometry-stats] [--unique-payloads]
```

Forwards `--geometry-stats` (per-zoom ocean ring geometry statistics) and
`--unique-payloads` to `elivagar verify`. `--unique-payloads` validates each
distinct compressed payload once while keeping addressed-tile accounting: it is
what makes a large run-heavy archive verifiable at all - the world artifact
addresses ~212M tiles over ~9.18M distinct payloads, so without it the check is
~23x the work.

`verify pmtiles` joined this family late (2026-07-24). It used to address a
different namespace entirely: `--dataset` plus `--tiles <variant>`, resolved
against the `pmtiles.<variant>` *config* table. Nothing populated that table on
any host, so the command was dead surface - `brokkr verify pmtiles --dataset
denmark` failed with `dataset 'denmark' has no pmtiles configured`, and there
was no `--file` escape hatch to reach an archive that isn't dataset output
(the world artifact `data/ocean-tiles.pmtiles` is exactly that case). It also
sat under the `Command::Verify` worktree wrapper, where `--commit` meant
"rebuild that binary" rather than "open that archive". `run()` in
`src/main_parts/bootstrap.rs` now dispatches this one variant *before*
`with_worktree` and rejects the outer `brokkr verify --commit` with a message
pointing at the subcommand flag; the rest of `VerifyCommand` (pbfhogg's
cross-validators, which genuinely do compare a historical binary against a
reference tool) still runs inside the worktree.

## Output regression: regress (tier-3 attribution)

`brokkr regress` (`src/elivagar/regress/`) is **native brokkr code** over the
linked elivagar crate - there is no `elivagar regress` subcommand to shell to
anymore. **Both sides are
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
codes. Exit 0 is no accountable diff, exit 1 a regression or budget overrun, and
exit **3** means the run could not be completed at all (an unreadable archive, a
tile that will not decode, an overlay that will not write). 3 is separate from 1
on purpose: exit 1 is the *verdict*, so a caller that could not tell it from an
operational failure would read a truncated archive as a regression, with nothing
but stderr to say otherwise.
Like the inspection subcommands, it takes the non-blocking brokkr lock first -
the diff itself needs no lock, but the archive resolver still bootstraps through
`cargo metadata`.

### The engine

Three passes over **blob pairs**, not tiles. PMTiles addresses tiles through
run-length directory entries, so one stored blob commonly serves thousands of
addressed tiles - a denmark archive addresses ~1.3M tiles from ~166k unique
blobs. `merge_runs` (`engine.rs`) cuts the two directories into spans constant on
both sides and clipped at zoom boundaries; spans sharing a `(current, baseline)`
blob pair collapse into one work item, and each verdict is multiplied back out by
tile count at report time. Each pass only sees what the last could not settle:

1. **raw** - byte equality of the two stored blobs.
2. **canonical** - `corpus::canonical::semantic_hash` per blob (memoized per
   blob, not per pair), which absorbs the intra-layer feature reordering the
   pipeline deliberately leaves unconstrained across archives. This is the *same*
   frozen hash the corpus gate uses, so the two can never disagree about "the
   same tile".
3. **detail** - full structural decode via elivagar's `tile_detail`, re-augmented
   by `prepared.rs` with the bboxes, digests and structure signatures the matcher
   needs, then classified.

Classification (`compare.rs`) writes to a `DiffSink` rather than a concrete
result, because the overlay renderer needs the same walk with different retention
(it keeps the features so it can draw them). Sharing the walk is what guarantees
an overlay shows the diff the report counted.

Features with an OSM id match by id; anonymous features bucket by attribute set
and match geometrically within each bucket, so the matcher can never pair two
features a renderer would style differently. The `ocean` layer opts out of id
matching entirely - its ids are synthetic piece indices, stable within a build
and meaningless across two. The residual matcher (`pairing.rs`) is a minimum-cost
maximum-cardinality matching over a sparse K-nearest candidate graph: greedy
pairing crosses over on a cluster that moved together, which turns one
displacement into an added plus a missing feature and reads as structural.

`geometry.rs` is exact integer arithmetic throughout - displacements are compared
against `--tol`, so a float path would make the tolerance verdict depend on
rounding. A displacement is only ever *reported* when the two geometries are the
same shape; a geometry-type, component-count, ring-role or hole-containment
change is `structural_moved` at distance 0, because "moved by N" would be a false
reassurance about a change that is not a movement.

The report is bounded by construction (`report.rs`): differing tiles are a count
plus coalesced ranges, displacements are histograms, and examples are capped per
outcome class by `ExampleSelector`. `--overlay` (`overlay.rs`) renders per-tile
attribution SVGs for the first differing tile ids - pink current, blue comparand,
grey matched-exactly, orange attributes-only - with an attribute-diff panel below
the tile. Grey is keyed to the *class* (`tolerance_moved` at displacement 0, which
only `matched()` produces), not to displacement alone: the topology changes above
are `structural_moved` at distance 0, and keying on displacement drew exactly
those as unchanged backdrop while the report counted them as structural.

One counter difference from the elivagar original: it emitted its `regress_*`
counters onto the sidecar marker FIFO as a child process. brokkr is the process
that *drains* that FIFO, so the same numbers are reported in-band instead - the
`regress ...` line of the text report, and `counters` in `--json`.

**There is deliberately no comparability gate and no baseline registry.**
`regress` is the attribution instrument, and reads no provenance contract by
design: its legitimate uses include cross-contract diffs (adjudicating
artifact-active vs computed output, pricing an intended config change), which
a brokkr-side refusal would block - and there is no raw binary to fall back to
anymore, so a refusal here would simply remove the capability.
Comparability is the caller's responsibility - the help text points at `brokkr
pmtiles-inspect` for reading the provenance blocks and warns that cross-variant
comparisons report six-figure diffs on two correct builds. This replaced the
old `src/elivagar/provenance.rs` comparability gate (and `brokkr bless` / the
`[<host>.datasets.<D>.blessed]` config entry), removed on 2026-07-24 when
elivagar retired the blessed-pmtiles-archive machinery in favour of a
git-committed output corpus. The corpus is the only baseline mechanism now;
see the pmtiles-corpus section below.

## compare-tiles: the lenient census

`brokkr compare-tiles <a> <b> [--sample N]` (`src/elivagar/compare_tiles.rs`) is
the lenient half of the pair. Where `regress` diffs feature by feature and
returns a verdict, this samples `N` tiles per zoom (default 200, at indices
spread across the zoom's whole common run so the sample spans the extent rather
than one corner of the Hilbert curve) and prints per-layer feature counts side
by side. It has no pass/fail, gates nothing, and never refuses.

Two things keep the census honest. The sample indices are `i * len / take`
rather than a `step_by` stride, because an integer stride rounds to 1 whenever
the zoom holds fewer than `2 * N` common tiles and `take` then truncates from
the front - silently reinstating the first-N bias the stride existed to avoid.
And a tile's layers are tallied into per-tile scratch and merged only when
**both** sides decoded, so a one-sided decode failure cannot leave side A's
features in a census side B never contributed to - which is precisely the
half-broken-archive case the tolerant mode is for.

That is why it decodes **tolerant** while regress decodes **strict**: a foreign
producer's extra wire fields are corruption to the gate and unremarkable here,
since the whole point is answering "roughly what changed, and where" for two
archives that may not be comparable at all.

Native since the corpus redesign - it used to build and run elivagar's
`compare_tiles` cargo example, which carried its own hand-rolled PMTiles reader
and MVT scanner. Needs no build and takes no lock. One reporting change came with
that: the example counted raw geometry-command varints (`cmds`), only reachable
by scanning wire bytes directly; decoding through `tile_detail` gives vertex
counts, so the column is now `verts`.

Oracle (`scripts/validate/earcut-oracle.mjs`, a Node script, not a Rust
subcommand) has no brokkr wrapper yet - deferred, since it needs a
Node-subprocess invocation pattern brokkr doesn't have today.

## The pmtiles corpus: `brokkr pmtiles-corpus <sub>`

`brokkr pmtiles-corpus` (`src/elivagar/corpus/` for the gate, `cmd::corpus` for
the dispatch) is the standing baseline mechanism that replaced the blessed
archive. It is **native brokkr code** over the linked elivagar crate - brokkr
owns the digest fold, the gating policy, the verdicts, the SVG render core and
the calibration instrument, and reads archives in-process through the
`eliv.rs` seam; there is no `elivagar corpus` subcommand to shell to anymore.
It is named `pmtiles-corpus`, not `corpus`, because `corpus` is
already piners' parity-corpus runner and brokkr's command names share one flat
clap namespace (the same reason `inspect` became `pmtiles-inspect`).

| brokkr | brokkr resolves |
|---|---|
| `pmtiles-corpus check [--dataset D] [--variant V] [--commit H \| --file P] [--corpus DIR]` | archive, corpus dir |
| `pmtiles-corpus bless [... ] [--corpus DIR] [--rotate] [--mode M]` | archive, corpus dir |
| `pmtiles-corpus render-manifest [...] [--corpus DIR] [--style P]` | archive, corpus dir |
| `pmtiles-corpus render [...] -z Z -x X -y Y [--layers L] [--style P] [-o OUT]` | archive only |
| `pmtiles-corpus rings [...] -o OUT` | archive only |
| `pmtiles-corpus mutate [...] [-o OUT] --op OP [--tile z/x/y]` | input archive only |

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
config/`data/` dir), and is overridable. brokkr owns the value sets now that the
gate is native: `--mode` parses to `DigestMode`, `--op` to `MutationOp`, and an
unknown spelling is a config error before anything opens.

brokkr adds no baseline registry, no default comparand, and no filesystem
inference. There is **no clean-tree gate and no tilegen lock** - a check is
read-only on the archive and never touches tilegen scratch, and bless writes
only into the corpus dir (committed with the landing, so a dirty tree is the
normal state).

### Exit codes

**0** pass, **1** content mismatch, **2** the archive cannot be judged
(non-MVT/non-gzip, absent or invalid embedded contract, contract mismatch),
**3** the baseline is the problem. The distinction is load-bearing for an
automated caller, which is why 3 exists at all: 1 says *the archive regressed*,
and merge damage to a committed `digest` is not that.

Every read of committed material in step 1 therefore goes through
`baseline_material` (`corpus/mod.rs`), which folds `NotFound`/`InvalidData`
into the exit-3 verdict - a missing or malformed `digest`, `leaves` or
`contract.json` all report as baseline trouble. Letting the `io::Error` escape
instead makes the dispatch wrap it as `DevError::Io` and exit **1**, which is
the misreport this closes. The fold is deliberately narrow: a genuine IO
failure (permissions, a bad disk) is not a verdict about anything and still
propagates as an error. Baseline *staleness* (step 4) shares exit 3 with
baseline damage and stays strictly subordinate to the content walk, so it can
never mask a mismatch.
